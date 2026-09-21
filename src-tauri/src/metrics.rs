use chrono::{Datelike, Local, NaiveTime, Utc};
use rusqlite::OpenFlags;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// 一次已完成的模型调用（来自 ZCode usage 数据库 model_usage 表）
#[derive(Clone, Debug)]
pub struct Call {
    /// model_usage 主键
    #[allow(dead_code)]
    pub id: String,
    pub started_ms: i64,
    #[allow(dead_code)]
    pub first_token_ms: Option<i64>,
    pub completed_ms: i64,
    /// 纯生成时长：completed_at - first_token_at（缺失时退化为 duration_ms）
    pub gen_ms: i64,
    pub output: u64,
    pub reasoning: u64,
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub session: String,
}

impl Call {
    /// 速率分子：输出 token + 思考 token（思考内容同样是流式输出）
    pub fn effective_out(&self) -> u64 {
        self.output + self.reasoning
    }
}

/// 分任务实时明细（多任务并发时才有多个）：一个 CLI 进程 = 一个任务行。
/// 同进程内并行的多个子代理在字节层不可拆分，如实显示为该进程合计
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskStat {
    pub pid: u32,
    /// 归属的进行中会话 id（空 = 尚未归属的流式进程）
    pub session: String,
    /// 该进程承载的进行中会话数（≥2 = 同进程多任务，速度为合计）
    pub n_sessions: u32,
    pub tps: f64,
    /// 该进程当前是否处于流式状态（探测窗速率超阈值）
    pub streaming: bool,
}

/// ZCode 连接明细行（netio 填充）：一条 ESTABLISHED 连接 + 归属进程
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConnStat {
    /// 远端 "ip:port"（v6 为 "[addr]:port"）
    pub remote: String,
    /// 归属进程 pid
    pub pid: u32,
    /// 进程类型标签（"CLI 会话进程" / "主进程" / "渲染进程" / "GPU 进程" /
    /// "工具进程" / "崩溃报告进程"）——两组都是 ZCode 自身进程，按角色区分
    pub proc: String,
}

/// 快照上传记录行（netio 填充）：每个 workspace 的**最近一次**快照实况，
/// 来自 `~/.zcode/v2/checkpoints/*/state.json`。Deserialize 供防护历史
/// 文件（speed-panel-ckpt-history.json）读回；hash = checkpoints 下的
/// 工作区子目录名（点行"打开目录"用；留档旧行没有 → None）
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CkptStat {
    /// 工作区显示名（workspacePath 末段）
    pub workspace: String,
    /// 最近一次压缩加密快照字节数
    pub bytes: u64,
    /// 快照记录时刻（recordedAt，epoch ms；0 = 未知）
    pub recorded_ms: i64,
    /// 最近快照已被服务端接受（lastAcceptedManifestHash 非空）
    pub accepted: bool,
    /// activeUpload 进行中
    pub uploading: bool,
    /// checkpoints 下的工作区子目录名（哈希）；留档旧行可为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// 推送给前端的指标快照
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub current_tps: f64,
    pub avg_tps: f64,
    pub total_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub calls_today: u64,
    pub sessions_today: u64,
    pub is_live: bool,
    /// 调用尚未落盘但按调用间隔推断仍在生成，速度为窗口回退值
    pub is_estimating: bool,
    /// 实测流式已开始但 30s 滑窗未填满（读数来自已活跃区间，前端显示"统计中"）
    pub ramping: bool,
    /// 调用已开始但尚未输出首字节（TTFT/管道未观测到增量）：显示"统计中…"提示
    /// 而不是误导性的估算值，前端表盘/桌宠显示 …
    pub is_starting: bool,
    /// 近 10 分钟已完成调用的真实速度（落盘口径，与速度曲线同源）。
    /// 部分调用期间 UI 管道无增量字节（IO 实测不可用），用它做回退显示
    pub window_tps: f64,
    /// 最近一次已完成调用的真实速度（落盘口径：输出+思考 ÷ 纯生成时长）。
    /// 今日无已完成调用时为 0；当前速度卡右上角的小表用它显示"上一轮"
    pub last_call_tps: f64,
    /// 历史最高单调用速度（t/s）。准入口径见 HistoryStats（有效 first_token、
    /// 纯生成 ≥1s、有效输出 ≥300 token——毫秒级小调用时间戳噪声大，不入记录）。
    /// 无合格调用时为 0；当前速度卡右下角小表"最高"显示
    pub hist_max_tps: f64,
    /// 历史平均速度：全部已完成调用 Σeff ÷ Σgen_s（与今日平均同口径，不过滤）。
    /// 今日平均卡右上角小表"历史"显示
    pub hist_avg_tps: f64,
    /// 当前速度来源："io"=进程流实测 / "window"=窗口回退 / "idle"=待机
    pub live_source: String,
    pub last_activity_ms: i64,
    pub now_ms: i64,
    pub rollout_dir: String,
    pub spark: Vec<f64>,
    /// 并发任务分进程明细（实时链路填充；≥2 个时前端显示任务列表）
    pub tasks: Vec<TaskStat>,
    // ---- 网络流量监控（netio.rs 填充；口径见该模块注释）----
    /// 整机接口计数是否可用（stub 平台 false，前端隐藏网络卡）
    pub net_available: bool,
    /// 整机实时上传/下载速度（B/s，接口计数器约 1s 滑窗实测）
    pub net_up_bps: f64,
    pub net_down_bps: f64,
    /// 整机当日上传/下载累计（真实，跨重启持久化续算）
    pub net_up_today: u64,
    pub net_down_today: u64,
    /// 会话流量估算（≈，token×系数）：当日上传（请求体）/ 下载（流式响应）
    pub net_sess_up_today: u64,
    pub net_sess_down_today: u64,
    /// 当日接受的快照工件字节（非会话上传的真实下界，加密压缩后）
    pub net_ckpt_today: u64,
    pub net_ckpt_today_count: u32,
    /// 当日已接受工件名单（时间/工作区/大小——回答"是哪几个"，跨重启持久化）
    pub net_ckpt_today_list: Vec<CkptStat>,
    /// 是否有快照上传进行中（activeUpload）
    pub net_ckpt_uploading: bool,
    /// checkpoints 目录状态：ok / missing / blocked（ACL 封锁）
    pub net_ckpt_status: String,
    /// 快照上传记录（每工作区最近一次工件的实况列表）
    pub net_ckpt_list: Vec<CkptStat>,
    /// 连接归属是否可用（仅 Windows）
    pub net_conns_available: bool,
    /// 会话组（CLI 进程）/ 桌面端组（其余 zcode.exe）的连接数与明细
    /// （每条含远端 + 归属 pid + 进程类型标签；两组均为 ZCode 自身进程）
    pub net_cli_conns: u32,
    pub net_app_conns: u32,
    pub net_cli_conn_list: Vec<ConnStat>,
    pub net_app_conn_list: Vec<ConnStat>,
}

/// 当前速度统计窗口
const LIVE_WINDOW_MS: i64 = 10 * 60 * 1000;
/// 距离最近一次调用完成超过该时长视为待机，当前速度归零
/// 估算窗口：按今日调用间隔中位数推断“仍在生成”，超出则待机
const ESTIMATE_MIN_MS: i64 = 20 * 1000;
const ESTIMATE_MAX_MS: i64 = 240 * 1000;
const ESTIMATE_DEFAULT_MS: i64 = 60 * 1000;
/// 极短生成时长的下限，避免除零/极端尖峰
const MIN_DUR_MS: i64 = 50;
/// 速度曲线：15 分钟，10 秒一档（对齐墙钟边界，便于前端平滑滚动）
const SPARK_BUCKETS: usize = 90;
const SPARK_BUCKET_MS: i64 = 10_000;

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

pub fn usage_db_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".zcode").join("cli").join("db").join("db.sqlite"))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn local_midnight_utc_ms() -> i64 {
    let now = Local::now();
    let tz = now.timezone();
    now.date_naive()
        .and_time(NaiveTime::MIN)
        .and_local_timezone(tz)
        .single()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
        .unwrap_or_else(|| now.timestamp_millis() - 86_400_000)
}

/// 当日聚合器：持有今日全部调用，计算所有指标（纯函数，便于测试）
pub struct Aggregator {
    pub calls: Vec<Call>,
    pub today_ymd: (i32, u32, u32),
}

/// 历史统计（全时段）：启动时对 usage 库做一次基线扫描（今日之前全部行），
/// 之后随 poll 增量累计，跨天不重置。历史平均与今日平均同口径（不过滤）；
/// 历史最高带准入门槛——实测库中 24ms/79 token 一类小调用的毫秒级时间戳
/// 噪声可产出上千 t/s 的假记录（与校准样本排除 <300 token 调用同理）
#[derive(Clone, Copy, Debug, Default)]
pub struct HistoryStats {
    /// 全部已完成调用 Σeff（历史平均分子）
    pub total_eff: u64,
    /// 全部已完成调用 Σgen_ms（历史平均分母；first_token 缺失退 duration 再 max(50)）
    pub total_gen_ms: i64,
    /// 历史最高单调用速度（t/s）：仅计入有效 first_token、gen≥1s 且 eff≥300 的调用
    pub max_tps: f64,
}

/// 历史最高准入：纯生成时长下限（ms）——短调用时间戳噪声大
const HIST_MAX_MIN_GEN_MS: i64 = 1000;
/// 历史最高准入：有效输出 token 下限（与校准样本准入同值）
const HIST_MAX_MIN_EFF: u64 = 300;

impl HistoryStats {
    /// 累计一次已完成调用（基线扫描与每拍增量共用）。gen_ms 为 poll 口径的
    /// 最终值（ft 有效取 completed-ft，否则 duration 兜底再 max(50)）
    pub fn fold(&mut self, ft: Option<i64>, completed_ms: i64, gen_ms: i64, eff: u64) {
        self.total_eff += eff;
        self.total_gen_ms += gen_ms.max(MIN_DUR_MS);
        // 峰值记录准入：ft 必须真实有效（duration 兜底的行不可信）+ 双下限
        let real_ft = matches!(ft, Some(f) if completed_ms > f);
        if real_ft && gen_ms >= HIST_MAX_MIN_GEN_MS && eff >= HIST_MAX_MIN_EFF {
            let tps = eff as f64 * 1000.0 / gen_ms as f64;
            if tps > self.max_tps {
                self.max_tps = tps;
            }
        }
    }

    pub fn avg_tps(&self) -> f64 {
        if self.total_gen_ms > 0 {
            self.total_eff as f64 / (self.total_gen_ms as f64 / 1000.0)
        } else {
            0.0
        }
    }
}

/// poll 口径的纯生成时长：ft 有效取 completed-ft，否则 duration_ms（>0 才用），
/// 最后 max(50) 兜底。基线扫描与增量摄取共用，保证两路口径一致
fn gen_ms_from(ft: Option<i64>, completed: i64, dur: Option<i64>) -> i64 {
    match ft {
        Some(f) if completed > f => completed - f,
        _ => dur.filter(|d| *d > 0).unwrap_or(MIN_DUR_MS),
    }
    .max(MIN_DUR_MS)
}


impl Aggregator {
    pub fn new() -> Self {
        Self {
            calls: Vec::new(),
            today_ymd: {
                let n = Local::now();
                (n.year(), n.month(), n.day())
            },
        }
    }

    pub fn ingest(&mut self, call: Call) {
        self.calls.push(call);
    }

    /// 跨天清理：清空今日累计
    pub fn rollover_if_needed(&mut self) {
        let n = Local::now();
        let ymd = (n.year(), n.month(), n.day());
        if ymd != self.today_ymd {
            self.today_ymd = ymd;
            self.calls.clear();
        }
    }

    pub fn calls(&self) -> &[Call] {
        &self.calls
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = now_ms();
        let mut out_total = 0u64;
        let mut reason_total = 0u64;
        let mut input_total = 0u64;
        let mut cc_total = 0u64;
        let mut cr_total = 0u64;
        let mut dur_total = 0i64;
        let mut w_out = 0u64;
        let mut w_dur = 0i64;
        let mut last_completed = 0i64;
        // 最近一次已完成调用的分子/分母，用于"上一轮调用速度"角标
        let mut last_eff = 0u64;
        let mut last_gen = 0i64;
        let mut sessions: HashSet<&str> = HashSet::new();
        // 桶对齐墙钟 10s 边界：桶序号 = 完成时刻所属槽 与 当前槽 的差
        let now_slot = now.div_euclid(SPARK_BUCKET_MS);
        let mut buckets = vec![(0u64, 0i64); SPARK_BUCKETS];

        for c in &self.calls {
            out_total += c.output;
            reason_total += c.reasoning;
            input_total += c.input;
            cc_total += c.cache_creation;
            cr_total += c.cache_read;
            dur_total += c.gen_ms.max(MIN_DUR_MS);
            if !c.session.is_empty() {
                sessions.insert(c.session.as_str());
            }
            if c.completed_ms >= last_completed {
                last_completed = c.completed_ms;
                last_eff = c.effective_out();
                last_gen = c.gen_ms.max(MIN_DUR_MS);
            }
            if c.completed_ms >= now - LIVE_WINDOW_MS {
                w_out += c.effective_out();
                w_dur += c.gen_ms.max(MIN_DUR_MS);
            }
            let slot = (now_slot - c.completed_ms.div_euclid(SPARK_BUCKET_MS)) as usize;
            if slot < SPARK_BUCKETS {
                let b = &mut buckets[SPARK_BUCKETS - 1 - slot];
                b.0 += c.effective_out();
                b.1 += c.gen_ms.max(MIN_DUR_MS);
            }
        }

        // 估算窗口：今日相邻完成时刻间隔的中位数（夹在 20s~240s），
        // 用于长思考/长输出期间（调用尚未落盘）继续按最近速度显示回退值
        let mut comps: Vec<i64> = self.calls.iter().map(|c| c.completed_ms).collect();
        comps.sort_unstable();
        comps.dedup();
        let mut gaps: Vec<i64> = comps
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|g| *g > 0 && *g < 600_000)
            .collect();
        let grace_ms = if gaps.len() >= 3 {
            let start = gaps.len().saturating_sub(10);
            let tail = &mut gaps[start..];
            tail.sort_unstable();
            (tail[tail.len() / 2]).clamp(ESTIMATE_MIN_MS, ESTIMATE_MAX_MS)
        } else {
            ESTIMATE_DEFAULT_MS
        };

        let since = now - last_completed;
        // is_live 只由实时 IO 实测决定（main 中覆写）；此处按调用间隔给出窗口回退
        let is_estimating = last_completed > 0 && since <= grace_ms && w_dur > 0;
        let current_tps = if is_estimating && w_dur > 0 {
            w_out as f64 / (w_dur as f64 / 1000.0)
        } else {
            0.0
        };
        let avg_tps = if dur_total > 0 {
            (out_total + reason_total) as f64 / (dur_total as f64 / 1000.0)
        } else {
            0.0
        };
        let mut spark: Vec<f64> = buckets
            .iter()
            .map(|(o, d)| {
                if *d > 0 {
                    *o as f64 / (*d as f64 / 1000.0)
                } else {
                    0.0
                }
            })
            .collect();
        // 估算期把最右一档（当前未落盘的调用）临时填成回退值，完成后被真实数据替换
        if is_estimating {
            if let Some(last) = spark.last_mut() {
                if *last <= 0.0 {
                    *last = current_tps;
                }
            }
        }
        let window_tps = if w_dur > 0 {
            w_out as f64 / (w_dur as f64 / 1000.0)
        } else {
            0.0
        };
        let last_call_tps = if last_gen > 0 {
            last_eff as f64 / (last_gen as f64 / 1000.0)
        } else {
            0.0
        };

        // 总量口径与 ZCode 官方统计一致：input + output + reasoning + cache_creation，
        // 缓存命中（cache_read）是提示复用、不是新增用量，单独展示不计入
        Snapshot {
            current_tps,
            avg_tps,
            total_tokens: out_total + reason_total + input_total + cc_total,
            output_tokens: out_total,
            reasoning_tokens: reason_total,
            input_tokens: input_total,
            cache_creation_tokens: cc_total,
            cache_read_tokens: cr_total,
            calls_today: self.calls.len() as u64,
            sessions_today: sessions.len() as u64,
            is_live: false,
            is_estimating,
            ramping: false,
            is_starting: false,
            window_tps,
            last_call_tps,
            hist_max_tps: 0.0,
            hist_avg_tps: 0.0,
            live_source: if is_estimating {
                "window".to_string()
            } else {
                "idle".to_string()
            },
            last_activity_ms: last_completed,
            now_ms: now,
            rollout_dir: String::new(),
            spark,
            tasks: Vec::new(),
            net_available: false,
            net_up_bps: 0.0,
            net_down_bps: 0.0,
            net_up_today: 0,
            net_down_today: 0,
            net_sess_up_today: 0,
            net_sess_down_today: 0,
            net_ckpt_today: 0,
            net_ckpt_today_count: 0,
            net_ckpt_today_list: Vec::new(),
            net_ckpt_uploading: false,
            net_ckpt_status: String::new(),
            net_ckpt_list: Vec::new(),
            net_conns_available: false,
            net_cli_conns: 0,
            net_app_conns: 0,
            net_cli_conn_list: Vec::new(),
            net_app_conn_list: Vec::new(),
        }
    }
}

/// ZCode usage 数据库（只读 WAL）轮询引擎
pub struct Engine {
    conn: Option<rusqlite::Connection>,
    agg: Aggregator,
    ingested: HashSet<String>,
    /// 历史统计（全时段）：首次 poll 时对今日之前的全部行做一次基线扫描，
    /// 之后随每拍新增调用增量累计
    hist: HistoryStats,
    hist_loaded: bool,
    pub db_path: Option<PathBuf>,
}

impl Engine {
    pub fn new() -> Self {
        let db_path = usage_db_path();
        let conn = db_path.as_ref().and_then(|p| {
            match rusqlite::Connection::open_with_flags(
                p,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                Ok(c) => {
                    // WAL 并发与读性能关键优化：
                    // 1. 设置 busy_timeout 为 3 秒，避免当 ZCode CLI 写事务或 checkpoint 时立即返回 SQLITE_BUSY
                    // 2. 启用 query_only 确保只读
                    let _ = c.busy_timeout(std::time::Duration::from_millis(3000));
                    let _ = c.execute_batch("PRAGMA query_only = ON;");
                    Some(c)
                }
                Err(e) => {
                    eprintln!("[zcode-speed-panel] usage DB open failed: {e}");
                    None
                }
            }
        });
        Self {
            conn,
            agg: Aggregator::new(),
            ingested: HashSet::new(),
            hist: HistoryStats::default(),
            hist_loaded: false,
            db_path,
        }
    }

    pub fn data_source_label(&self) -> String {
        self.db_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(未找到 ~/.zcode/cli/db/db.sqlite)".into())
    }

    /// 轮询 usage 数据库，增量摄取今日完成的调用。返回本轮新增（供实时 IO 模块校准）。
    pub fn poll(&mut self) -> Vec<Call> {
        self.agg.rollover_if_needed();
        let today_start_ms = local_midnight_utc_ms();
        // 跨天时重置已摄取集合
        if self.agg.calls.is_empty() && !self.ingested.is_empty() {
            self.ingested.clear();
        }
        let Some(conn) = &self.conn else {
            return Vec::new();
        };
        // 历史统计基线：首次 poll 扫描今日之前的全部已完成行（本地 SQLite 全表
        // 一次读，实测 ~2 万行毫秒级；今日行由下方增量路径累计，不重复计入）。
        // conn 与 hist 分字段借用，避免整个 self 的可变/不可变借用冲突
        if !self.hist_loaded {
            self.hist_loaded = true;
            Self::scan_history_before(conn, &mut self.hist, today_start_ms);
        }
        let mut new_calls = Vec::new();
        let sql = concat!(
            "SELECT id, started_at, first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens, input_tokens, ",
            "cache_creation_input_tokens, cache_read_input_tokens, session_id ",
            "FROM model_usage WHERE status='completed' AND completed_at >= ?1 ",
            "ORDER BY completed_at ASC"
        );
        let mut stmt = match conn.prepare_cached(sql) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[zcode-speed-panel] usage DB prepare failed: {e}");
                return Vec::new();
            }
        };
        let rows = stmt
            .query_map([today_start_ms], |r| {
            let id: String = r.get(0)?;
            let started: i64 = r.get(1)?;
            let ft: Option<i64> = r.get(2)?;
            let completed: i64 = r.get(3)?;
            let dur: Option<i64> = r.get(4)?;
            // rusqlite 不支持 u64 列读取，按 i64 取再转
            let out: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
            let reason: i64 = r.get::<_, Option<i64>>(6)?.unwrap_or(0);
            let input: i64 = r.get::<_, Option<i64>>(7)?.unwrap_or(0);
            let cc: i64 = r.get::<_, Option<i64>>(8)?.unwrap_or(0);
            let cr: i64 = r.get::<_, Option<i64>>(9)?.unwrap_or(0);
            let session: String = r.get(10)?;
            Ok((
                id,
                started,
                ft,
                completed,
                dur,
                out.max(0) as u64,
                reason.max(0) as u64,
                input.max(0) as u64,
                cc.max(0) as u64,
                cr.max(0) as u64,
                session,
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[zcode-speed-panel] usage DB query failed: {e}");
                return Vec::new();
            }
        };
        for row in rows.flatten() {
            let (id, started, ft, completed, dur, out, reason, input, cc, cr, session) = row;
            if self.ingested.contains(&id) {
                continue;
            }
            self.ingested.insert(id.clone());
            let gen_ms = gen_ms_from(ft, completed, dur);
            // 历史统计增量累计（全时段不过滤；峰值走 HistoryStats::fold 的准入口径）
            let eff = out + reason;
            self.hist.fold(ft, completed, gen_ms, eff);
            self.agg.ingest(Call {
                id,
                started_ms: started,
                first_token_ms: ft,
                completed_ms: completed,
                gen_ms,
                output: out,
                reasoning: reason,
                input,
                cache_creation: cc,
                cache_read: cr,
                session,
            });
            new_calls.push(self.agg.calls.last().unwrap().clone());
        }
        // 今日聚合只保留今日数据（摄取集合在跨天 rollover 时重置）
        self.agg.calls.retain(|c| c.started_ms >= today_start_ms);
        new_calls
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut s = self.agg.snapshot();
        s.rollout_dir = self.data_source_label();
        s.hist_max_tps = self.hist.max_tps;
        s.hist_avg_tps = self.hist.avg_tps();
        s
    }

    /// 历史统计基线扫描：累计 completed_at 早于今日零点的全部已完成行。
    /// 失败只打日志不 panic（历史角标显示 0，今日增量路径照常）
    fn scan_history_before(
        conn: &rusqlite::Connection,
        hist: &mut HistoryStats,
        today_start_ms: i64,
    ) {
        let sql = concat!(
            "SELECT first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens FROM model_usage ",
            "WHERE status='completed' AND completed_at < ?1"
        );
        let mut query = || -> rusqlite::Result<()> {
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map([today_start_ms], |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u64,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                ))
            })?;
            for row in rows.flatten() {
                let (ft, completed, dur, out, reason) = row;
                let gen = gen_ms_from(ft, completed, dur);
                hist.fold(ft, completed, gen, out + reason);
            }
            Ok(())
        };
        if let Err(e) = query() {
            eprintln!("[zcode-speed-panel] 历史基线扫描失败: {e}");
        }
    }

    /// 今日已摄取的全部调用（供实时模块确定当前会话）
    pub fn calls(&self) -> &[Call] {
        &self.agg.calls()
    }

    /// 是否有调用正在进行：看最近活跃会话的最新 assistant 消息行。
    /// message 行在调用开始瞬间即提交（≤200ms 可读），行内 data 的 time 对象在
    /// 调用结束（含取消/出错）时补写 completed 字段——比 model_usage 完成行更快、
    /// 且覆盖 status='cancelled'/'error'（这两种调用永远没有 completed 状态行，
    /// 旧口径下会卡"生成中"直到 10 分钟兜底）。
    /// 返回全部进行中的 (会话, 调用开始时刻)，按开始时刻降序——多任务并发
    /// （多窗口 / 子代理会话）时实时速度按进程集合聚合，不再单选最新一条。
    /// 10 分钟上限兜底崩溃后无人补写 completed 的行。
    pub fn call_in_flight(&self) -> Vec<(String, i64)> {
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        // 最近活跃会话（session 表 ~1k 行，按 time_updated 倒序小表扫描可接受；
        // 上限 16：多任务聚合要覆盖全部进行中会话，>6 个并发子代理不能漏计）；
        // message 表缺 time_created 单列索引，不能全局 ORDER BY（实测 ~200ms/次）
        let mut stmt = match conn.prepare_cached(
            "SELECT id FROM session ORDER BY time_updated DESC LIMIT 16",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let sessions: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => return Vec::new(),
        };
        drop(stmt);

        let mut cands: Vec<(String, i64, bool)> = Vec::new();
        for sess in &sessions {
            // 每会话只看最新一条 assistant 行（走 (session_id, time_created) 复合索引）
            let Ok(mut stmt) = conn.prepare_cached(
                "SELECT time_created, substr(data,1,120) FROM message \
                 WHERE session_id = ?1 ORDER BY time_created DESC LIMIT 8",
            ) else {
                continue;
            };
            let Ok(rows) = stmt.query_map([sess], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) else {
                continue;
            };
            for (created, prefix) in rows.flatten() {
                if !prefix.contains("\"assistant\"") {
                    continue;
                }
                let done = prefix.contains("\"completed\"");
                cands.push((sess.clone(), created, done));
                break;
            }
        }
        inflight_from_rows(&cands, Utc::now().timestamp_millis())
    }

    /// 模型速度趋势：只读查询窗口内已完成的 model_usage 行，按模型 × 时间桶聚合。
    /// 仅查询现算（零本地存储、不给 usage 库建索引/写入）；conn 缺失或查询失败
    /// 返回空 payload（不 panic）。聚合口径见 aggregate_model_stats
    pub fn model_stats(&self, window_min: i64) -> ModelStatsPayload {
        let window_min = clamp_window_min(window_min);
        let now = now_ms();
        let Some(conn) = &self.conn else {
            eprintln!("[zcode-speed-panel] model_stats: usage DB 不可用");
            return empty_model_stats(window_min, now);
        };
        // 注：model_usage 表的模型列实名是 model_id（PRAGMA table_info 核实，无 model 列）
        let sql = concat!(
            "SELECT model_id, first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens FROM model_usage ",
            "WHERE status='completed' AND completed_at >= ?1 ORDER BY completed_at ASC"
        );
        let cutoff = now - window_min * 60_000;
        let query = || -> rusqlite::Result<Vec<ModelUsageRow>> {
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map([cutoff], |r| {
                let model: String = r.get(0)?;
                let ft: Option<i64> = r.get(1)?;
                let completed: i64 = r.get(2)?;
                let dur: Option<i64> = r.get(3)?;
                // rusqlite 不支持 u64 列读取，按 i64 取再转（与 poll 同口径）
                let out: i64 = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                let reason: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
                Ok(ModelUsageRow {
                    model,
                    first_token_at: ft,
                    completed_at: completed,
                    duration_ms: dur,
                    output_tokens: out.max(0) as u64,
                    reasoning_tokens: reason.max(0) as u64,
                })
            })?;
            Ok(rows.flatten().collect())
        };
        match query() {
            Ok(rows) => aggregate_model_stats(rows, window_min, now),
            Err(e) => {
                eprintln!("[zcode-speed-panel] model_stats query failed: {e}");
                empty_model_stats(window_min, now)
            }
        }
    }
}

/// message 门控纯判定：候选 (会话, assistant 行创建时刻, 是否已带 completed)。
/// 每会话只认最新一条 assistant 行（更老的未完成行是崩溃残留，已被更新行覆盖），
/// 其中最新行未完成且新鲜的会话**全部**视为进行中（多任务并发各自计入，
/// 供实时链路按进程集合聚合），按创建时刻降序返回
pub(crate) fn inflight_from_rows(
    cands: &[(String, i64, bool)],
    now_ms: i64,
) -> Vec<(String, i64)> {
    let mut newest: HashMap<&str, &(String, i64, bool)> = HashMap::new();
    for row in cands {
        match newest.get(row.0.as_str()) {
            Some(prev) if prev.1 >= row.1 => {}
            _ => {
                newest.insert(row.0.as_str(), row);
            }
        }
    }
    let mut out: Vec<(String, i64)> = newest
        .values()
        .filter(|(_, created, done)| !done && now_ms - *created <= 600_000)
        .map(|(s, c, _)| (s.clone(), *c))
        .collect();
    out.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    out
}

// ============ 模型速度趋势（按模型 × 时间桶聚合，详情弹窗用，零本地存储） ============

/// 合法统计窗口（分钟）：最近 10 分钟 / 1 小时 / 6 小时
const MODEL_WINDOW_CHOICES: [i64; 3] = [10, 60, 360];
/// 趋势图统一 60 桶（0 = 最新桶）
const MODEL_BUCKETS: usize = 60;

/// 单模型单桶聚合
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelBucket {
    /// 该桶 tps = Σ(output+reasoning) ÷ Σ纯生成秒；无调用为 0
    pub tps: f64,
    pub calls: u64,
    pub tokens: u64,
}

/// 单模型一条趋势线 + 窗口汇总
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ModelSeries {
    pub model: String,
    /// 恒 60 个，0 = 最新桶
    pub buckets: Vec<ModelBucket>,
    pub total_calls: u64,
    pub total_tokens: u64,
    /// 全窗口 Σeff ÷ Σgen_s
    pub avg_tps: f64,
    /// 各桶 tps 的最大值
    pub peak_tps: f64,
    /// 该模型 eff 占全部模型 eff 比例（0~1）
    pub share: f64,
}

/// model_stats 命令的返回载荷
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatsPayload {
    pub window_min: i64,
    pub bucket_ms: i64,
    pub now_ms: i64,
    /// 按 total_tokens 降序
    pub series: Vec<ModelSeries>,
}

/// model_usage 查询行（聚合纯函数的输入）
struct ModelUsageRow {
    model: String,
    first_token_at: Option<i64>,
    completed_at: i64,
    duration_ms: Option<i64>,
    output_tokens: u64,
    reasoning_tokens: u64,
}

/// 每模型每桶的原始累计：(有效输出 token, 生成毫秒, 调用数)
struct BucketAcc {
    eff: u64,
    gen_ms: i64,
    calls: u64,
}

/// 非法窗口值归入最近的合法档位（10 / 60 / 360 分钟）
fn clamp_window_min(window_min: i64) -> i64 {
    MODEL_WINDOW_CHOICES
        .iter()
        .copied()
        .min_by_key(|&w| (w - window_min).abs())
        .unwrap_or(60)
}

/// 空 payload（conn 缺失 / 查询失败时返回，不 panic）
fn empty_model_stats(window_min: i64, now_ms: i64) -> ModelStatsPayload {
    ModelStatsPayload {
        window_min,
        bucket_ms: window_min * 60_000 / MODEL_BUCKETS as i64,
        now_ms,
        series: Vec::new(),
    }
}

/// 纯函数：把窗口内已完成的调用行聚合成按模型 60 桶的趋势与汇总（便于单测）。
/// - 桶序号 = (now − completed) ÷ bucket_ms（div_euclid，未来时刻为负直接丢弃），
///   0 = 最新桶、59 = 最旧桶，越界丢弃；
/// - gen_ms = completed − first_token，first_token 缺失或非正退 duration_ms，
///   再 max(50) 兜底（与今日聚合同口径）；
/// - eff = output + reasoning；桶 tps = Σeff ÷ Σgen_s，无调用为 0；
/// - series 按 total_tokens 降序。
fn aggregate_model_stats(
    rows: Vec<ModelUsageRow>,
    window_min: i64,
    now_ms: i64,
) -> ModelStatsPayload {
    let window_min = clamp_window_min(window_min);
    let bucket_ms = window_min * 60_000 / MODEL_BUCKETS as i64;
    // model -> (每桶累计, 总eff, 总gen_ms, 总调用数)
    let mut per_model: HashMap<String, (Vec<BucketAcc>, u64, i64, u64)> = HashMap::new();
    for r in rows {
        let gen = match r.first_token_at {
            Some(f) if r.completed_at > f => r.completed_at - f,
            _ => r.duration_ms.filter(|d| *d > 0).unwrap_or(MIN_DUR_MS),
        }
        .max(MIN_DUR_MS);
        let eff = r.output_tokens + r.reasoning_tokens;
        let slot = (now_ms - r.completed_at).div_euclid(bucket_ms);
        if slot < 0 || slot as usize >= MODEL_BUCKETS {
            continue;
        }
        let entry = per_model.entry(r.model).or_insert_with(|| {
            (
                (0..MODEL_BUCKETS)
                    .map(|_| BucketAcc { eff: 0, gen_ms: 0, calls: 0 })
                    .collect(),
                0,
                0,
                0,
            )
        });
        let b = &mut entry.0[slot as usize];
        b.eff += eff;
        b.gen_ms += gen;
        b.calls += 1;
        entry.1 += eff;
        entry.2 += gen;
        entry.3 += 1;
    }

    let grand_eff: u64 = per_model.values().map(|e| e.1).sum();
    let mut series: Vec<ModelSeries> = per_model
        .into_iter()
        .map(|(model, (buckets, total_eff, total_gen, total_calls))| {
            let mut peak = 0.0f64;
            let buckets: Vec<ModelBucket> = buckets
                .into_iter()
                .map(|b| {
                    let tps = if b.gen_ms > 0 {
                        b.eff as f64 / (b.gen_ms as f64 / 1000.0)
                    } else {
                        0.0
                    };
                    if tps > peak {
                        peak = tps;
                    }
                    ModelBucket { tps, calls: b.calls, tokens: b.eff }
                })
                .collect();
            ModelSeries {
                model,
                buckets,
                total_calls,
                total_tokens: total_eff,
                avg_tps: if total_gen > 0 {
                    total_eff as f64 / (total_gen as f64 / 1000.0)
                } else {
                    0.0
                },
                peak_tps: peak,
                share: if grand_eff > 0 {
                    total_eff as f64 / grand_eff as f64
                } else {
                    0.0
                },
            }
        })
        .collect();
    series.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens).then(a.model.cmp(&b.model)));
    ModelStatsPayload { window_min, bucket_ms, now_ms, series }
}

// ============ 输出速度曲线（时间范围可选，chart 卡用，零本地存储） ============

/// 曲线合法时间范围（分钟）：15 分钟 / 1 小时 / 6 小时 / 24 小时
const CHART_WINDOW_CHOICES: [i64; 4] = [15, 60, 360, 1440];
/// 曲线统一 90 桶（与今日 spark 同密度）：15m→10s、1h→40s、6h→4min、24h→16min
const CHART_BUCKETS: usize = 90;

/// chart_stats 命令的返回载荷：单序列（全部模型合并）按时间桶的 tps
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChartStatsPayload {
    pub window_min: i64,
    pub bucket_ms: i64,
    pub now_ms: i64,
    /// 恒 90 个，旧→新排列（0 = 最旧桶，末位 = 最新桶）
    pub buckets: Vec<f64>,
}

/// 非法窗口值归入最近的合法档位（15 / 60 / 360 / 1440 分钟）
fn clamp_chart_window(window_min: i64) -> i64 {
    CHART_WINDOW_CHOICES
        .iter()
        .copied()
        .min_by_key(|&w| (w - window_min).abs())
        .unwrap_or(15)
}

/// 纯函数：把窗口内已完成的调用行聚合成 90 桶 tps（口径与今日 spark 一致：
/// gen = poll 口径兜底链，桶 tps = Σeff ÷ Σgen_s）。便于单测
fn aggregate_chart_stats(
    rows: Vec<ModelUsageRow>,
    window_min: i64,
    now_ms: i64,
) -> ChartStatsPayload {
    let window_min = clamp_chart_window(window_min);
    let bucket_ms = window_min * 60_000 / CHART_BUCKETS as i64;
    let mut acc = vec![(0u64, 0i64); CHART_BUCKETS]; // (Σeff, Σgen_ms)
    for r in rows {
        let gen = gen_ms_from(r.first_token_at, r.completed_at, r.duration_ms);
        let slot = now_ms.div_euclid(bucket_ms) - r.completed_at.div_euclid(bucket_ms);
        if slot < 0 || slot as usize >= CHART_BUCKETS {
            continue;
        }
        let b = &mut acc[CHART_BUCKETS - 1 - slot as usize];
        b.0 += r.output_tokens + r.reasoning_tokens;
        b.1 += gen;
    }
    ChartStatsPayload {
        window_min,
        bucket_ms,
        now_ms,
        buckets: acc
            .into_iter()
            .map(|(eff, gen)| {
                if gen > 0 {
                    eff as f64 / (gen as f64 / 1000.0)
                } else {
                    0.0
                }
            })
            .collect(),
    }
}

impl Engine {
    /// 输出速度曲线：只读查询窗口内已完成的 model_usage 行，聚合成 90 桶 tps
    /// （全部模型合并单序列，旧→新）。conn 缺失或查询失败返回全 0 payload
    pub fn chart_stats(&self, window_min: i64) -> ChartStatsPayload {
        let window_min = clamp_chart_window(window_min);
        let now = now_ms();
        let empty = ChartStatsPayload {
            window_min,
            bucket_ms: window_min * 60_000 / CHART_BUCKETS as i64,
            now_ms: now,
            buckets: vec![0.0; CHART_BUCKETS],
        };
        let Some(conn) = &self.conn else {
            return empty;
        };
        // 与 model_stats 同一条查询（多取一列 model_id，聚合时忽略——保持
        // SQL 与行结构一致，便于复用 prepare_cached 的口径）
        let sql = concat!(
            "SELECT model_id, first_token_at, completed_at, duration_ms, ",
            "output_tokens, reasoning_tokens FROM model_usage ",
            "WHERE status='completed' AND completed_at >= ?1 ORDER BY completed_at ASC"
        );
        let cutoff = now - window_min * 60_000;
        let query = || -> rusqlite::Result<Vec<ModelUsageRow>> {
            let mut stmt = conn.prepare_cached(sql)?;
            let rows = stmt.query_map([cutoff], |r| {
                let model: String = r.get(0)?;
                let ft: Option<i64> = r.get(1)?;
                let completed: i64 = r.get(2)?;
                let dur: Option<i64> = r.get(3)?;
                let out: i64 = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                let reason: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(0);
                Ok(ModelUsageRow {
                    model,
                    first_token_at: ft,
                    completed_at: completed,
                    duration_ms: dur,
                    output_tokens: out.max(0) as u64,
                    reasoning_tokens: reason.max(0) as u64,
                })
            })?;
            Ok(rows.flatten().collect())
        };
        match query() {
            Ok(rows) => aggregate_chart_stats(rows, window_min, now),
            Err(e) => {
                eprintln!("[zcode-speed-panel] chart_stats query failed: {e}");
                empty
            }
        }
    }
}

// ============ 测试 ============
#[cfg(test)]
mod tests {
    use super::*;

    /// message 门控：未带 completed 的最新 assistant 行 → 进行中；
    /// 已完成的行、超龄的僵尸行（崩溃兜底）、以及"同会话更新的已完成行"都不算；
    /// 多会话并发（多窗口/子代理）全部返回
    #[test]
    fn inflight_from_rows_gating() {
        let now = 1_000_000i64;
        // 新会话首条调用：行未完成 → 进行中
        let r = inflight_from_rows(&[("new".into(), now - 3_000, false)], now);
        assert_eq!(r, vec![("new".to_string(), now - 3_000)]);
        // 同会话有更新的已完成 assistant 行（旧僵尸行在上）→ 不算
        assert_eq!(
            inflight_from_rows(
                &[
                    ("a".into(), now - 60_000, false),      // 崩溃残留
                    ("a".into(), now - 30_000, true),       // 会话 a 最新 assistant 行
                ],
                now
            ),
            Vec::new()
        );
        // 多会话并发：全部进行中会话都返回，按开始时刻降序
        // （子 agent 会话 b/c 晚于主会话 a 开始，a 的当轮已完成）
        let r = inflight_from_rows(
            &[
                ("a".into(), now - 40_000, true),
                ("b".into(), now - 5_000, false),
                ("c".into(), now - 20_000, false),
            ],
            now,
        );
        assert_eq!(
            r,
            vec![
                ("b".to_string(), now - 5_000),
                ("c".to_string(), now - 20_000),
            ]
        );
        // 未完成但超过 10 分钟兜底 → 判停
        assert_eq!(
            inflight_from_rows(&[("z".into(), now - 601_000, false)], now),
            Vec::new()
        );
        assert_eq!(inflight_from_rows(&[], now), Vec::new());
    }

    fn call(completed: i64, gen_ms: i64, out: u64, reason: u64, input: u64, session: &str) -> Call {
        Call {
            id: format!("{}-{}", completed, out),
            started_ms: completed - gen_ms - 1000,
            first_token_ms: Some(completed - gen_ms),
            completed_ms: completed,
            gen_ms,
            output: out,
            reasoning: reason,
            input,
            cache_creation: 0,
            cache_read: 0,
            session: session.into(),
        }
    }

    #[test]
    fn snapshot_computes_speeds() {
        let now = now_ms();
        let mut agg = Aggregator::new();
        agg.ingest(call(now - 10_000, 10_000, 500, 40, 100, "a"));
        agg.ingest(call(now - 1_000, 8_000, 240, 60, 100, "a"));
        let s = agg.snapshot();
        // 纯生成速率：(500+40 + 240+60) / 18s = 46.7
        assert!((s.avg_tps - 840.0 / 18.0).abs() < 1e-9);
        assert!((s.current_tps - 840.0 / 18.0).abs() < 1e-9);
        assert!(!s.is_live); // is_live 仅由实时 IO 实测在 main 中覆写
        // 总 token = 输出 740 + 思考 100 + 输入 200
        assert_eq!(s.total_tokens, 1040);
        assert_eq!(s.output_tokens, 740);
        assert_eq!(s.reasoning_tokens, 100);
        assert_eq!(s.sessions_today, 1);
        assert_eq!(s.spark.len(), SPARK_BUCKETS);
        assert_eq!(s.live_source, "window"); // 无 IO 探测时 is_live=false → 窗口回退
        // 上一轮调用速度 = 最近一次完成调用（now-1s）的 eff/gen = 300 / 8s
        assert!((s.last_call_tps - 37.5).abs() < 1e-9);
    }

    /// "上一轮调用速度"取完成时刻最晚的那条，与 ingest 顺序无关（DB 查询排序可能变化）
    #[test]
    fn last_call_tps_uses_latest_completed() {
        let now = now_ms();
        let mut agg = Aggregator::new();
        agg.ingest(call(now - 1_000, 4_000, 400, 0, 0, "a")); // 100 t/s
        agg.ingest(call(now - 30_000, 2_000, 100, 0, 0, "b")); // 50 t/s，更早完成
        agg.ingest(call(now - 20_000, 5_000, 250, 50, 0, "c")); // 60 t/s，仍早于 now-1s
        let s = agg.snapshot();
        assert!((s.last_call_tps - 100.0).abs() < 1e-9);
        // 平均速度与"上一轮"是两个口径：总 eff 800 / 总 11s ≠ 100
        assert!((s.avg_tps - 800.0 / 11.0).abs() < 1e-9);
    }

    #[test]
    fn speed_excludes_time_before_first_token() {
        let now = now_ms();
        let mut agg = Aggregator::new();
        agg.ingest(call(now - 5_000, 5_000, 500, 0, 0, "a"));
        let s1 = agg.snapshot();
        assert!((s1.avg_tps - 100.0).abs() < 1e-9);
        // 调用 B：30s 前就发出请求（长 TTFT/排队），但纯生成为 5s、输出 500
        agg.ingest(Call {
            id: "b".into(),
            started_ms: now - 30_000,
            first_token_ms: Some(now - 6_000),
            completed_ms: now - 1_000,
            gen_ms: 5_000,
            output: 500,
            reasoning: 0,
            input: 0,
            cache_creation: 0,
            cache_read: 0,
            session: "b".into(),
        });
        let s2 = agg.snapshot();
        // 分母用 completed - first_token（不含首 token 前的等待），仍是 100 t/s
        assert!((s2.avg_tps - 100.0).abs() < 1e-9);
    }

    fn mrow(
        model: &str,
        completed: i64,
        ft: Option<i64>,
        dur: Option<i64>,
        out: u64,
        reason: u64,
    ) -> ModelUsageRow {
        ModelUsageRow {
            model: model.into(),
            first_token_at: ft,
            completed_at: completed,
            duration_ms: dur,
            output_tokens: out,
            reasoning_tokens: reason,
        }
    }

    /// 两模型 × 两桶：tps/calls/tokens/avg/peak/share 正确、桶对齐、空桶为 0、
    /// series 按 total_tokens 降序、窗口外（过早/未来）行丢弃
    #[test]
    fn model_stats_two_models_two_buckets() {
        let now = 1_700_000_000_000i64;
        let rows = vec![
            // 模型 A：桶 0（gen 4s，eff 400 → 100 t/s）、桶 1（gen 1s，eff 100 → 100 t/s）
            mrow("model-a", now - 5_000, Some(now - 9_000), Some(9_000), 300, 100),
            mrow("model-a", now - 15_000, Some(now - 16_000), Some(6_000), 100, 0),
            // 模型 B：桶 0（首 token 缺失退 duration 2s，eff 400 → 200 t/s）、
            // 桶 2（gen 3s，eff 1200 → 400 t/s）
            mrow("model-b", now - 5_000, None, Some(2_000), 400, 0),
            mrow("model-b", now - 25_000, Some(now - 28_000), None, 900, 300),
            // 越界：早于窗口（桶 70）与未来时刻（div_euclid 为负），都应丢弃
            mrow("model-a", now - 700_000, Some(now - 701_000), None, 999, 0),
            mrow("model-a", now + 1_000, Some(now), None, 999, 0),
        ];
        let p = aggregate_model_stats(rows, 10, now);
        assert_eq!(p.window_min, 10);
        assert_eq!(p.bucket_ms, 10_000);
        assert_eq!(p.now_ms, now);
        // 按 total_tokens 降序：B(1600) 在前，A(500) 在后
        assert_eq!(p.series.len(), 2);
        assert_eq!(p.series[0].model, "model-b");
        assert_eq!(p.series[1].model, "model-a");

        let b = &p.series[0];
        assert_eq!(b.buckets.len(), 60);
        assert_eq!(b.total_calls, 2);
        assert_eq!(b.total_tokens, 1600);
        assert!((b.avg_tps - 1600.0 / 5.0).abs() < 1e-9); // Σeff 1600 ÷ 5s
        assert!((b.peak_tps - 400.0).abs() < 1e-9);
        assert!((b.share - 1600.0 / 2100.0).abs() < 1e-9);
        assert!((b.buckets[0].tps - 200.0).abs() < 1e-9);
        assert_eq!(b.buckets[0].calls, 1);
        assert_eq!(b.buckets[0].tokens, 400);
        assert_eq!(b.buckets[1].calls, 0); // 空桶
        assert_eq!(b.buckets[1].tps, 0.0);
        assert_eq!(b.buckets[1].tokens, 0);
        assert!((b.buckets[2].tps - 400.0).abs() < 1e-9);
        assert_eq!(b.buckets[2].tokens, 1200);
        // 越界行未计入任何桶
        assert_eq!(b.buckets.iter().map(|x| x.calls).sum::<u64>(), 2);

        let a = &p.series[1];
        assert_eq!(a.total_calls, 2);
        assert_eq!(a.total_tokens, 500);
        assert!((a.avg_tps - 100.0).abs() < 1e-9); // 500 ÷ 5s
        assert!((a.peak_tps - 100.0).abs() < 1e-9);
        assert!((a.share - 500.0 / 2100.0).abs() < 1e-9);
        assert!((a.buckets[0].tps - 100.0).abs() < 1e-9);
        assert!((a.buckets[1].tps - 100.0).abs() < 1e-9);
        assert_eq!(a.buckets[2].calls, 0);
    }

    /// 窗口 clamp（999→360、0→10、40→60、400→360）与 gen_ms 缺失退化
    /// （first_token None → duration_ms → max(50) 兜底）
    #[test]
    fn model_stats_window_clamp_and_gen_fallback() {
        assert_eq!(clamp_window_min(999), 360);
        assert_eq!(clamp_window_min(0), 10);
        assert_eq!(clamp_window_min(40), 60);
        assert_eq!(clamp_window_min(400), 360);
        assert_eq!(clamp_window_min(60), 60);
        assert_eq!(clamp_window_min(360), 360);

        let now = 1_700_000_000_000i64;
        let rows = vec![
            mrow("m", now - 5_000, None, Some(5_000), 500, 0), // gen=5000ms
            mrow("m", now - 6_000, None, None, 100, 0),        // duration 缺失 → 50ms
            mrow("m", now - 7_000, Some(now - 7_000), Some(0), 100, 0), // ft 非正（=completed）→ dur 0 非正 → 50ms
        ];
        // window_min=999 归入 360（6 小时）：桶宽 360_000ms，三行都落最新桶
        let p = aggregate_model_stats(rows, 999, now);
        assert_eq!(p.window_min, 360);
        assert_eq!(p.bucket_ms, 360_000);
        assert_eq!(p.series.len(), 1);
        let s = &p.series[0];
        assert_eq!(s.buckets.len(), 60);
        assert_eq!(s.total_calls, 3);
        assert_eq!(s.total_tokens, 700);
        // Σeff 700 ÷ (5s + 50ms + 50ms)
        assert!((s.avg_tps - 700.0 / 5.1).abs() < 1e-9);
        assert!((s.buckets[0].tps - 700.0 / 5.1).abs() < 1e-9);
        assert_eq!(s.peak_tps, s.buckets[0].tps);
        assert!((s.share - 1.0).abs() < 1e-9);
        // 空输入 → 空 series
        let p = aggregate_model_stats(Vec::new(), 10, now);
        assert!(p.series.is_empty());
    }

    /// 历史统计：平均不过滤（含 duration 兜底行）；峰值记录准入——
    /// 有效 first_token + gen≥1s + eff≥300，三者缺一不入
    #[test]
    fn history_stats_fold_and_admission() {
        let now = 1_700_000_000_000i64;
        let mut h = HistoryStats::default();
        // 正常大调用：1000 tok / 4s = 250 t/s，入峰值
        h.fold(Some(now - 5_000), now - 1_000, 4_000, 1_000);
        // 更快的小调用：300 tok / 1.05s ≈ 285.7 t/s，eff=300 达标 → 应刷新峰值
        h.fold(Some(now - 3_000), now - 1_950, 1_050, 300);
        assert!((h.max_tps - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // 假记录陷阱：79 tok / 24ms（真实库实测形态，1580 t/s）——eff 与时长双不足
        h.fold(Some(now - 100), now - 76, 24, 79);
        assert!((h.max_tps - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // eff 达标但时长不足（500 tok / 200ms = 2500 t/s）→ 不入
        h.fold(Some(now - 300), now - 100, 200, 500);
        assert!((h.max_tps - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // duration 兜底行（ft 缺失）计入平均但不入峰值
        h.fold(None, now - 60_000, 10_000, 2_000);
        assert!((h.max_tps - 300.0 * 1000.0 / 1050.0).abs() < 1e-9);
        // 平均 = Σeff 3800 ÷ Σgen（24ms 行按 MIN_DUR_MS=50 进位）
        let total_eff = 1_000 + 300 + 79 + 500 + 2_000;
        let total_gen = 4_000 + 1_050 + 50 + 200 + 10_000;
        assert!((h.avg_tps() - total_eff as f64 / (total_gen as f64 / 1000.0)).abs() < 1e-9);
        assert_eq!(h.total_eff, total_eff);
        assert_eq!(h.total_gen_ms, total_gen);
        // 空库
        assert_eq!(HistoryStats::default().avg_tps(), 0.0);
        assert_eq!(HistoryStats::default().max_tps, 0.0);
    }

    /// 曲线聚合：90 桶、旧→新排列、越界丢弃、gen 兜底口径与今日 spark 一致。
    /// now 取槽内中段（非边界对齐），保证"几秒前完成"稳定落最新桶
    #[test]
    fn chart_stats_buckets_and_order() {
        let now = 1_700_000_005_000i64; // 10s 槽内第 5s
        let rows = vec![
            // 15 分钟档桶宽 10s：3s 前完成 → 与 now 同槽 → 最新桶（末位）。
            // gen = ft 差 12s，eff 1000 → 83.3 t/s
            mrow("m", now - 3_000, Some(now - 15_000), Some(15_000), 1_000, 0),
            // 95s 前完成 → 距最新槽 9 桶 → 索引 89-9=80；gen 4s eff 3000 → 750 t/s
            mrow("m", now - 95_000, Some(now - 99_000), Some(9_000), 3_000, 0),
            // 窗口外（20 分钟前）→ 丢弃
            mrow("m", now - 1_200_000, Some(now - 1_204_000), None, 9_999, 0),
        ];
        let p = aggregate_chart_stats(rows, 15, now);
        assert_eq!(p.window_min, 15);
        assert_eq!(p.bucket_ms, 10_000);
        assert_eq!(p.buckets.len(), 90);
        assert!((p.buckets[89] - 1_000.0 * 1000.0 / 12_000.0).abs() < 1e-9); // 最新桶
        assert!((p.buckets[80] - 750.0).abs() < 1e-9);
        assert!((p.buckets[0] - 0.0).abs() < 1e-9); // 空桶
    }

    /// 曲线窗口 clamp：999→1440、30→15、90→60、720→360；1h 档桶宽 40s
    #[test]
    fn chart_stats_window_clamp() {
        assert_eq!(clamp_chart_window(999), 1440);
        assert_eq!(clamp_chart_window(30), 15);
        assert_eq!(clamp_chart_window(90), 60);
        assert_eq!(clamp_chart_window(720), 360);
        assert_eq!(clamp_chart_window(15), 15);
        let p = aggregate_chart_stats(Vec::new(), 60, 1_700_000_000_000i64);
        assert_eq!(p.bucket_ms, 60 * 60_000 / 90);
        assert_eq!(p.buckets.len(), 90);
        assert!(p.buckets.iter().all(|v| *v == 0.0));
    }
}
