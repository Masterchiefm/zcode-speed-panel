#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod liveio;
mod metrics;
mod netio;
mod snapshot_guard;
mod updater;

use liveio::{LiveIo, RoundDrift};
use metrics::{home_dir, Engine, ModelStatsPayload, Snapshot};
use updater::Release;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WindowEvent};

/// 窗口显示模式：完整面板 / 悬浮窗
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Full,
    Float,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Float => "float",
        }
    }
    fn parse(s: &str) -> Mode {
        if s.trim() == "float" {
            Mode::Float
        } else {
            Mode::Full
        }
    }
}

/// 悬浮窗样式：迷你仪表盘 / 速度胶囊 / 桌宠
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatStyle {
    Gauge,
    Pill,
    Pet,
}

impl FloatStyle {
    fn as_str(self) -> &'static str {
        match self {
            FloatStyle::Gauge => "gauge",
            FloatStyle::Pill => "pill",
            FloatStyle::Pet => "pet",
        }
    }
    fn parse(s: &str) -> FloatStyle {
        match s.trim() {
            "pill" => FloatStyle::Pill,
            "pet" => FloatStyle::Pet,
            _ => FloatStyle::Gauge,
        }
    }
}

/// 持久化状态：模式、样式与两种模式各自记住的窗口位置/桌宠尺寸。
/// 桌宠位置与完整面板位置互相独立——收起为桌宠时桌宠回到自己上次的位置
/// （无记忆时锚定窗体中心，而不是窗体左上角），展开时窗体回到自己的老位置。
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct Persisted {
    mode: String,
    style: String,
    /// 完整面板上次位置（物理像素）
    #[serde(default)]
    full_pos: Option<(i32, i32)>,
    /// 悬浮窗上次位置（物理像素）
    #[serde(default)]
    float_pos: Option<(i32, i32)>,
    /// 桌宠悬浮窗边长（逻辑像素）
    #[serde(default)]
    pet_size: Option<f64>,
}

struct AppState {
    engine: Mutex<Engine>,
    mode: Mutex<Mode>,
    style: Mutex<FloatStyle>,
    live: Mutex<LiveIo>,
    /// 网络流量监控（netio.rs：整机接口计数 + 连接归属 + 快照上传证据）
    net: Mutex<netio::NetIo>,
    /// 快照防护（snapshot_guard.rs：目录写入锁 mac chflags / win icacls 拒绝
    /// ACE，随 poller 每拍更新）
    guard: Mutex<snapshot_guard::SnapshotGuard>,
    debug: Mutex<DebugLog>,
    persist: Mutex<Persisted>,
    /// 位置落盘节流（拖动期间每 2s 一次，关闭/退出立即落盘）
    last_pos_save: Mutex<Option<std::time::Instant>>,
    /// 托盘菜单顶部的状态项（disabled，仅展示生成状态）
    tray_status: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>>,
    /// 上次写入状态项/托盘 tooltip 的状态文本（变化才更新，避免每拍 churn）
    tray_status_last: Mutex<String>,
    /// mac 启动引导提示是否待领取（一次性）：setup 在事件循环前执行，
    /// 此时 emit 必然早于页面加载被丢弃，改为前端就绪后 invoke 领取
    tray_hint_pending: Mutex<bool>,
    /// macOS 无边框多屏安全最大化记忆：(还原物理坐标, 还原物理尺寸)
    saved_max_rect: Mutex<Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>>,
    /// 当前轮（门控"进行中"连续段）显示速度累计：(Σtps, 实测拍数, 是否见过多进程聚合拍)
    round_tps: Mutex<(f64, u32, bool)>,
    /// 上一拍是否有进行中调用（true→false 沿 = 一轮结束，结算均值喂漂移检测）
    round_was_inflight: Mutex<bool>,
    /// 轮均速漂移检测：上轮均值 vs 之前连续 5 轮均值 ≥3 倍（双向）→ 自动重校准
    drift: Mutex<RoundDrift>,
    /// 上次已落盘的系数样本队列（变化才写 speed-panel-cal.json）
    cal_saved: Mutex<Vec<f64>>,
    /// 桌宠多任务加高的防抖计数（≥2 任务 +1 / <2 任务 -1，3 拍确认）
    pet_task_streak: Mutex<u32>,
    /// 桌宠窗口当前应有的多任务加高（0 或 PET_TASK_EXTRA；与实际窗口尺寸
    /// 的差值由 poller 每拍对比修正，模式/样式切换后也能自动补齐）
    pet_task_extra: Mutex<f64>,
    /// 应用内更新（updater.rs）：最新 Release、预下载产物与并发门旗。
    /// 网络操作全在后台线程；自动检查路径失败一律静默（见 updater.rs 模块注释）
    update: Mutex<UpdateMem>,
}

/// 更新流程的内存态（不落盘：每次启动都检查一次，无需跨启动记忆检查时间）
#[derive(Default)]
struct UpdateMem {
    /// 上次成功查到 Release 的时间（网络失败不记，下个小时仍会重试）
    last_check_ms: i64,
    checking: bool,
    downloading: bool,
    /// 发现的新版本（Some 即有更新）
    latest: Option<Release>,
    /// 预下载完成的安装包 (tag, 路径)
    downloaded: Option<(String, PathBuf)>,
    /// 下载完成即自动启动安装（用户已点过"立即更新"，等下载就位）
    install_when_ready: bool,
}

/// 调试日志：记录实时显示值、统计值与每轮调用完成后的真值，
/// 供"实时读数 vs 落盘统计"的偏差分析。JSONL 追加写，超限轮转保留一代；
/// 轮转出的旧文件超过 7 天在启动时自动清理。
struct DebugLog {
    file: Option<fs::File>,
    written: u64,
    last_heartbeat: std::time::Instant,
}

const DEBUG_LOG_MAX: u64 = 8 * 1024 * 1024;
/// 轮转旧日志的保留时长
const DEBUG_LOG_KEEP: std::time::Duration = std::time::Duration::from_secs(7 * 86400);

impl DebugLog {
    fn new() -> Self {
        let mut log = DebugLog { file: None, written: 0, last_heartbeat: std::time::Instant::now() };
        log.cleanup_rotated();
        log.reopen();
        log
    }

    fn path() -> Option<PathBuf> {
        home_dir().map(|h| h.join(".zcode").join("speed-panel-debug.jsonl"))
    }

    /// 自动清理：删除超过保留期的轮转日志（speed-panel-debug.jsonl.N）
    fn cleanup_rotated(&mut self) {
        let Some(p) = DebugLog::path() else { return };
        let Some(dir) = p.parent() else { return };
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("speed-panel-debug.jsonl.") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if let Ok(mtime) = meta.modified() {
                if mtime < std::time::SystemTime::now() - DEBUG_LOG_KEEP {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    fn reopen(&mut self) {
        if let Some(p) = DebugLog::path() {
            if let Ok(meta) = fs::metadata(&p) {
                self.written = meta.len();
            }
            self.file = fs::OpenOptions::new().create(true).append(true).open(&p).ok();
        }
    }

    fn write(&mut self, value: serde_json::Value) {
        use std::io::Write;
        if self.written > DEBUG_LOG_MAX {
            self.file = None;
            if let Some(p) = DebugLog::path() {
                let _ = fs::rename(&p, p.with_extension("jsonl.1"));
            }
            self.written = 0;
            self.reopen();
        }
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{}", value);
            self.written += value.to_string().len() as u64 + 1;
        }
    }
}

/// 完整面板默认尺寸（逻辑像素）：高度 800 让打开时全部卡片（含底部曲线卡）
/// 免滚动全见（内容自然高 ~760；快照记录列表固定 5 行后网络卡 ~220）
const FULL_SIZE: (f64, f64) = (1000.0, 800.0);
/// 仪表悬浮窗：正方形；148 让实时主环"大一圈"（上轮小环 46px 固定右上角，
/// 主环画布 116px 宽，半径由画布尺寸推出——同步改 style.css #mini-gauge）
const FLOAT_GAUGE_SIZE: (f64, f64) = (148.0, 148.0);
const FLOAT_PILL_SIZE: (f64, f64) = (172.0, 72.0);
/// 桌宠默认边长（逻辑像素），滚轮缩放范围 [100, 480]
const FLOAT_PET_SIZE: f64 = 200.0;
const PET_SIZE_MIN: f64 = 100.0;
const PET_SIZE_MAX: f64 = 480.0;
/// 桌宠窗口顶部气泡预留高度（逻辑像素）：两行气泡最大 ~51px（fs=15 时
/// 10 + 18×2 + 3）+ 余量。窗口 = 边长 ×（边长 + 预留），气泡底边锚在精灵
/// 头顶附近、向上生长，精灵不再为气泡让位缩小（pet.ts 按底部正方形区排版，
/// 改此值须同步两处 set_size 与 pet.ts 排版逻辑）
const PET_BUBBLE_RESERVE: f64 = 56.0;
/// 桌宠多任务加高（逻辑像素）：≥2 个进行中任务（连续 3 拍防抖）时窗口向上
/// 加高这么多给气泡的分任务行让位（底边不动：加高多少上移多少）。96px 在
/// 默认 200 尺寸下可容纳 6 行气泡（实时 + 6 任务 + 上轮）。回落同样防抖
const PET_TASK_EXTRA: f64 = 96.0;

fn mode_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".zcode").join("speed-panel-mode.txt"))
}

/// 系数样本持久化：重启后热启动，不再每次从先验 600 重新收敛（实测高速
/// 会话真值系数 ~160 时，冷启动读数偏低 2~3 倍、收敛需 ~25 分钟）
fn cal_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".zcode").join("speed-panel-cal.json"))
}

/// 恢复有效期：模型/分词器换代后旧样本即过期噪声，超期回先验重新收敛
const CAL_STALE_MS: i64 = 14 * 24 * 3600 * 1000;

fn load_cal_samples() -> Vec<f64> {
    let raw = cal_file().and_then(|p| fs::read_to_string(p).ok());
    let Some(s) = raw else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        eprintln!("[zcode-speed-panel] cal 样本文件损坏，回先验");
        return Vec::new();
    };
    let updated = v.get("updated_ms").and_then(|x| x.as_i64()).unwrap_or(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if now_ms - updated > CAL_STALE_MS {
        eprintln!("[zcode-speed-panel] cal 样本超 14 天过期，回先验");
        return Vec::new();
    }
    v.get("samples")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_f64())
                .collect::<Vec<f64>>()
        })
        .unwrap_or_default()
}

fn save_cal_samples(samples: &[f64]) {
    if let Some(path) = cal_file() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let json = serde_json::json!({ "updated_ms": now_ms, "samples": samples });
        if let Err(e) = fs::write(path, json.to_string()) {
            eprintln!("[zcode-speed-panel] cal 样本落盘失败: {e}");
        }
    }
}

fn load_persisted() -> Persisted {
    let raw = mode_file().and_then(|p| fs::read_to_string(p).ok());
    match raw {
        Some(s) => match serde_json::from_str::<Persisted>(&s) {
            Ok(p) => p,
            // 旧格式：纯文本 "full"/"float"
            Err(_) => Persisted {
                mode: s,
                ..Default::default()
            },
        },
        None => Persisted::default(),
    }
}

fn save_all(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    let style = *state.style.lock().unwrap();
    let p = state.persist.lock().unwrap().clone();
    // 网络当日累计一并落盘（退出/位置保存路径共用）
    state.net.lock().unwrap().save_forced();
    if let Some(path) = mode_file() {
        let json = serde_json::json!({
            "mode": mode.as_str(),
            "style": style.as_str(),
            "full_pos": p.full_pos,
            "float_pos": p.float_pos,
            "pet_size": p.pet_size,
        });
        let _ = fs::write(path, json.to_string());
    }
}

/// 把窗口完整拉回它所在显示器的可见区域（多屏时以窗口当前点定位）
fn clamp_to_screen(window: &tauri::WebviewWindow, x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    let monitor = window
        .monitor_from_point(x as f64, y as f64)
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten())
        .or_else(|| window.primary_monitor().ok().flatten());
    let Some(m) = monitor else {
        return (x, y);
    };
    let mp = m.position();
    let ms = m.size();
    let max_x = (mp.x + ms.width as i32 - w as i32).max(mp.x);
    let max_y = (mp.y + ms.height as i32 - h as i32).max(mp.y);
    (x.clamp(mp.x, max_x), y.clamp(mp.y, max_y))
}


fn apply_mode(window: &tauri::WebviewWindow, mode: Mode, style: FloatStyle, p: &Persisted, pet_extra: f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    match mode {
        Mode::Full => {
            let _ = window.set_min_size(Some(LogicalSize::new(720.0, 520.0)));
            let _ = window.set_size(LogicalSize::new(FULL_SIZE.0, FULL_SIZE.1));
            // 顶栏：mac 恢复原生 Overlay 标题栏——**真·系统交通灯**（红关/黄最小化/
            // 绿全屏原生动画），内容延伸到标题栏下，前端给左侧留位；同时切到
            // Regular 策略亮出 Dock 图标（只有 Regular 应用能原生全屏，见 setup
            // 注释）。Windows 维持无边框 + 前端自绘 — ▢ ✕（悬浮窗必须无边框）
            #[cfg(target_os = "macos")]
            {
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Regular);
                let _ = window.set_decorations(true);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = window.set_decorations(false);
            let _ = window.set_resizable(true);
            let _ = window.set_always_on_top(false);
            let _ = window.set_skip_taskbar(false);
            let _ = window.set_shadow(true);
            // 回到完整面板自己的老位置（无记忆时保持当前左上角，钳回可见区域）
            if let Some((x, y)) = p.full_pos {
                let (px, py) = clamp_to_screen(
                    window,
                    x,
                    y,
                    (FULL_SIZE.0 * scale) as u32,
                    (FULL_SIZE.1 * scale) as u32,
                );
                let _ = window.set_position(PhysicalPosition::new(px, py));
            }
        }
        Mode::Float => {
            let (w, h) = match style {
                FloatStyle::Gauge => FLOAT_GAUGE_SIZE,
                FloatStyle::Pill => FLOAT_PILL_SIZE,
                FloatStyle::Pet => {
                    let s = p.pet_size.unwrap_or(FLOAT_PET_SIZE).clamp(PET_SIZE_MIN, PET_SIZE_MAX);
                    // 顶部预留带给两行气泡：精灵不缩小，气泡向上生长；
                    // 多任务加高（pet_task_extra）让分任务行也有处可长
                    (s, s + PET_BUBBLE_RESERVE + pet_extra)
                }
            };
            let _ = window.set_min_size(None::<LogicalSize<f64>>);
            let _ = window.set_size(LogicalSize::new(w, h));
            let _ = window.set_decorations(false);
            // mac：收回 Accessory——藏 Dock 图标回菜单栏常驻（应用不退出，
            // 与 Regular 亮出 Dock 的完整面板互为两态，见 setup 注释）
            #[cfg(target_os = "macos")]
            let _ = window
                .app_handle()
                .set_activation_policy(tauri::ActivationPolicy::Accessory);
            let _ = window.set_resizable(false);
            // 悬浮窗：置顶、不占任务栏、无原生阴影（阴影会盖住圆角外透明区）
            let _ = window.set_always_on_top(true);
            let _ = window.set_skip_taskbar(true);
            let _ = window.set_shadow(false);
            // 位置：桌宠/悬浮窗自己上次的位置；无记忆时锚定当前窗体中心
            //（而不是跟随左上角——旧版收起后桌宠总落在原窗体左上角的问题）
            let (pw, ph) = ((w * scale) as u32, (h * scale) as u32);
            let target = match p.float_pos {
                Some((x, y)) => clamp_to_screen(window, x, y, pw, ph),
                None => {
                    let cur = window.outer_position().unwrap_or_default();
                    let sz = window.outer_size().unwrap_or_default();
                    let cx = cur.x + sz.width as i32 / 2;
                    let cy = cur.y + sz.height as i32 / 2;
                    clamp_to_screen(window, cx - pw as i32 / 2, cy - ph as i32 / 2, pw, ph)
                }
            };
            let _ = window.set_position(PhysicalPosition::new(target.0, target.1));
        }
    }
}

fn switch_mode(app: &AppHandle, mode: Mode) {
    let state = app.state::<AppState>();
    let style = *state.style.lock().unwrap();
    let prev = *state.mode.lock().unwrap();
    if mode == Mode::Float {
        *state.saved_max_rect.lock().unwrap() = None;
    }
    // 记住旧模式下窗口的位置（两种模式各自独立记忆）
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(pos) = win.outer_position() {
            let mut p = state.persist.lock().unwrap();
            match prev {
                Mode::Full => p.full_pos = Some((pos.x, pos.y)),
                Mode::Float => p.float_pos = Some((pos.x, pos.y)),
            }
        }
    }
    // 先更新模式再应用新尺寸/位置：应用过程触发的 Moved 事件按新模式回写
    *state.mode.lock().unwrap() = mode;
    let p = state.persist.lock().unwrap().clone();
    let pet_extra = *state.pet_task_extra.lock().unwrap();
    if let Some(window) = app.get_webview_window("main") {
        apply_mode(&window, mode, style, &p, pet_extra);
    }
    save_all(app);
    let _ = app.emit("mode", mode.as_str());
}

/// 折叠为悬浮窗：完整面板 → 切换悬浮窗模式；已在悬浮窗 → 唤起并聚焦。
/// CloseRequested / mac 菜单栏 Cmd+Q / ExitRequested 兜底共用
fn collapse_to_float(app: &AppHandle) {
    let mode = *app.state::<AppState>().mode.lock().unwrap();
    if mode == Mode::Full {
        switch_mode(app, Mode::Float);
    } else {
        show_main(app);
    }
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotPayload {
    snapshot: Snapshot,
    rollout_dir: String,
    mode: String,
    float_style: String,
    /// 快照防护状态（每拍附带，前端卡片渲染）
    guard: snapshot_guard::SnapshotGuardStatus,
}

fn build_payload(app: &AppHandle) -> SnapshotPayload {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    let style = *state.style.lock().unwrap();
    let rollout_dir;
    let new_calls;
    let engine_calls: Vec<metrics::Call>;
    let inflight: Vec<(String, i64)>;
    let mut snapshot;
    {
        let mut engine = state.engine.lock().unwrap();
        new_calls = engine.poll();
        snapshot = engine.snapshot();
        rollout_dir = engine.data_source_label();
        engine_calls = engine.calls().to_vec();
        inflight = engine.call_in_flight();
    }
    // 实时实测：进程 IO 写字节流（真实值）。多任务并发（多窗口/子代理）时
    // 按进行中会话的归属进程并集聚合，当前速度 = 真实总吞吐
    let now_ms = snapshot.now_ms;
    // 快照防护（snapshot_guard.rs）：锁定探测 + blocked_rounds 增量累计 +
    // 节流扫描，随 payload 推送前端卡片
    let guard_status = state.guard.lock().unwrap().tick(snapshot.calls_today, now_ms);
    // 网络流量监控：整机接口计数差分 + 连接归属 + checkpoint 工件证据
    let net_now = state.net.lock().unwrap().tick(now_ms);
    let net_log_events: Vec<serde_json::Value> = state.net.lock().unwrap().take_events().into_iter().collect();
    snapshot.net_available = net_now.available;
    snapshot.net_up_bps = net_now.up_bps;
    snapshot.net_down_bps = net_now.down_bps;
    snapshot.net_up_today = net_now.up_today;
    snapshot.net_down_today = net_now.down_today;
    // 会话流量估算（≈）：上传分子用未缓存提示（缓存命中不重发，实测整机
    // 当日上传仅数十 KB），下载按输出 token × SSE 密度系数；非会话上传的
    // 真实下界来自 checkpoint 工件
    let uncached_prompt = snapshot
        .input_tokens
        .saturating_add(snapshot.cache_creation_tokens)
        .saturating_sub(snapshot.cache_read_tokens);
    let (sess_up, sess_down) = netio::sess_bytes_est(
        uncached_prompt,
        snapshot.output_tokens + snapshot.reasoning_tokens,
    );
    snapshot.net_sess_up_today = sess_up;
    snapshot.net_sess_down_today = sess_down;
    snapshot.net_ckpt_today = net_now.ckpt_today_bytes;
    snapshot.net_ckpt_today_count = net_now.ckpt_today_count;
    snapshot.net_ckpt_today_list = net_now.ckpt_today_list.clone();
    snapshot.net_ckpt_uploading = net_now.ckpt_uploading;
    snapshot.net_ckpt_status = net_now.ckpt_status.clone();
    snapshot.net_ckpt_list = net_now.ckpt_list.clone();
    snapshot.net_conns_available = net_now.conns_available;
    snapshot.net_cli_conns = net_now.cli_conns;
    snapshot.net_app_conns = net_now.app_conns;
    snapshot.net_cli_conn_list = net_now.cli_conn_list.clone();
    snapshot.net_app_conn_list = net_now.app_conn_list.clone();
    let cal_event;
    let bpt_now;
    let pipe_bps;
    let npids;
    let proc_bps_log: Vec<(u32, f64)>;
    let infl_attr_log: Vec<(String, u32)>;
    {
        let mut live = state.live.lock().unwrap();
        if !live.history_done() {
            live.ingest_history(engine_calls.as_slice());
        }
        live.observe(&new_calls);
        live.set_inflight(inflight.clone());
        let live_now = live.measure(now_ms);
        cal_event = live.take_calibration();
        bpt_now = live.bytes_per_token();
        pipe_bps = live_now.pipe_bps;
        npids = live_now.n_pids;
        proc_bps_log = live_now.proc_bps.clone();
        infl_attr_log = live.inflight_attr();
        snapshot.tasks = live_now
            .tasks
            .iter()
            .map(|t| metrics::TaskStat {
                pid: t.pid,
                session: t.session.clone().unwrap_or_default(),
                n_sessions: t.n_sessions as u32,
                tps: t.tps,
                streaming: t.streaming,
            })
            .collect();
        // 系数样本队列变化（新样本入样/手动或漂移重校准）即落盘，重启热启动
        {
            let q = live.cal_state();
            let mut saved = state.cal_saved.lock().unwrap();
            if *saved != q {
                save_cal_samples(&q);
                *saved = q;
            }
        }
        let ever_saw = live.ever_saw_procs();
        if live_now.available {
            if live_now.streaming {
                snapshot.is_live = true;
                snapshot.is_estimating = false;
                snapshot.ramping = live_now.ramping;
                snapshot.is_starting = live_now.awaiting;
                snapshot.live_source = "io".into();
                if live_now.awaiting {
                    // 启动期（门控已开、首字节未到）：显示"统计中…"提示，
                    // 不显示误导性的估算值
                    snapshot.current_tps = 0.0;
                } else if live_now.tps < 1.0 && snapshot.window_tps > 0.0 {
                    // 部分调用期间 UI 管道无增量字节（IO 实测为 0）：回退到近期
                    // 已完成调用的真实速度（与速度曲线同口径），标记 ≈ 估算。
                    // ≈ 是落盘口径的全局值，没有可拆的分任务实测，明细清空
                    snapshot.current_tps = snapshot.window_tps;
                    snapshot.is_estimating = true;
                    snapshot.live_source = "window".into();
                    snapshot.tasks.clear();
                } else {
                    snapshot.current_tps = live_now.tps;
                }
                if let Some(last) = snapshot.spark.last_mut() {
                    *last = snapshot.current_tps;
                }
            } else {
                // IO 可用但门控判定无调用 → 如实待机（真实值优先，不用估算掩盖）
                snapshot.is_estimating = false;
                snapshot.ramping = false;
                snapshot.current_tps = 0.0;
                snapshot.live_source = "idle".into();
                if let Some(last) = snapshot.spark.last_mut() {
                    *last = 0.0;
                }
            }
        } else if !inflight.is_empty() && !ever_saw {
            // IO 从未可用（IO 探测环境不可用 / 面板刚启动进程未发现）：
            // 按 message 门控决定，而不是按调用间隔盲估——有调用进行中才显示
            // （近期有真值则估算 ≈，否则"统计中…"提示），门控已停立即归零。
            // 旧口径按间隔中位数推断，调用结束后还会空转"估算中"最长 240s
            if !(snapshot.is_estimating && snapshot.current_tps > 0.0) {
                snapshot.is_estimating = false;
                snapshot.is_starting = true;
                snapshot.current_tps = 0.0;
                snapshot.live_source = "window".into();
                if let Some(last) = snapshot.spark.last_mut() {
                    *last = 0.0;
                }
            }
        } else {
            // 发现过进程但当前不可用（CLI 已全部退出），或门控已停 → 如实待机
            snapshot.is_live = false;
            snapshot.is_estimating = false;
            snapshot.ramping = false;
            snapshot.current_tps = 0.0;
            snapshot.live_source = "idle".into();
            if let Some(last) = snapshot.spark.last_mut() {
                *last = 0.0;
            }
        }

    // ---- 调试日志：实时显示值 / 统计值 / 每轮完成后的真值 ----
    {
        let state = app.state::<AppState>();
        let mut log = state.debug.lock().unwrap();
        for ev in &net_log_events {
            log.write(ev.clone());
        }
        for c in &new_calls {
            log.write(serde_json::json!({
                "kind": "call",
                "t": now_ms,
                "id": c.id,
                "sess": &c.session[c.session.len().saturating_sub(8)..],
                "done": c.completed_ms,
                "gen_ms": c.gen_ms,
                "eff": c.effective_out(),
                "true_tps": (c.effective_out() as f64) / (c.gen_ms.max(50) as f64 / 1000.0),
            }));
        }
        if let Some(cal) = &cal_event {
            // 对账：清洗流按本调用区间积分 ÷ 生成长秒 ÷ 当前系数 = 该调用期间
            // 显示口径的平均 t/s 预测，与落盘真值 true_tps 对比即可评估实时准确性
            let pred_tps = if cal.gen_ms > 0 && cal.bpt_now > 0.0 {
                cal.clean_bytes / (cal.gen_ms as f64 / 1000.0) / cal.bpt_now
            } else {
                0.0
            };
            log.write(serde_json::json!({
                "kind": "cal",
                "t": now_ms,
                "id": cal.id,
                "gen_ms": cal.gen_ms,
                "eff": cal.eff,
                "true_tps": (cal.true_tps * 10.0).round() / 10.0,
                "raw_kb": (cal.raw_bytes / 1024.0 * 10.0).round() / 10.0,
                "clean_kb": (cal.clean_bytes / 1024.0 * 10.0).round() / 10.0,
                "attr_pid": cal.attr_pid,
                "top_pid": cal.top_pid,
                "others_kb": (cal.others_bytes / 1024.0 * 10.0).round() / 10.0,
                "bpt_sample": (cal.bpt_sample * 10.0).round() / 10.0,
                "bpt_now": (cal.bpt_now * 10.0).round() / 10.0,
                "pred_tps": (pred_tps * 10.0).round() / 10.0,
                "skipped": cal.cal_skipped,
            }));
        }
        let active = snapshot.is_live || snapshot.is_estimating || snapshot.is_starting;
        let heartbeat = log.last_heartbeat.elapsed() > std::time::Duration::from_secs(30);
        if active || heartbeat {
            log.last_heartbeat = std::time::Instant::now();
            let tail: Vec<f64> = snapshot
                .spark
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect();
            // 多任务排查三件套：被跟踪进程的探测窗速率 / 进行中会话数 / 会话归属映射
            let pids_json: serde_json::Map<String, serde_json::Value> = proc_bps_log
                .iter()
                .map(|(pid, kbps)| (pid.to_string(), serde_json::json!(kbps)))
                .collect();
            let attr_json: Vec<String> = infl_attr_log
                .iter()
                .map(|(s, pid)| format!("{}…{}", &s[s.len().saturating_sub(4)..], pid))
                .collect();
            log.write(serde_json::json!({
                "kind": "tick",
                "t": now_ms,
                "src": snapshot.live_source,
                "tps": (snapshot.current_tps * 10.0).round() / 10.0,
                "pipe": (pipe_bps / 10.0).round() * 10.0,
                "stream": snapshot.is_live,
                "ramp": snapshot.ramping,
                "start": snapshot.is_starting,
                "est": snapshot.is_estimating,
                "bpt": (bpt_now * 10.0).round() / 10.0,
                "avg": (snapshot.avg_tps * 10.0).round() / 10.0,
                "spark_tail": tail,
                "calls": snapshot.calls_today,
                "npids": npids,
                "infl": inflight.len(),
                "pids": pids_json,
                "attr": attr_json,
                "net_up": (net_now.up_bps / 1024.0 * 10.0).round() / 10.0,
                "net_dn": (net_now.down_bps / 1024.0 * 10.0).round() / 10.0,
                "cli_conn": net_now.cli_conns,
                "app_conn": net_now.app_conns,
                "ckpt_up": net_now.ckpt_uploading,
            }));
        }
    }

    }

    // ---- 轮均速漂移自动重校准：一轮 = 门控"进行中"连续的一段，轮内显示速度
    //      （io 实测拍）取均值；上轮均值 vs 之前连续 5 轮均值 ≥3 倍（双向）
    //      判定量级突变（换模型/分词器，旧系数过期）→ 丢弃系数样本回先验。
    //      多进程聚合轮（多任务并发）不参与：任务数变化带来的吞吐差不是系数漂移 ----
    {
        let state = app.state::<AppState>();
        let now_inflight = !inflight.is_empty();
        let was_inflight = {
            let mut flag = state.round_was_inflight.lock().unwrap();
            std::mem::replace(&mut *flag, now_inflight)
        };
        if snapshot.is_live && snapshot.current_tps > 0.0 {
            let mut acc = state.round_tps.lock().unwrap();
            acc.0 += snapshot.current_tps;
            acc.1 += 1;
            if npids != 1 {
                acc.2 = true;
            }
        }
        if was_inflight && !now_inflight {
            // 一轮结束：结算均值。静默/估算轮（无实测拍）与多进程聚合轮不参与漂移检测
            let (sum, n, saw_multi) = {
                let mut acc = state.round_tps.lock().unwrap();
                std::mem::take(&mut *acc)
            };
            if n > 0 && !saw_multi {
                let avg = sum / n as f64;
                let mut drift = state.drift.lock().unwrap();
                if let Some(base) = drift.observe(avg) {
                    let (bpt_old, bpt_new) = {
                        let mut live = state.live.lock().unwrap();
                        let old = live.bytes_per_token();
                        (old, live.reset_calibration())
                    };
                    state.debug.lock().unwrap().write(serde_json::json!({
                        "kind": "cal_reset",
                        "t": now_ms,
                        "reason": "auto",
                        "round_avg": (avg * 10.0).round() / 10.0,
                        "base_avg": (base * 10.0).round() / 10.0,
                        "bpt_old": (bpt_old * 10.0).round() / 10.0,
                        "bpt_new": (bpt_new * 10.0).round() / 10.0,
                    }));
                    // 与手动触发同款反馈（⟳ 按钮闪 ✓）：自动触发伴随系数大幅
                    // 偏离，用户恰恰需要这个提示
                    let _ = app.emit("recalibrated", ());
                }
            }
        }
    }
    SnapshotPayload {
        rollout_dir,
        snapshot,
        mode: mode.as_str().to_string(),
        float_style: style.as_str().to_string(),
        guard: guard_status,
    }
}

#[tauri::command]
fn snapshot(app: AppHandle) -> SnapshotPayload {
    build_payload(&app)
}

/// 当前 epoch ms（快照防护锁定时刻记录用）
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- 快照防护（snapshot_guard.rs）：状态随 metrics payload 每拍附带，
//      此处三个命令供前端卡片手动查询 / 开启 / 解除（开启与解除的知情
//      同意确认弹窗在前端 #guard-confirm，见 key-rules #16）----

#[tauri::command]
fn snapshot_guard_status(app: AppHandle) -> snapshot_guard::SnapshotGuardStatus {
    let state = app.state::<AppState>();
    let mut guard = state.guard.lock().unwrap();
    let calls = guard.last_calls_seen();
    guard.tick(calls, epoch_ms())
}

/// 开启防护（前端已过确认弹窗，keep_files = 保留现有快照递归锁 / 删除后锁空目录）
#[tauri::command]
fn snapshot_guard_apply(
    app: AppHandle,
    keep_files: Option<bool>,
) -> Result<snapshot_guard::SnapshotGuardStatus, String> {
    let state = app.state::<AppState>();
    // 锁定时刻的 calls_today 基线取实时真值（Engine 只读聚合，一次性开销可接受）
    let calls = state.engine.lock().unwrap().snapshot().calls_today;
    let result = state
        .guard
        .lock()
        .unwrap()
        .apply(calls, epoch_ms(), keep_files.unwrap_or(false));
    result
}

/// 解除防护：递归解锁（文件不动——删除模式目录本就为空，保留模式快照原地恢复可写）
#[tauri::command]
fn snapshot_guard_release(app: AppHandle) -> Result<snapshot_guard::SnapshotGuardStatus, String> {
    let state = app.state::<AppState>();
    let calls = state.engine.lock().unwrap().snapshot().calls_today;
    let result = state.guard.lock().unwrap().release(calls);
    result
}

/// 在系统文件管理器中打开某工作区的快照目录（上传记录行的 📂，跨平台：
/// mac Finder / Windows 资源管理器）。hash 为 checkpoints 下子目录名，
/// 白名单校验防路径穿越；目录不存在（快照已删除/未生成）如实报错
#[tauri::command]
fn open_checkpoint_dir(hash: String) -> Result<(), String> {
    if !snapshot_guard::valid_hash_name(&hash) {
        return Err("非法的工作区目录名".into());
    }
    let dir = snapshot_guard::checkpoints_dir()
        .ok_or("无法定位用户目录")?
        .join(&hash);
    if !dir.is_dir() {
        return Err("该工作区的快照目录不存在（快照可能已被删除或尚未生成）".into());
    }
    #[cfg(target_os = "macos")]
    let st = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let st = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = &dir;
        return Err("仅支持 macOS / Windows".into());
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    st.map(|_| ()).map_err(|e| format!("打开目录失败: {e}"))
}

/// 模型速度趋势：只读查询 usage 库按模型 × 桶聚合（详情弹窗打开期间前端每 5s 拉取）。
/// 聚合在 Engine 内现算完成，零本地存储、不写入 usage 库
#[tauri::command]
fn model_stats(app: AppHandle, window_min: i64) -> ModelStatsPayload {
    let state = app.state::<AppState>();
    let engine = state.engine.lock().unwrap();
    engine.model_stats(window_min)
}

/// 输出速度曲线（时间范围可选 15m/1h/6h/24h）：只读查询 usage 库聚合 90 桶
/// tps（全部模型合并，旧→新）。15 分钟档与 metrics payload 里的今日 spark
/// 数据同口径；前端长档位时每 5s 拉取，并把实时速度混入最新桶
#[tauri::command]
fn chart_stats(app: AppHandle, window_min: i64) -> metrics::ChartStatsPayload {
    let state = app.state::<AppState>();
    let engine = state.engine.lock().unwrap();
    engine.chart_stats(window_min)
}

#[tauri::command]
fn set_mode(app: AppHandle, mode: String, style: Option<String>) {
    if let Some(s) = style {
        let st = FloatStyle::parse(&s);
        *app.state::<AppState>().style.lock().unwrap() = st;
    }
    switch_mode(&app, Mode::parse(&mode));
}

#[tauri::command]
fn set_float_style(app: AppHandle, style: String) {
    let st = FloatStyle::parse(&style);
    {
        let state = app.state::<AppState>();
        *state.style.lock().unwrap() = st;
        let mode = *state.mode.lock().unwrap();
    if mode == Mode::Float {
        if let Some(window) = app.get_webview_window("main") {
            let p = state.persist.lock().unwrap().clone();
            let pet_extra = *state.pet_task_extra.lock().unwrap();
            apply_mode(&window, mode, st, &p, pet_extra);
        }
    }
    }
    save_all(&app);
    let _ = app.emit("float-style", st.as_str());
}

/// 桌宠滚轮缩放：调整悬浮窗边长（逻辑像素）并持久化
#[tauri::command]
fn set_float_size(app: AppHandle, size: f64) {
    let size = size.clamp(PET_SIZE_MIN, PET_SIZE_MAX);
    {
        let state = app.state::<AppState>();
        state.persist.lock().unwrap().pet_size = Some(size);
        let mode = *state.mode.lock().unwrap();
        let style = *state.style.lock().unwrap();
        if mode == Mode::Float && style == FloatStyle::Pet {
            if let Some(window) = app.get_webview_window("main") {
                // 高度含顶部气泡预留带与多任务加高（与 apply_mode 同口径）
                let extra = *state.pet_task_extra.lock().unwrap();
                let _ = window.set_size(LogicalSize::new(size, size + PET_BUBBLE_RESERVE + extra));
            }
        }
    }
    save_all(&app);
}

/// 桌宠多任务加高的防抖与差值应用：≥2 个进行中任务连续 3 拍 → 加高
/// PET_TASK_EXTRA，回落连续 3 拍 → 收回（与完整面板任务卡同款 3 拍防抖）。
/// want 与已应用值一致时直接返回，不 churn 窗口尺寸
fn update_pet_task_extra(app: &AppHandle, multi_now: bool) {
    let state = app.state::<AppState>();
    let streak = {
        let mut s = state.pet_task_streak.lock().unwrap();
        *s = if multi_now {
            (*s + 1).min(3)
        } else {
            s.saturating_sub(1)
        };
        *s
    };
    let want = if streak >= 3 { PET_TASK_EXTRA } else { 0.0 };
    let cur = *state.pet_task_extra.lock().unwrap();
    if (want - cur).abs() < f64::EPSILON {
        return;
    }
    *state.pet_task_extra.lock().unwrap() = want;
    apply_pet_size(app);
}

/// 按当前桌宠边长 + 气泡预留带 + 多任务加高设置窗口尺寸，并按高度差整体
/// 上移/下移保持底边（精灵脚部）在屏幕上不动；仅桌宠悬浮窗模式下生效，
/// 其他模式/样式只更新状态值，切回来时由 apply_mode / 本函数补齐
fn apply_pet_size(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    let style = *state.style.lock().unwrap();
    if mode != Mode::Float || style != FloatStyle::Pet {
        return;
    }
    let extra = *state.pet_task_extra.lock().unwrap();
    let size = state
        .persist
        .lock()
        .unwrap()
        .pet_size
        .unwrap_or(FLOAT_PET_SIZE)
        .clamp(PET_SIZE_MIN, PET_SIZE_MAX);
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let scale = w.scale_factor().unwrap_or(1.0);
    let Ok(outer) = w.outer_size() else {
        return;
    };
    let new_h = size + PET_BUBBLE_RESERVE + extra;
    let dy = ((new_h - outer.height as f64 / scale) * scale).round() as i32;
    let pos = w.outer_position().unwrap_or_default();
    let (nx, ny) = clamp_to_screen(&w, pos.x, pos.y - dy, outer.width, (new_h * scale) as u32);
    let _ = w.set_size(LogicalSize::new(size, new_h));
    let _ = w.set_position(PhysicalPosition::new(nx, ny));
}

/// 悬浮窗右键菜单"退出"：保存状态后退出应用
#[tauri::command]
fn quit_app(app: AppHandle) {
    save_all(&app);
    app.exit(0);
}

/// 手动重新校准（完整面板当前速度卡左上角 ⟳ 按钮）：丢弃已学习的系数样本
/// 回到平台先验，由后续调用重新收敛；漂移检测历史同步复位
#[tauri::command]
fn recalibrate(app: AppHandle) {
    let state = app.state::<AppState>();
    let (bpt_old, bpt_new) = {
        let mut live = state.live.lock().unwrap();
        let old = live.bytes_per_token();
        (old, live.reset_calibration())
    };
    state.drift.lock().unwrap().reset();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    state.debug.lock().unwrap().write(serde_json::json!({
        "kind": "cal_reset",
        "t": now,
        "reason": "manual",
        "bpt_old": (bpt_old * 10.0).round() / 10.0,
        "bpt_new": (bpt_new * 10.0).round() / 10.0,
    }));
    let _ = app.emit("recalibrated", ());
}

/// mac 启动引导提示（一次性）：由前端页面就绪后主动 invoke 领取——
/// setup 内 emit 必然早于页面加载被丢弃，改为前端就绪后 invoke 领取。非 mac 恒返回 false
#[tauri::command]
fn tray_hint_once(app: AppHandle) -> bool {
    let state = app.state::<AppState>();
    let mut guard = state.tray_hint_pending.lock().unwrap();
    let pending = *guard;
    *guard = false;
    pending
}

fn toggle_window_maximize(window: &tauri::WebviewWindow) {
    if window.is_maximized().unwrap_or(false) {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
}

/// 多屏安全最大化/还原：macOS 无边框窗口原生 toggle_maximize 会跳回主屏，
/// 此处按窗口中心点所在显示器铺满（避让菜单栏）；Windows 直接调用系统最大化
#[tauri::command]
fn toggle_maximize_safe(window: tauri::WebviewWindow, state: tauri::State<'_, AppState>) {
    #[cfg(windows)]
    {
        let _ = &state; // state 仅 mac 分支使用，消除 Windows 未用警告
        toggle_window_maximize(&window);
    }
    #[cfg(target_os = "macos")]
    {
        let mut saved = state.saved_max_rect.lock().unwrap();
        if let Some((pos, size)) = saved.take() {
            // 已最大化，执行还原
            let _ = window.set_size(size);
            let _ = window.set_position(pos);
        } else {
            // 未最大化，执行安全最大化
            let cur_pos = window.outer_position().unwrap_or_default();
            let cur_size = window.outer_size().unwrap_or_default();
            *saved = Some((cur_pos, cur_size));

            let cx = cur_pos.x + cur_size.width as i32 / 2;
            let cy = cur_pos.y + cur_size.height as i32 / 2;
            let monitor = window
                .monitor_from_point(cx as f64, cy as f64)
                .ok()
                .flatten()
                .or_else(|| window.current_monitor().ok().flatten())
                .or_else(|| window.primary_monitor().ok().flatten());

            if let Some(m) = monitor {
                let scale = m.scale_factor();
                let mp = m.position();
                let ms = m.size();
                // 避让 macOS 顶部菜单栏高度约 28pt
                let top_margin = (28.0 * scale) as i32;
                let target_x = mp.x;
                let target_y = mp.y + top_margin;
                let target_w = ms.width;
                let target_h = ms.height.saturating_sub(top_margin as u32);

                let _ = window.set_position(PhysicalPosition::new(target_x, target_y));
                let _ = window.set_size(PhysicalSize::new(target_w, target_h));
            } else {
                toggle_window_maximize(&window);
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        toggle_window_maximize(&window);
    }
}

// ---- 应用内更新（updater.rs）：检查 / 预下载 / 安装编排，事件驱动前端卡片 ----

/// 前端 "update" 事件载荷：扁平结构按 state 分支（available / downloading /
/// ready / launching / error），不需要的字段留空
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateEvent {
    state: &'static str,
    current_version: String,
    new_version: String,
    release_url: String,
    notes: String,
    downloaded_bytes: u64,
    total_bytes: u64,
    message: String,
}

/// 手动检查（check_update 命令）的同步返回：前端据此弹轻提示
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "kind")]
enum CheckOutcome {
    UpToDate { current: String },
    Available { current: String, new_version: String },
    Failed { message: String },
}

fn current_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

/// Release 说明截断（按字符计，防超长 body 撑爆前端卡片；前端另有 max-height）
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn update_event(state: &'static str, current: &str, rel: Option<&Release>) -> UpdateEvent {
    UpdateEvent {
        state,
        current_version: current.to_string(),
        new_version: rel.map(|r| r.version.clone()).unwrap_or_default(),
        release_url: rel.map(|r| r.url.clone()).unwrap_or_default(),
        notes: rel.map(|r| truncate_chars(&r.notes, 400)).unwrap_or_default(),
        downloaded_bytes: 0,
        total_bytes: rel.map(|r| r.asset_size).unwrap_or(0),
        message: String::new(),
    }
}

/// 执行一次检查（手动/自动共用）：取到 Release 才记 last_check（网络失败
/// 不记，下个小时重试）；有新版本时 emit + 静默预下载（装时免等）。
/// 失败结果只返回给手动调用方提示，自动路径直接丢弃
fn do_check(app: &AppHandle) -> CheckOutcome {
    let state = app.state::<AppState>();
    {
        let mut u = state.update.lock().unwrap();
        if u.checking {
            return CheckOutcome::Failed { message: "已有检查正在进行".into() };
        }
        u.checking = true;
    }
    let current = current_version(app);
    let outcome = match updater::fetch_latest(&format!("zcode-speed-panel/{current}")) {
        None => CheckOutcome::Failed { message: "网络异常或 Release 信息不可用".into() },
        Some(rel) => {
            {
                let mut u = state.update.lock().unwrap();
                u.last_check_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
            }
            if updater::is_newer(&rel.tag, &current) {
                let ev = update_event("available", &current, Some(&rel));
                state.update.lock().unwrap().latest = Some(rel.clone());
                let _ = app.emit("update", ev);
                let new_version = rel.version.clone();
                spawn_download(app.clone(), rel);
                CheckOutcome::Available { current, new_version }
            } else {
                CheckOutcome::UpToDate { current }
            }
        }
    };
    state.update.lock().unwrap().checking = false;
    outcome
}

/// 后台预下载安装包：进度以 "update" 事件推送（250ms 节流），完成 emit
/// ready；用户已点"立即更新"（install_when_ready）则顺势启动安装。
/// 自动预下载失败完全静默（安装时再试）；等待安装时失败才 emit error
fn spawn_download(app: AppHandle, rel: Release) {
    let current = current_version(&app);
    {
        let state = app.state::<AppState>();
        let mut u = state.update.lock().unwrap();
        if let Some((tag, _)) = &u.downloaded {
            if *tag == rel.tag {
                // 该版本已预下载过（前端刷新后恢复状态也走这里）
                drop(u);
                let _ = app.emit("update", update_event("ready", &current, Some(&rel)));
                return;
            }
        }
        if u.downloading {
            return;
        }
        u.downloading = true;
    }
    std::thread::spawn(move || {
        let mut ev = update_event("downloading", &current, Some(&rel));
        let mut last_emit = std::time::Instant::now();
        let progress_app = app.clone();
        let result = updater::download(&rel, &format!("zcode-speed-panel/{current}"), &mut |done, total| {
            if last_emit.elapsed() >= Duration::from_millis(250) {
                last_emit = std::time::Instant::now();
                ev.downloaded_bytes = done;
                ev.total_bytes = total;
                let _ = progress_app.emit("update", ev.clone());
            }
        });
        match result {
            Ok(path) => {
                let mut ev_ready = update_event("ready", &current, Some(&rel));
                ev_ready.downloaded_bytes = rel.asset_size;
                let launch = {
                    let state = app.state::<AppState>();
                    let mut u = state.update.lock().unwrap();
                    u.downloading = false;
                    u.downloaded = Some((rel.tag.clone(), path));
                    u.install_when_ready
                };
                let _ = app.emit("update", ev_ready);
                if launch {
                    launch_update(&app);
                }
            }
            Err(msg) => {
                let wait = {
                    let state = app.state::<AppState>();
                    let mut u = state.update.lock().unwrap();
                    u.downloading = false;
                    let wait = u.install_when_ready;
                    u.install_when_ready = false; // 失败后等用户再点，不自动重试
                    wait
                };
                if wait {
                    // 用户已在等安装却装不上：如实告知（唯一打扰的场景，
                    // 静默会让"立即更新"按钮看起来失灵）
                    let mut ev = update_event("error", &current, Some(&rel));
                    ev.message = msg;
                    let _ = app.emit("update", ev);
                }
            }
        }
    });
}

/// 启动安装：Windows 运行 NSIS 安装包后退出应用（安装器接管，等 600ms
/// 再退避免安装器撞上尚在退出的进程锁）；macOS 打开 dmg 由用户拖入
/// Applications（应用不退出，旧版本跑到用户重启）
fn launch_update(app: &AppHandle) {
    let state = app.state::<AppState>();
    let rel = state.update.lock().unwrap().latest.clone();
    let downloaded = state.update.lock().unwrap().downloaded.clone();
    let (Some(rel), Some((tag, path))) = (rel, downloaded) else {
        return;
    };
    if tag != rel.tag {
        return; // 陈旧产物不装（latest 变更时预下载会重新拉新包）
    }
    let mut ev = update_event("launching", &current_version(app), Some(&rel));
    #[cfg(target_os = "windows")]
    let msg = "安装程序已启动，应用即将退出…".to_string();
    #[cfg(target_os = "macos")]
    let msg = "已打开安装镜像：请将 zcode-speed-panel 拖入 Applications 覆盖安装".to_string();
    ev.message = msg;
    let _ = app.emit("update", ev);
    if updater::launch_installer(&path).is_err() {
        let mut ev = update_event("error", &current_version(app), Some(&rel));
        ev.message = "启动安装程序失败".into();
        let _ = app.emit("update", ev);
        return;
    }
    #[cfg(target_os = "windows")]
    {
        save_all(app);
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            handle.exit(0);
        });
    }
}

/// 手动检查（footer 右下角版本号点击）：同步返回结果给前端做轻提示；
/// 有更新时卡片由 "update" 事件渲染（本命令只负责结果提示）
#[tauri::command]
fn check_update(app: AppHandle) -> CheckOutcome {
    do_check(&app)
}

/// 前端"立即更新"按钮：已预下载 → 直接启动安装；否则标记待装并确保
/// 下载线程在跑（就绪后自动安装，无需再点一次）
#[tauri::command]
fn install_update(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let rel = state.update.lock().unwrap().latest.clone().ok_or("没有可用更新")?;
    let ready = {
        let u = state.update.lock().unwrap();
        matches!(&u.downloaded, Some((tag, _)) if *tag == rel.tag)
    };
    if ready {
        launch_update(&app);
        return Ok(());
    }
    state.update.lock().unwrap().install_when_ready = true;
    spawn_download(app.clone(), rel); // 已在下载则内部 no-op
    Ok(())
}

/// 当前版本号（footer 右下角显示，来源 tauri.conf.json）
#[tauri::command]
fn app_version(app: AppHandle) -> String {
    current_version(&app)
}

// ---- 自动启动（autostart.rs）：设置弹窗「自动启动」区的读写命令 ----
// 注册表 / LaunchAgent 即事实源，get 回读真实状态（不信任前端缓存）

#[tauri::command]
fn autostart_get() -> String {
    autostart::current_mode().as_str().to_string()
}

/// 返回实际生效的模式（写后回读，失败把错误如实带给前端）
#[tauri::command]
fn autostart_set(mode: String) -> Result<String, String> {
    let parsed = autostart::AutostartMode::parse(&mode);
    autostart::set_mode(parsed)?;
    Ok(autostart::current_mode().as_str().to_string())
}

/// 导出文本文件（快照上传记录等前端生成的报告）：写入
/// `~/.zcode/speed-panel-exports/<file_name>`，返回完整路径供前端提示。
/// 文件名做白名单清洗（只留字母数字._-，防路径注入/穿越）
#[tauri::command]
fn export_text_file(file_name: String, text: String) -> Result<String, String> {
    const MAX_TEXT: usize = 4 * 1024 * 1024;
    if text.len() > MAX_TEXT {
        return Err("内容过大".into());
    }
    let cleaned: String = file_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    if cleaned.is_empty() || cleaned.starts_with('.') {
        return Err("文件名无效".into());
    }
    let Some(home) = home_dir() else {
        return Err("无法定位用户目录".into());
    };
    let dir = home.join(".zcode").join("speed-panel-exports");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建导出目录失败: {e}"))?;
    let path = dir.join(cleaned);
    std::fs::write(&path, text.as_bytes()).map_err(|e| format!("写入失败: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// 用系统默认浏览器打开链接（更新说明页）。WebView 内 <a> 导航行为不可控，
/// 统一由后端代开；仅接受 https，防前端注入 file:// 一类协议
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("仅支持 https 链接".into());
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：cmd 窗口一闪而过的问题
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .creation_flags(0x0800_0000)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开链接失败: {e}"))
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开链接失败: {e}"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = url;
        Err("当前平台不支持".into())
    }
}

/// 后台更新检查线程：启动延迟 8s（避开启动期 SQLite/IO 高峰）先查一次；
/// 之后每小时醒一次，距上次成功检查 ≥24h 才真正发请求（每天一次）。
/// 失败在 fetch_latest 内部吞掉，线程永不打扰用户
fn update_loop(app: AppHandle) {
    std::thread::sleep(Duration::from_secs(8));
    let _ = do_check(&app);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
        let due = {
            let state = app.state::<AppState>();
            let u = state.update.lock().unwrap();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            now - u.last_check_ms >= 24 * 3600 * 1000
        };
        if due {
            let _ = do_check(&app);
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        // mac：窗口从隐藏→显示时提示"应用常驻菜单栏"（无 Dock 图标，用户
        // 关掉窗口后靠提示找回入口）；已可见（如重复启动唤起）不打扰
        #[cfg(target_os = "macos")]
        let was_hidden = !win.is_visible().unwrap_or(true);
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        #[cfg(target_os = "macos")]
        if was_hidden {
            let _ = app.emit("tray-hint", ());
        }
    }
}

fn hide_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
}

fn toggle_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if win.is_visible().unwrap_or(false) && !win.is_minimized().unwrap_or(false) {
            let _ = win.hide();
        } else {
            show_main(app);
        }
    }
}

/// 窗口移动：按当前模式回写位置到内存，节流落盘（拖动每 2s 最多一次）
fn on_window_moved(app: &AppHandle, pos: PhysicalPosition<i32>) {
    let state = app.state::<AppState>();
    let mode = *state.mode.lock().unwrap();
    {
        let mut p = state.persist.lock().unwrap();
        match mode {
            Mode::Full => p.full_pos = Some((pos.x, pos.y)),
            Mode::Float => p.float_pos = Some((pos.x, pos.y)),
        }
    }
    let due = {
        let mut last = state.last_pos_save.lock().unwrap();
        let due = last.map_or(true, |t| t.elapsed() > Duration::from_secs(2));
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    };
    if due {
        save_all(app);
    }
}

/// 托盘状态：菜单顶部状态项文本 + 托盘 tooltip。按快照状态生成
///（生成中/估算中/待机），文本变化才写（避免每 700ms 重复设置）
fn update_tray_status(app: &AppHandle, s: &Snapshot) {
    let state_word = if s.is_live || s.is_starting {
        "生成中"
    } else if s.is_estimating {
        "估算中"
    } else {
        "待机"
    };
    let text = if s.is_live || s.is_starting {
        format!("生成中 {:.1} t/s", s.current_tps)
    } else if s.is_estimating {
        format!("估算中 ≈{:.1} t/s", s.current_tps)
    } else {
        "待机".to_string()
    };
    let state = app.state::<AppState>();
    {
        let mut last = state.tray_status_last.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.clone();
    }
    if let Some(item) = state.tray_status.lock().unwrap().as_ref() {
        let _ = item.set_text(text);
    }
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_tooltip(Some(&format!("ZCode 速度仪表盘 · {state_word}")));
    }
}

/// 后台轮询线程：增量解析 model-io 文件并推送快照
fn poller(app: AppHandle) {
    loop {
        let payload = build_payload(&app);
        update_tray_status(&app, &payload.snapshot);
        // 多任务（≥2 进程）防抖后为桌宠窗口加高/收回分任务行空间
        update_pet_task_extra(&app, payload.snapshot.tasks.len() >= 2);
        let _ = app.emit("metrics", &payload);
        std::thread::sleep(Duration::from_millis(700));
    }
}

fn main() {
    tauri::Builder::default()
        // 重复启动时唤起已有窗口
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .manage(AppState {
            engine: Mutex::new(Engine::new()),
            mode: Mutex::new(Mode::Full),
            style: Mutex::new(FloatStyle::Gauge),
            live: Mutex::new(LiveIo::new()),
            net: Mutex::new(netio::NetIo::new()),
            guard: Mutex::new(snapshot_guard::SnapshotGuard::new()),
            debug: Mutex::new(DebugLog::new()),
            persist: Mutex::new(Persisted::default()),
            last_pos_save: Mutex::new(None),
            tray_status: Mutex::new(None),
            tray_status_last: Mutex::new(String::new()),
            tray_hint_pending: Mutex::new(cfg!(target_os = "macos")),
            saved_max_rect: Mutex::new(None),
            round_tps: Mutex::new((0.0, 0, false)),
            round_was_inflight: Mutex::new(false),
            drift: Mutex::new(RoundDrift::new()),
            cal_saved: Mutex::new(Vec::new()),
            pet_task_streak: Mutex::new(0),
            pet_task_extra: Mutex::new(0.0),
            update: Mutex::new(UpdateMem::default()),
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            model_stats,
            chart_stats,
            set_mode,
            set_float_style,
            set_float_size,
            quit_app,
            toggle_maximize_safe,
            recalibrate,
            tray_hint_once,
            check_update,
            install_update,
            app_version,
            export_text_file,
            open_url,
            autostart_get,
            autostart_set,
            snapshot_guard_status,
            snapshot_guard_apply,
            snapshot_guard_release,
            open_checkpoint_dir
        ])
        .setup(|app| {
            // mac 激活策略**动态切换**（apply_mode 按模式设置，不再固定）：
            // 完整面板 = Regular（有 Dock 图标——macOS 只把 Regular 应用当
            // "正经应用"，绿色交通灯才给原生全屏 Space；Accessory 恒为辅助
            // 全屏：铺满但菜单栏还在，2026-09-19 实测定论）；收起悬浮窗 =
            // Accessory（藏 Dock 回菜单栏常驻，应用不退出）

            // mac：自定义应用菜单拦截 Cmd+Q 为"折叠为悬浮窗"（不注册系统
            // 退出项），并附编辑菜单保住 WebView 的 Cmd+C/V/X/A 快捷键
            #[cfg(target_os = "macos")]
            {
                macos_ui::install(app)?;
                app.on_menu_event(|app, ev| {
                    if ev.id().as_ref() == "collapse-to-float" {
                        save_all(app);
                        collapse_to_float(app);
                    }
                });
            }

            // ---- 系统托盘 ----
            // 顶部状态项（disabled 不可点，poller 每拍按快照刷新文本）
            let status = MenuItem::with_id(app, "status", "待机", false, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let hide = MenuItem::with_id(app, "hide", "隐藏到托盘", true, None::<&str>)?;
            let toggle_float =
                MenuItem::with_id(app, "toggle-float", "悬浮窗 / 完整面板", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[&status, &sep, &show, &hide, &toggle_float, &quit],
            )?;
            app.state::<AppState>().tray_status.lock().unwrap().replace(status);

            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
            TrayIconBuilder::with_id("main-tray")
                .icon(icon)
                .tooltip("ZCode 速度仪表盘")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "show" => show_main(app),
                    "hide" => hide_main(app),
                    "toggle-float" => {
                        let cur = *app.state::<AppState>().mode.lock().unwrap();
                        switch_mode(app, if cur == Mode::Float { Mode::Full } else { Mode::Float });
                    }
                    "quit" => {
                        save_all(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        toggle_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // ---- 关闭按钮 = 收起为悬浮窗；移动时记忆位置（两种模式各自独立） ----
            let win_handle = app.handle().clone();
            app.get_webview_window("main")
                .unwrap()
                .on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        // 点关闭 = 立刻变悬浮窗（不藏托盘；完全退出走托盘/右键菜单）
                        collapse_to_float(&win_handle);
                    }
                    WindowEvent::Moved(pos) => on_window_moved(&win_handle, *pos),
                    _ => {}
                });

            // ---- 恢复上次显示模式、样式与位置后再亮出窗口，避免闪一下完整尺寸 ----
            let persisted = load_persisted();
            let mode = Mode::parse(&persisted.mode);
            let style = FloatStyle::parse(&persisted.style);
            {
                let state = app.state::<AppState>();
                *state.mode.lock().unwrap() = mode;
                *state.style.lock().unwrap() = style;
                *state.persist.lock().unwrap() = persisted;
            }
            // 恢复上次学习的系数样本（无文件/损坏/超 14 天 → 保持先验 600）：
            // dev 热重启或开机后读数立即可用，不再每次冷启动重新收敛
            {
                let state = app.state::<AppState>();
                let samples = load_cal_samples();
                if !samples.is_empty() {
                    let n = state.live.lock().unwrap().restore_cal(samples);
                    eprintln!("[zcode-speed-panel] 校准样本恢复 {n} 个");
                }
                *state.cal_saved.lock().unwrap() = state.live.lock().unwrap().cal_state();
            }
            let window = app.get_webview_window("main").unwrap();
            let p = app.state::<AppState>().persist.lock().unwrap().clone();
            let pet_extra = *app.state::<AppState>().pet_task_extra.lock().unwrap();
            apply_mode(&window, mode, style, &p, pet_extra);
            // 跟随 ZCode 启动（autostart.rs follow 模式，开机带 --zcode-follow）：
            // 静默待命——不亮窗口只留托盘，由检测线程在发现 ZCode 进程后唤起。
            // 单实例插件保证该参数只对"开机第一个实例"生效（已有实例时本进程
            // 到不了 setup，唤起回调直接 show 旧实例窗口）
            let follow_boot = autostart::follow_requested();
            if follow_boot {
                eprintln!("[zcode-speed-panel] 跟随 ZCode 启动：静默待命（托盘常驻）");
                let watch = app.handle().clone();
                std::thread::spawn(move || {
                    // 开机瞬间系统忙，先歇 3s 再开始检测
                    std::thread::sleep(Duration::from_secs(3));
                    loop {
                        if autostart::zcode_running() {
                            eprintln!("[zcode-speed-panel] 检测到 ZCode 进程，亮出面板");
                            show_main(&watch);
                            break;
                        }
                        std::thread::sleep(Duration::from_secs(2));
                    }
                });
            } else {
                let _ = window.show();
            }
            // mac 启动引导提示不在此 emit：setup 早于事件循环/WKWebView 加载，
            // 发即被弃——改为前端就绪后 invoke `tray_hint_once` 领取（一次性）

            // ---- 启动轮询线程 ----
            let poll_handle = app.handle().clone();
            std::thread::spawn(move || poller(poll_handle));

            // ---- 启动更新检查线程（启动+8s 一次、常驻期间每天一次，静默） ----
            let update_handle = app.handle().clone();
            std::thread::spawn(move || update_loop(update_handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building zcode-speed-panel")
        .run(|app, event| match event {
            // 兜底防线：非显式 exit(0) 的退出请求（如 mac 上最后的窗口关闭、
            // 系统注销前的退出）一律阻止并折叠为悬浮窗——真退出只有托盘
            // "退出"与悬浮窗右键"退出程序"两条路（app.exit 时 code=Some，放行）
            tauri::RunEvent::ExitRequested { code: None, api, .. } => {
                api.prevent_exit();
                save_all(app);
                collapse_to_float(app);
            }
            // 真退出前再保存一次（best-effort）
            tauri::RunEvent::Exit => {
                save_all(app);
            }
            _ => {}
        });
}

/// mac 专属 UI：应用菜单栏。Cmd+Q 被拦截为"折叠为悬浮窗"（Accessory 模式下
/// 应用没有 Dock/Cmd+Tab 入口，直接退出会让用户以为应用没了）；菜单中不注册
/// 任何系统退出项，保证退出只走托盘与悬浮窗右键。编辑 submenu 保留
/// Cmd+C/V/X/A，否则 WebView 的文本编辑快捷键会失灵
#[cfg(target_os = "macos")]
mod macos_ui {
    use super::*;
    use tauri::menu::{MenuItem, PredefinedMenuItem, Submenu};

    pub fn install(app: &tauri::App) -> tauri::Result<()> {
        let collapse = MenuItem::with_id(
            app,
            "collapse-to-float",
            "隐藏为悬浮窗",
            true,
            Some("CmdOrCtrl+Q"),
        )?;
        let app_menu = Submenu::with_id_and_items(
            app,
            "app",
            "zcode-speed-panel",
            true,
            &[&collapse],
        )?;
        let edit_menu = Submenu::with_id_and_items(
            app,
            "edit",
            "编辑",
            true,
            &[
                &PredefinedMenuItem::cut(app, None)?,
                &PredefinedMenuItem::copy(app, None)?,
                &PredefinedMenuItem::paste(app, None)?,
                &PredefinedMenuItem::select_all(app, None)?,
            ],
        )?;
        let menu = Menu::with_items(app, &[&app_menu, &edit_menu])?;
        app.set_menu(menu)?;
        Ok(())
    }
}
