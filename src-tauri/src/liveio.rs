//! 实时速度实测：轮询 ZCode CLI 进程的 IO 写字节计数（往桌面 UI 管道的流式渲染数据），
//! 在模型流式输出期间该计数会以数十 KB/s 持续增长，是真实的实时信号。
//!
//! 平台原语见 [`platform`]：Windows 用 Toolhelp 枚举 + GetProcessIoCounters 读
//! 累计写字节；macOS 用 libproc 枚举 + proc_pid_rusage 的 ri_diskio_byteswritten。
//!
//! - 进程发现：Windows 过滤 `zcode.exe` 且命令行含 `zcode.cjs`；macOS 按
//!   KERN_PROCARGS2 命令行参数精确匹配 `zcode-cli`（均可多进程并存）
//! - 落盘扣除（仅 Windows）：rollout/日志/WAL 的增长从字节增量中减去（完成/flush
//!   瞬间的尖峰来源），在**聚合流**上整体只扣一次（多进程求和时逐进程各扣一遍
//!   会把全局文件增量扣 N 倍）。扣除允许单拍为负（flush 与写入错位时由区间总和
//!   收敛），只在窗口汇总时钳非负——若逐拍钳 0，错位的落盘增量会被永久吞掉
//!   （实测读数塌缩到真值的 1/5 就是这个原因）。macOS 不做该扣除（rollout 目录
//!   净变化可为负，方向性反噬清洗流，见 platform::mac 的 tracked_files_total）
//! - 噪声底：BASE_NOISE + 每进程自适应心跳底（封顶，防止持续流式期间分位数被
//!   流式增量"毒化"，把自己的输出当噪声扣掉）；两平台参数不同（mac idle 实测
//!   严格 0 字节，静态底噪为 0）
//! - 突发剔除：单拍原始增量超过阈值整拍丢弃，不进积分（仅 Windows：请求体
//!   上传 ~190KB/拍与真实流式 ~52KB/拍可分；mac 流式本身就是单拍突发形态，
//!   阈值在 CleanParams 中禁用）
//! - 字节→token 换算【一致性校准】：调用完成后用 与显示路径完全相同的清洗流
//!   在 [first_token, completed] 区间的积分字节 ÷ 真实 output_tokens 做滑动自校准。
//!   校准分子与显示分子同源，任何系统性扣除（噪声底/落盘/错位）都会被系数抵消，
//!   显示值收敛到真实 t/s。历史教训：校准用未清洗的总字节流、显示用清洗后的流，
//!   两条链路口径不一致曾导致系数被抬高 2~3 倍、读数系统性偏低。样本准入另带
//!   跨进程守卫：窗口内其他进程字节占比过高（对方窗口并发流式）时拒收，防止
//!   归因进程的积分混入外来字节污染系数。
//!   mac 的磁盘写字节为页缓存异步落盘计数（滞后 write() 数秒~数十秒），校准
//!   窗口延长到 completed + cal_grace_ms（延迟落盘宽限，Windows=0 当拍处理），
//!   并对偏离生效系数超倍的样本做离群拒绝（Windows 禁用）。

use crate::metrics::Call;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// 当前速度统计窗口（显示幅度）
const WINDOW_MS: i64 = 30_000;
/// 进程/文件采样环容量（~3min @700ms，覆盖校准回溯区间）
const RING_CAP: usize = 260;
/// 进程列表刷新周期
const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// 流式判定阈值（清洗后速率）
const STREAMING_BPS: f64 = 4_000.0;
/// 启停由调用门控决定；该短窗仅用于定位幅度锚点（首字节拍）
const DETECT_MS: i64 = 2_500;

/// 判停兜底宽限：门控开着（message 行 completed 未补写落盘，CLI 侧可延迟
/// 数秒~分钟）但流式锚点出现后清洗流速持续低于流式阈值达该时长 → 判定生成
/// 实际已停，读数归零不再显示"生成中/估算"。锚点未建立的管道静默调用不受
/// 影响（全程无字节是其常态，由 window 回退服务）；正常流式的字节间歇远短
/// 于该值，误判时字节恢复当拍自愈
const SILENT_STOP_MS: i64 = 15_000;
/// 启动提示窗口：门控已开但尚未观测到流式字节（TTFT）的时长上限。
/// 窗口内显示"统计中…"提示（不显示误导性的估算值）；超窗仍无字节则视为
/// 管道静默调用，回退到近期真值估算（≈）
const TTFT_HINT_MS: i64 = 20_000;
/// 待机心跳底噪的粗略上界（B/s），叠加每进程自适应底噪（封顶后）过滤心跳
const BASE_NOISE_BPS: f64 = 3_000.0;
/// 每进程自适应心跳底的单拍封顶（字节/拍）。实测流式可到 ~75KB/s（52KB/拍），
/// 分位数底噪若不封顶，持续流式期间会被流式增量抬高，把自己的输出当噪声扣掉
const FLOOR_CAP_BYTES: f64 = 2_000.0;
/// 单拍原始增量的剔除阈值：请求体上传实测 ~190KB/拍（拆分也 >90KB），
/// 真实流式最大 ~52KB/拍，取中间值整拍丢弃（字节与时长都不进积分）
const BURST_TICK_BYTES: f64 = 100_000.0;
/// 初始字节/token 系数（清洗流实测约 350~900，取偏保守值，校准后快速收敛）
const DEFAULT_BPT: f64 = 600.0;
/// 校准样本的合理区间（防异常样本污染中位数）。一致性校准下系数会吸收系统性
/// 扣除（落盘镜像/噪声底/突发拍剔除），区间必须足够宽以免破坏收敛；
/// 垃圾样本主要由 CAL_MIN_TOKENS 的调用规模门槛拦截
const CAL_MIN: f64 = 100.0;
const CAL_MAX: f64 = 6_000.0;
/// 校准样本的调用规模下限：小调用的 UI 固定帧开销占比大，禁止入样本
const CAL_MIN_TOKENS: u64 = 300;
/// 轮均速漂移自动重校准：一轮 = 门控"进行中"信号连续的一段，轮内显示速度
/// （io 实测拍）取算术平均；上轮均值与之前连续 DRIFT_ROUNDS 轮的均值差异
/// ≥ DRIFT_RATIO 倍（双向）判定量级突变（换模型/分词器，旧系数大概率过期）
const DRIFT_ROUNDS: usize = 5;
const DRIFT_RATIO: f64 = 3.0;
/// 会话→进程归属的切换迟滞：已有归属的会话，仅当候选进程在调用窗口内的原始
/// 字节 ≥ 现归属进程的该倍数才切换。两个 CLI 窗口并发流式时 top-writer 会逐
/// 调用翻转（2026-09-18 现场：同会话相邻两次调用归属在两个 pid 间摆动，校准
/// 样本被对方窗口的字节污染，系数在 262~764 间摆动、读数偏差 2~3 倍）
const ATTR_SWITCH_RATIO: f64 = 2.0;
/// 并发归属去重阈值：top 进程已被另一个进行中会话占用时，次高进程窗口字节
/// 达到 top 的该比例才改归次高。两路并发流式速率相近（比例 ~1），空闲进程
/// 的底噪泄漏 ~0.1；0.5 居中——纠正平局错归，又不把共享进程的会话推给
/// 空闲进程（2026-09-18 现场：同一 ZCode 窗口新开任务复用同一 app-server
/// 进程，两会话字节全走同一 pid，次高只有 ~0.17 比例的噪声）
const ATTR_DEDUP_RATIO: f64 = 0.5;
/// 校准样本跨进程守卫：调用窗口内**其他**进程的原始字节超过归属进程的该比例
/// 即拒收样本——此时归属进程的清洗积分必然混入对方窗口的流式语义，样本失真
const CROSS_PID_RATIO: f64 = 0.2;

/// 清洗/校准参数（平台参数化）。Windows 列为长期实测调优值（上方原常量，
/// 禁改）；macOS 列基于 120s 探针 + 2026-09-17 真值对账（6 条 cal 事件，
/// 归因正确时 pred_tps 与 true_tps 完全一致）修正：流式期
/// ri_diskio_byteswritten ≈195KB/s、idle 严格 0 字节、单拍增量突发式
/// 0,0,0,+225KB~1.5MB、B/token 真值 ≈650（探针期的 ≈3900 为误判）、
/// 磁盘计数为页缓存异步落盘（滞后 write() 数秒~数十秒）。
#[derive(Clone, Copy)]
pub struct CleanParams {
    /// 单拍原始增量剔除阈值（Windows：请求体上传 ~190KB/拍，与真实流式
    /// ~52KB/拍之间取整拍丢弃）。mac：流式本身就是单拍突发形态
    ///（225KB~1.5MB 是常态信号），取 u64::MAX 禁用——100KB 阈值会丢全部信号
    pub burst_tick_bytes: f64,
    /// 待机心跳底噪粗略上界（B/s）。mac idle 实测严格 0 字节，无需静态底噪
    pub base_noise_bps: f64,
    /// 每进程自适应心跳底的单拍封顶（字节/拍）。两平台一致：封顶只防
    /// 分位数被毒化，mac idle 恒 0 时自适应会自行降 0
    pub floor_cap_bytes: f64,
    /// 校准样本 B/token 合理区间下/上界。mac 实测 ≈3900，上限放宽留余量
    pub cal_min: f64,
    pub cal_max: f64,
    /// 校准样本的调用规模下限（平台无关）
    pub cal_min_tokens: u64,
    /// 初始字节/token 系数先验。Windows 长期 600；mac 真值对账（2026-09-17，
    /// 6 条 cal 事件）实测接受样本 614/724，取 700——旧值 2000 源自 120s 探针
    /// 的 ≈3900 误判，冷启动读数 3 倍低估
    pub default_bpt: f64,
    /// 幅度锚点探测短窗（首字节拍）。首版两平台一致，mac 若状态抖动再调
    pub detect_ms: i64,
    /// 校准延迟落盘宽限（ms）：调用完成后 pending 等满该时长再积分，校准积分
    /// 与 raw 统计窗口上限同步延长到 completed + grace。mac 的
    /// ri_diskio_byteswritten 是页缓存异步落盘计数，滞后 write() 数秒~数十秒
    ///（实测 117s 长调用 96% 字节落在 completed 之后，用户盯着 0.7 t/s 两分钟
    /// 而真值 65.3）；Windows 的 WriteTransferCount 为同步计数，取 0 = 当拍处理
    pub cal_grace_ms: i64,
    /// 校准样本离群拒绝倍数：样本 B/token 与当前生效系数偏差超该倍数即拒收
    ///（延迟落盘的半截样本 / 归因异常样本不进中位数）。0 = 禁用（Windows）
    pub cal_outlier_ratio: f64,
}

impl CleanParams {
    /// Windows 长期实测值（原常量原值，禁改）
    #[allow(dead_code)] // mac 构建下仅测试引用；bin 构建时未使用
    pub fn windows() -> Self {
        Self {
            burst_tick_bytes: BURST_TICK_BYTES,
            base_noise_bps: BASE_NOISE_BPS,
            floor_cap_bytes: FLOOR_CAP_BYTES,
            cal_min: CAL_MIN,
            cal_max: CAL_MAX,
            cal_min_tokens: CAL_MIN_TOKENS,
            default_bpt: DEFAULT_BPT,
            detect_ms: DETECT_MS,
            // 延迟落盘宽限与离群拒绝在 Windows 禁用：WriteTransferCount 同步计数，
            // 当拍处理、无离群过滤，行为与历史版本逐字节等价
            cal_grace_ms: 0,
            cal_outlier_ratio: 0.0,
        }
    }

    /// macOS 实测值：burst 即信号须禁用剔除、无静态底噪、系数先验按真值对账
    /// 取 700、延迟落盘宽限 15s（页缓存异步落盘滞后）、样本离群 3 倍拒绝
    #[cfg(target_os = "macos")]
    pub fn macos() -> Self {
        Self {
            burst_tick_bytes: u64::MAX as f64,
            base_noise_bps: 0.0,
            floor_cap_bytes: FLOOR_CAP_BYTES,
            cal_min: CAL_MIN,
            cal_max: 12_000.0,
            cal_min_tokens: CAL_MIN_TOKENS,
            default_bpt: 700.0,
            detect_ms: DETECT_MS,
            cal_grace_ms: 15_000,
            cal_outlier_ratio: 3.0,
        }
    }

    pub fn platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::macos()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::windows()
        }
    }
}

#[derive(Default, Clone)]
pub struct LiveNow {
    /// 是否成功发现了 CLI 进程（false 时前端回退到窗口/估算显示）
    pub available: bool,
    pub streaming: bool,
    /// 流式已开始但 30s 滑窗尚未填满（读数来自已活跃区间，前端显示"统计中"）
    pub ramping: bool,
    /// 调用已开始但尚未观测到首字节（TTFT，限制在提示窗口内）：
    /// 前端显示"统计中…"提示而非估算值
    pub awaiting: bool,
    pub tps: f64,
    /// 清洗后的管道字节率（B/s），调试日志/对账用
    pub pipe_bps: f64,
    /// 本拍聚合窗口内贡献达到流式量级的进程数（1 = 单任务；>1 = 多任务聚合；
    /// 空闲进程的底噪泄漏不计入）。轮漂移检测只采单进程轮——任务数变化带来
    /// 的天然吞吐差不是系数漂移
    pub n_pids: usize,
    /// 分任务明细：显示集合内每个进程的实时速度（明细之和 = 聚合读数）
    pub tasks: Vec<TaskLive>,
    /// 诊断：每台被跟踪进程的探测窗清洗速率（KB/s）。多任务排查对账用
    ///（2026-09-18 排查时 tick 只有 npids 单字段，无法回答"哪台进程在写"）
    pub proc_bps: Vec<(u32, f64)>,
}

/// 分任务实时明细（`LiveNow::tasks` 元素）：一个 CLI 进程 = 一个任务行。
/// 同进程内并行的多个子代理在字节层不可拆分，如实显示为该进程合计
#[derive(Clone, Debug, Default)]
pub struct TaskLive {
    pub pid: u32,
    /// 归属的进行中会话（尚无归属记录的流式进程为 None）
    pub session: Option<String>,
    /// 该进程承载的进行中会话数（≥2 = 同进程多任务，速度为合计，标签见 n_sessions）
    pub n_sessions: usize,
    pub tps: f64,
    pub streaming: bool,
}

/// 一次调用完成后的校准与对账事件（调试日志用）
#[derive(Clone, Debug, serde::Serialize)]
pub struct CalEvent {
    pub id: String,
    pub session: String,
    pub completed_ms: i64,
    /// 调用真值（落盘 output+reasoning ÷ 生成时长）
    pub true_tps: f64,
    pub gen_ms: i64,
    pub eff: u64,
    /// 流式区间原始写字节（未清洗，全进程求和；归属判定与对账基线）
    pub raw_bytes: f64,
    /// 与显示同口径的清洗流在 [first_token, completed] 的积分字节（本调用系数分子）
    pub clean_bytes: f64,
    /// 本调用清洗流样本 B/token（0 = 未入校准）
    pub bpt_sample: f64,
    /// 事件后生效的系数
    pub bpt_now: f64,
    /// 未入校准（调用过短 / 无有效字节 / 离群拒收）
    pub cal_skipped: bool,
    /// clean 积分实际使用的进程（归属进程积分分支；全进程求和分支为 None）
    pub attr_pid: Option<u32>,
    /// raw_by_pid 中原始字节最大的进程（归因异常定位：attr 与 top 不一致
    /// 且 clean 远小于 raw 即归属错了进程）
    pub top_pid: Option<u32>,
    /// 调用窗口内其他进程的原始字节（跨进程守卫诊断：占比超过归属进程的
    /// CROSS_PID_RATIO 时样本被拒收）
    pub others_bytes: f64,
}

// ============ 纯计算部分（跨平台，可单测）：拍清洗 / 区间积分 / 中位数 ============

/// 清洗后的单拍管道字节。bytes 可为负：落盘 flush 与 IO 计数错位时，
/// 由区间积分求和收敛（Σ bytes = Σ 原始增量 − Σ 落盘 − Σ 噪声底），不能逐拍钳 0
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TickRow {
    /// 拍时长
    pub dt_ms: i64,
    /// 清洗后字节（原始写入 − 落盘增长 − 噪声底，可能为负）
    pub bytes: f64,
    /// 拍结束时刻（墙钟 ms）
    pub end_ms: i64,
}

/// 由累计写字节序列构建清洗后的拍序列（仅扣逐进程噪声底；tracked 文件增长
/// 由 [`merge_streams`] 在聚合流上统一扣除——多进程求和时逐进程各扣一遍会把
/// 全局文件增量扣 N 倍）。bytes 可为负（落盘错位由区间积分对消）
pub(crate) fn build_rows(samples: &[(i64, u64)], min_delta: f64, p: &CleanParams) -> Vec<TickRow> {
    let floor_static = min_delta.min(p.floor_cap_bytes);
    let mut rows = Vec::with_capacity(samples.len());
    for w in samples.windows(2) {
        let (t0, w0) = w[0];
        let (t1, w1) = w[1];
        let dt_ms = t1 - t0;
        if dt_ms <= 0 {
            continue;
        }
        let raw = w1.saturating_sub(w0) as f64;
        // 请求体上传等单拍突发：整拍剔除（既不积分字节也不计时长）；
        // mac 下 burst 即信号，阈值在 CleanParams 中禁用
        if raw > p.burst_tick_bytes {
            continue;
        }
        let dt_s = dt_ms as f64 / 1000.0;
        rows.push(TickRow {
            dt_ms,
            bytes: raw - floor_static - p.base_noise_bps * dt_s,
            end_ms: t1,
        });
    }
    rows
}

/// 把多条进程清洗流合并为一条聚合流：同拍字节求和、墙钟时长只计一次
/// （逐进程分别积分会把时长也求和，速率被摊薄成跨进程均值）、tracked 文件
/// 增长整体只扣一次。行按 end_ms 对齐（同一轮询循环成对采样，时间戳一致；
/// 某进程缺拍的区间其字节贡献自然为 0）。单条流输入时与逐行扣文件的
/// 历史算术完全等价
pub(crate) fn merge_streams(streams: &[&[TickRow]], files: &[(i64, u64)]) -> Vec<TickRow> {
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<i64, (i64, f64)> = BTreeMap::new();
    for rows in streams {
        for r in rows.iter() {
            let e = merged.entry(r.end_ms).or_insert((r.dt_ms, 0.0));
            // 同拍 dt 必须一致（同一轮询循环成对采样）——一旦将来采样时刻分化，
            // 按 end_ms 合并会静默分裂成两行导致时长双计，这里让它尽早炸出来
            debug_assert_eq!(e.0, r.dt_ms);
            e.1 += r.bytes;
        }
    }
    let mut out = Vec::with_capacity(merged.len());
    for (end_ms, (dt_ms, bytes)) in merged {
        // [t0, end) 区间内 tracked 文件的落盘增长（边界半开，避免相邻区间重复计入）
        let t0 = end_ms - dt_ms;
        let mut fg = 0f64;
        for f in files.windows(2) {
            let (ft0, fv0) = f[0];
            let (ft1, fv1) = f[1];
            if ft1 > t0 && ft0 < end_ms {
                fg += fv1.saturating_sub(fv0) as f64;
            }
        }
        out.push(TickRow {
            dt_ms,
            bytes: bytes - fg,
            end_ms,
        });
    }
    out
}

/// tracked 文件总量序列 → 增长拍序列（供分任务明细按窗口分摊扣除）
pub(crate) fn file_growth_rows(files: &[(i64, u64)]) -> Vec<TickRow> {
    files
        .windows(2)
        .filter_map(|w| {
            let (t0, v0) = w[0];
            let (t1, v1) = w[1];
            let dt_ms = t1 - t0;
            if dt_ms <= 0 {
                return None;
            }
            Some(TickRow {
                dt_ms,
                bytes: v1.saturating_sub(v0) as f64,
                end_ms: t1,
            })
        })
        .collect()
}

/// 按时间比例积分 [from_ms, to_ms] 区间：跨界拍按重叠时长分摊。
/// 返回 (字节, 秒)。被剔除的突发拍不贡献时长，不稀释速率
pub(crate) fn integrate(rows: &[TickRow], from_ms: i64, to_ms: i64) -> (f64, f64) {
    let (mut bytes, mut secs) = (0f64, 0f64);
    for r in rows {
        let lo = (r.end_ms - r.dt_ms).max(from_ms);
        let hi = r.end_ms.min(to_ms);
        if hi <= lo {
            continue;
        }
        let ov = (hi - lo) as f64;
        let dt = r.dt_ms as f64;
        bytes += r.bytes * (ov / dt);
        secs += ov / 1000.0;
    }
    (bytes, secs)
}

/// 校准系数维护：滑动窗口样本的中位数（纯函数便于测试）
pub(crate) fn median_bpt(samples: &mut VecDeque<f64>, sample: f64, cap: usize) -> f64 {
    samples.push_back(sample);
    while samples.len() > cap {
        samples.pop_front();
    }
    let mut sorted: Vec<f64> = samples.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    sorted[sorted.len() / 2]
}

/// 实时显示的进程集合选择（纯函数便于测试）：
/// - 进行中会话全部有存活归属 → 归属 pid 的并集：多任务并发时聚合为真实总吞吐，
///   同时产出分任务明细；
/// - 任一进行中会话无归属（新会话/子代理的首个调用尚未完成过）→ None =
///   全进程求和兜底：该会话自己的进程还没有归属记录，只按并集算会漏掉它、
///   显示成别的窗口的速度；空闲进程经底噪清洗后贡献 ≈ 0，代价可忽略；
/// - 无进行中会话 → None（门控关闭，读数归零，走哪条分支不影响）
pub(crate) fn pick_pid_set(
    inflight: &[(String, i64)],
    session_pid: &HashMap<String, u32>,
    active_pids: &HashSet<u32>,
) -> Option<Vec<u32>> {
    if inflight.is_empty() {
        return None;
    }
    let mut set = Vec::with_capacity(inflight.len());
    for (s, _) in inflight {
        match session_pid.get(s) {
            Some(pid) if active_pids.contains(pid) => set.push(*pid),
            // 无归属或归属进程已死 → 求和兜底（session_pid 每拍按存活 pid 淘汰，
            // 这里遇到的"已死"只会是本拍内竞态，兜底同样安全）
            _ => return None,
        }
    }
    set.sort_unstable();
    set.dedup();
    Some(set)
}

/// 会话→进程归属切换判定（纯函数便于测试）：返回本次应写入的归属 pid。
/// 已有归属时带迟滞——仅当候选 top 进程窗口内原始字节 ≥ 现归属的
/// ATTR_SWITCH_RATIO 倍才切换，杜绝并发窗口间逐调用翻转；无现归属或
/// 现归属窗口内零字节（归属过期的自愈路径）时直接采信 top
pub(crate) fn should_reattribute(
    cur: Option<u32>,
    top: Option<u32>,
    raw_by_pid: &HashMap<u32, u64>,
) -> Option<u32> {
    let top = top?;
    match cur {
        Some(c) if c != top => {
            let cur_raw = *raw_by_pid.get(&c).unwrap_or(&0) as f64;
            let top_raw = *raw_by_pid.get(&top).unwrap_or(&0) as f64;
            if top_raw >= cur_raw * ATTR_SWITCH_RATIO && top_raw > cur_raw {
                Some(top)
            } else {
                Some(c)
            }
        }
        _ => Some(top),
    }
}

/// 会话→进程归属判定（纯函数便于测试）：返回本次应写入的归属 pid。
/// `owned` = 其他**进行中**会话已归属的 pid 集合。三个分支：
/// - 首次归属：top 已被占用且次高字节达 top 的 ATTR_DEDUP_RATIO → 改归次高
///   （并发平局下 max_by 取 top 是随机的，两会话会一起挤到同一台进程上，
///   显示集合随之塌缩成单进程）；次高只有噪声比例则维持 top（真实共享）
/// - 现归属被另一个进行中会话占用：top 未被占用且字节达现归属的一半即可
///   切换——被占用场景下 2 倍迟滞会把历史错归锁死
/// - 现归属未被占用：维持 2 倍迟滞原语义（should_reattribute）
pub(crate) fn pick_attribution(
    cur: Option<u32>,
    raw_by_pid: &HashMap<u32, u64>,
    owned: &HashSet<u32>,
) -> Option<u32> {
    // 候选按窗口字节降序、平局按 pid 升序——遍历序确定，不再依赖 HashMap 顺序
    let mut cands: Vec<(u32, u64)> = raw_by_pid.iter().map(|(p, b)| (*p, *b)).collect();
    cands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top = cands.first().copied()?;
    match cur {
        None => {
            if owned.contains(&top.0) {
                if let Some((pid, bytes)) = cands.get(1) {
                    if !owned.contains(pid)
                        && *bytes as f64 >= (top.1 as f64) * ATTR_DEDUP_RATIO
                        && *bytes >= 20_000
                    {
                        return Some(*pid);
                    }
                }
            }
            Some(top.0)
        }
        Some(c) if c != top.0 && owned.contains(&c) => {
            // 现归属被占用：top 未被占用且字节达现归属的一半即可切换
            //（被占用场景 2 倍迟滞会把历史错归锁死；top 也被占用则无处可去）
            if !owned.contains(&top.0)
                && top.1 as f64 >= (*raw_by_pid.get(&c).unwrap_or(&0) as f64) * ATTR_DEDUP_RATIO
            {
                Some(top.0)
            } else {
                Some(c)
            }
        }
        Some(c) => should_reattribute(Some(c), Some(top.0), raw_by_pid),
    }
}

/// 校准样本跨进程守卫（纯函数便于测试）：调用窗口内其他进程的原始字节
/// 不超过归属进程的 CROSS_PID_RATIO 才允许入样。对方窗口并发流式时，
/// 归属进程的清洗积分窗口内必然混入外来字节，B/token 样本失真
pub(crate) fn cross_pid_ok(others_raw: f64, attr_raw: f64) -> bool {
    attr_raw > 0.0 && others_raw <= attr_raw * CROSS_PID_RATIO
}

/// 校准样本准入：管道积分须有实质贡献（≥原始字节的 20%），且 B/token 未钳位
/// 就落在合理区间。管道静默调用（字节全在完成瞬间落盘，实测样本可低至
/// ~7 B/token）与异常比例样本整条拒绝，防止中位数系数被污染。
/// 在此基础上，样本与当前生效系数 bpt_now 偏差超 cal_outlier_ratio 倍时
/// 拒收（mac：延迟落盘只积分到一半字节/归因错进程的半截样本不进中位数；
/// Windows ratio=0 显式禁用，行为与历史版本一致）。
/// 返回 (是否入样, 样本值)
pub(crate) fn cal_sample(
    eff: u64,
    clean_bytes: f64,
    raw_bytes: f64,
    bpt_now: f64,
    p: &CleanParams,
) -> (bool, f64) {
    if eff < p.cal_min_tokens || clean_bytes <= 0.0 || raw_bytes <= 0.0 {
        return (false, 0.0);
    }
    let ratio = clean_bytes / eff as f64;
    let usable = clean_bytes / raw_bytes >= 0.2;
    let in_range = usable && ratio >= p.cal_min && ratio <= p.cal_max;
    // 离群拒绝：outlier_ratio=0（Windows）显式禁用，避免 0 作除数/0 乘误判；
    // 拒收样本值记 0（与调用侧 cal_skipped 时 bpt_sample=0 的口径一致）
    if in_range
        && p.cal_outlier_ratio > 0.0
        && bpt_now > 0.0
        && (ratio < bpt_now / p.cal_outlier_ratio || ratio > bpt_now * p.cal_outlier_ratio)
    {
        return (false, 0.0);
    }
    (in_range, ratio)
}

/// 校准样本队列容量（滑动窗口，含预置先验占位）
const CAL_QUEUE_CAP: usize = 5;

/// 启动提示判定（纯函数）：门控开启、尚无流式锚点（首字节未到）且距调用开始
/// 仍在提示窗口内。窗口外保持无锚点 = 管道静默调用，由上层回退到估算显示
pub(crate) fn awaiting_hint(
    inflight_started: Option<i64>,
    anchor: Option<i64>,
    now_ms: i64,
) -> bool {
    match (inflight_started, anchor) {
        (Some(started), None) => now_ms - started <= TTFT_HINT_MS,
        _ => false,
    }
}

/// 判停兜底（纯函数）：门控开着且流式锚点已建立，但最近一次达到流式阈值的
/// 时刻距今超过宽限 → 生成实际已停（等 completed 落盘期间不再挂着"生成中"）
pub(crate) fn stale_stop(anchor: Option<i64>, last_stream_ms: Option<i64>, now_ms: i64) -> bool {
    anchor.is_some()
        && last_stream_ms.map_or(false, |t| now_ms - t > SILENT_STOP_MS)
}

/// 轮均速漂移检测（纯函数便于测试）：逐轮喂入显示速度均值，与之前连续
/// DRIFT_ROUNDS 轮的均值比较，双向差异 ≥ DRIFT_RATIO 倍即判定速度量级突变，
/// 应触发重新校准（`LiveIo::reset_calibration`）。触发后清空历史，
/// 新量级重新积累基线，避免同一突变反复触发
#[derive(Default)]
pub struct RoundDrift {
    history: VecDeque<f64>,
}

impl RoundDrift {
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(DRIFT_ROUNDS),
        }
    }

    /// 观测一轮的显示均值。返回 Some(基线均值) = 触发重校准（基线供日志）；
    /// 均值 ≤0 的轮（管道静默、无实测拍）不参与也不入历史
    pub fn observe(&mut self, round_avg: f64) -> Option<f64> {
        if round_avg <= 0.0 {
            return None;
        }
        let base = (self.history.len() == DRIFT_ROUNDS)
            .then(|| self.history.iter().sum::<f64>() / DRIFT_ROUNDS as f64);
        if let Some(b) = base {
            if round_avg / b >= DRIFT_RATIO || b / round_avg >= DRIFT_RATIO {
                self.history.clear();
                return Some(b);
            }
        }
        self.history.push_back(round_avg);
        while self.history.len() > DRIFT_ROUNDS {
            self.history.pop_front();
        }
        None
    }

    /// 清空历史（手动重校准后同步复位，新基线从零积累）
    pub fn reset(&mut self) {
        self.history.clear();
    }
}

struct ProcRing {
    handle: platform::ProcHandle,
    samples: VecDeque<(i64, u64)>,
    /// 该进程最小的每拍增量（自适应心跳噪声底，使用时封顶）
    min_delta: f64,
}

/// 平台进程原语：进程发现 / 打开句柄 / 读累计写字节 / tracked 文件总量。
/// 三份实现按 cfg 选择，对外路径统一为 `liveio::platform::*`（examples 复用）。
/// 所有 FFI 失败路径返回 None/空 Vec，禁止 panic
pub mod platform {
    /// Windows：Toolhelp 枚举 + 读命令行过滤 CLI 子进程；GetProcessIoCounters
    /// 读进程启动以来累计写字节（WriteTransferCount，内核维护，权威）
    #[cfg(windows)]
    mod win {
        use std::ffi::c_void;

        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn CloseHandle(h: *mut c_void) -> i32;
            fn GetProcessIoCounters(h: *mut c_void, counters: *mut IoCounters) -> i32;
            fn ReadProcessMemory(
                h: *mut c_void,
                addr: *const c_void,
                buf: *mut c_void,
                size: usize,
                read: *mut usize,
            ) -> i32;
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
            fn Process32FirstW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
            fn Process32NextW(snap: isize, entry: *mut ProcessEntry32W) -> i32;
        }
        #[repr(C)]
        struct IoCounters {
            read_ops: u64,
            write_ops: u64,
            other_ops: u64,
            read_bytes: u64,
            write_bytes: u64,
            other_bytes: u64,
        }

        #[repr(C)]
        struct ProcessEntry32W {
            size: u32,
            usage: u32,
            process_id: u32,
            default_heap_id: usize,
            module_id: u32,
            threads: u32,
            parent_process_id: u32,
            pri_class_base: i32,
            flags: u32,
            exe_file: [u16; 260],
        }

        const PROCESS_QUERY_LIMITED: u32 = 0x1410; // QUERY_INFORMATION | QUERY_LIMITED | VM_READ
        const TH32CS_SNAPPROCESS: u32 = 2;

        fn process_command_line(pid: u32) -> Option<String> {
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED, 0, pid);
                if h.is_null() {
                    return None;
                }
                // 经典方案：ProcessBasicInformation → PEB → ProcessParameters → CommandLine
                let rd = |addr: usize, buf: &mut [u8]| -> bool {
                    let mut n = 0usize;
                    ReadProcessMemory(h, addr as *const c_void, buf.as_mut_ptr().cast(), buf.len(), &mut n)
                        != 0
                };
                let mut pbi = [0u8; 48];
                let mut ret: u32 = 0;
                if NtQueryInformationProcess(h, 0, pbi.as_mut_ptr().cast(), 48, &mut ret) != 0 {
                    CloseHandle(h);
                    return None;
                }
                #[cfg(target_pointer_width = "64")]
                {
                    let peb = usize::from_ne_bytes(pbi[8..16].try_into().ok()?);
                    if peb == 0 {
                        CloseHandle(h);
                        return None;
                    }
                    let mut pp_ptr = [0u8; 8];
                    if !rd(peb + 0x20, &mut pp_ptr) {
                        CloseHandle(h);
                        return None;
                    }
                    let pp = usize::from_ne_bytes(pp_ptr.try_into().ok()?);
                    if pp == 0 {
                        CloseHandle(h);
                        return None;
                    }
                    // RTL_USER_PROCESS_PARAMETERS.CommandLine (UNICODE_STRING) @ 0x70
                    let mut us = [0u8; 16];
                    if !rd(pp + 0x70, &mut us) {
                        CloseHandle(h);
                        return None;
                    }
                    let len = u16::from_ne_bytes([us[0], us[1]]) as usize;
                    let buf_ptr = usize::from_ne_bytes(us[8..16].try_into().ok()?);
                    if len == 0 || buf_ptr == 0 {
                        CloseHandle(h);
                        return None;
                    }
                    let mut wbuf = vec![0u8; len];
                    if !rd(buf_ptr, &mut wbuf) {
                        CloseHandle(h);
                        return None;
                    }
                    let u16s: Vec<u16> = wbuf
                        .chunks_exact(2)
                        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                        .collect();
                    CloseHandle(h);
                    return Some(String::from_utf16_lossy(&u16s));
                }
                #[cfg(not(target_pointer_width = "64"))]
                {
                    CloseHandle(h);
                    None
                }
            }
        }

        pub fn discover_cli_pids() -> Vec<u32> {
            let mut pids = Vec::new();
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == -1 {
                    return pids;
                }
                let mut entry = ProcessEntry32W {
                    size: std::mem::size_of::<ProcessEntry32W>() as u32,
                    usage: 0,
                    process_id: 0,
                    default_heap_id: 0,
                    module_id: 0,
                    threads: 0,
                    parent_process_id: 0,
                    pri_class_base: 0,
                    flags: 0,
                    exe_file: [0; 260],
                };
                let ok = Process32FirstW(snap, &mut entry);
                if ok != 0 {
                    loop {
                        let exe = String::from_utf16_lossy(
                            &entry.exe_file[..entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260)],
                        );
                        if exe.eq_ignore_ascii_case("zcode.exe") {
                            if let Some(cmd) = process_command_line(entry.process_id) {
                                if cmd.contains("zcode.cjs") {
                                    pids.push(entry.process_id);
                                }
                            }
                        }
                        if Process32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap as *mut c_void);
            }
            pids
        }

        /// 系统里是否存在任意 ZCode 进程（进程名 zcode.exe，桌面端壳与 CLI
        /// 子进程同名——不读命令行，名字匹配即算）。供自动启动 follow 模式
        /// 的待命检测用（autostart.rs）：桌面端或 CLI 任一在跑都算「ZCode
        /// 正在运行」。与 discover_cli_pids 的区别：那要读命令行精确过滤
        /// CLI 子进程，这里只要名字命中，更快也更宽
        pub fn any_zcode_process() -> bool {
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == -1 {
                    return false;
                }
                let mut entry = ProcessEntry32W {
                    size: std::mem::size_of::<ProcessEntry32W>() as u32,
                    usage: 0,
                    process_id: 0,
                    default_heap_id: 0,
                    module_id: 0,
                    threads: 0,
                    parent_process_id: 0,
                    pri_class_base: 0,
                    flags: 0,
                    exe_file: [0; 260],
                };
                let mut found = false;
                let ok = Process32FirstW(snap, &mut entry);
                if ok != 0 {
                    loop {
                        let end = entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260);
                        if String::from_utf16_lossy(&entry.exe_file[..end])
                            .eq_ignore_ascii_case("zcode.exe")
                        {
                            found = true;
                            break;
                        }
                        if Process32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap as *mut c_void);
                found
            }
        }

        pub fn io_write_bytes(h: &ProcHandle) -> Option<u64> {
            let mut io = IoCounters {
                read_ops: 0,
                write_ops: 0,
                other_ops: 0,
                read_bytes: 0,
                write_bytes: 0,
                other_bytes: 0,
            };
            unsafe {
                if GetProcessIoCounters(h.handle as *mut c_void, &mut io) != 0 {
                    Some(io.write_bytes)
                } else {
                    None
                }
            }
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn NtQueryInformationProcess(
                h: *mut c_void,
                class: u32,
                info: *mut c_void,
                len: u32,
                ret_len: *mut u32,
            ) -> i32;
        }

        /// 进程句柄：OpenProcess 打开的内核句柄（常驻复用，不逐拍开关）
        pub struct ProcHandle {
            handle: isize,
        }

        pub fn open_proc(pid: u32) -> Option<ProcHandle> {
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED, 0, pid) } as isize;
            if handle == 0 {
                None
            } else {
                Some(ProcHandle { handle })
            }
        }

        /// tracked 文件总量（rollout 目录全部 jsonl + CLI 日志目录 + db WAL，
        /// 落盘写入的尖峰来源），供清洗流做落盘扣除
        pub fn tracked_files_total() -> u64 {
            let mut total = 0u64;
            if let Some(home) = crate::metrics::home_dir() {
                for dir in [
                    home.join(".zcode/cli/rollout"),
                    home.join(".zcode/cli/log"),
                ] {
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        for e in rd.flatten() {
                            if let Ok(m) = e.metadata() {
                                total += m.len();
                            }
                        }
                    }
                }
                if let Ok(m) = std::fs::metadata(home.join(".zcode/cli/db/db.sqlite-wal")) {
                    total += m.len();
                }
            }
            total
        }
    }

    /// macOS：libproc 枚举进程（KERN_PROCARGS2 的 argv 含精确参数
    /// `zcode-cli`）+ proc_pid_rusage 的 ri_diskio_byteswritten（内核维护的
    /// 进程累计磁盘写字节，权威且读取零开销）
    #[cfg(target_os = "macos")]
    mod mac {
        use std::ffi::{c_int, c_void};

        // 链接名 "proc"（库文件为 /usr/lib/libproc.dylib，链接名不带 lib 前缀）
        #[link(name = "proc")]
        extern "C" {
            /// 注意 buffersize 单位是**字节**（不是 pid 个数），传 pid 容量 × 4
            fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
            fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        }
        #[link(name = "System")]
        extern "C" {
            fn sysctl(
                name: *const c_int,
                namelen: u32,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> c_int;
        }

        const CTL_KERN: c_int = 1;
        const KERN_PROCARGS2: c_int = 49;

        /// rusage_info_v4 逐字段镜像（对照 macOS SDK sys/resource.h）。
        /// 注意新内核布局在 ri_proc_start_abstime 之后有 ri_proc_exit_abstime，
        /// 它决定了 ri_diskio_byteswritten 的偏移——字段偏移由下方 const 断言
        /// 在编译期钉死，SDK 布局变化会直接编译失败，禁止删断言
        #[repr(C)]
        struct RusageInfoV4 {
            ri_uuid: [u8; 16],
            ri_user_time: u64,
            ri_system_time: u64,
            ri_pkg_idle_wkups: u64,
            ri_interrupt_wkups: u64,
            ri_pageins: u64,
            ri_wired_size: u64,
            ri_resident_size: u64,
            ri_phys_footprint: u64,
            ri_proc_start_abstime: u64,
            ri_proc_exit_abstime: u64,
            ri_child_user_time: u64,
            ri_child_system_time: u64,
            ri_child_pkg_idle_wkups: u64,
            ri_child_interrupt_wkups: u64,
            ri_child_pageins: u64,
            ri_child_elapsed_abstime: u64,
            ri_diskio_bytesread: u64,
            ri_diskio_byteswritten: u64,
            ri_cpu_time_qos_default: u64,
            ri_cpu_time_qos_maintenance: u64,
            ri_cpu_time_qos_background: u64,
            ri_cpu_time_qos_utility: u64,
            ri_cpu_time_qos_legacy: u64,
            ri_cpu_time_qos_user_initiated: u64,
            ri_cpu_time_qos_user_interactive: u64,
            ri_billed_system_time: u64,
            ri_serviced_system_time: u64,
            ri_logical_writes: u64,
            ri_lifetime_max_phys_footprint: u64,
            ri_instructions: u64,
            ri_cycles: u64,
            ri_billed_energy: u64,
            ri_serviced_energy: u64,
            ri_interval_max_phys_footprint: u64,
            ri_runnable_time: u64,
        }

        /// 编译期断言关键字段偏移与本机 SDK 头文件一致（C 程序实测 offsetof：
        /// ri_proc_start_abstime=80、ri_diskio_byteswritten=152、sizeof=296）；
        /// 断言不过必须修结构排布，禁止删断言
        const _: () = {
            assert!(std::mem::offset_of!(RusageInfoV4, ri_proc_start_abstime) == 80);
            assert!(std::mem::offset_of!(RusageInfoV4, ri_diskio_byteswritten) == 152);
            assert!(std::mem::size_of::<RusageInfoV4>() == 296);
        };

        const RUSAGE_INFO_V4: c_int = 4;

        /// 进程句柄：pid + 打开时抓取的进程启动时刻（绝对时间）。
        /// 采样时启动时刻不一致 = pid 已被复用，视为进程退出剔除
        pub struct ProcHandle {
            pid: u32,
            start_abstime: u64,
        }

        fn read_rusage(pid: u32) -> Option<RusageInfoV4> {
            // 512 字节缓冲 ≥ sizeof(RusageInfoV4)=296，容纳未来字段增长
            let mut buf = [0u8; 512];
            let ok = unsafe {
                proc_pid_rusage(pid as c_int, RUSAGE_INFO_V4, buf.as_mut_ptr().cast::<c_void>())
            };
            if ok != 0 {
                return None;
            }
            Some(unsafe { buf.as_ptr().cast::<RusageInfoV4>().read_unaligned() })
        }

        /// 枚举 ZCode CLI 进程（可多进程并存）。识别口径：KERN_PROCARGS2 的
        /// argv 中存在精确参数 "zcode-cli"（CLI 由 Electron Helper fork 而来，
        /// proc_pidpath 只能拿到 "ZCode Helper" 可执行路径，无法与其他 Helper
        /// 进程区分；实测 CLI 进程 argv[1] == "zcode-cli"）。
        /// proc_listallpids 两段式：先传 null 缓冲取 pid 数量，再取列表
        ///（buffersize 单位是字节）；m <= 0 视为失败返回空
        pub fn discover_cli_pids() -> Vec<u32> {
            let mut pids = Vec::new();
            unsafe {
                let n = proc_listallpids(std::ptr::null_mut(), 0);
                if n <= 0 {
                    return pids;
                }
                let cap = (n + 16) as usize;
                let mut buf = vec![0i32; cap];
                let m = proc_listallpids(buf.as_mut_ptr().cast::<c_void>(), (cap * 4) as c_int);
                if m <= 0 {
                    return pids;
                }
                let mut scratch = vec![0u8; 64 * 1024];
                for pid in &buf[..m as usize] {
                    if *pid > 0 && argv_has_cli_marker(*pid as u32, &mut scratch) {
                        pids.push(*pid as u32);
                    }
                }
            }
            pids
        }

        /// KERN_PROCARGS2 打包区中是否存在精确字符串 "zcode-cli"。
        /// 布局为 [nargs: i32][argv0 …（argv0 后有对齐 NUL 填充）argv1..][envp…]，
        /// 对齐填充与空参数难以区分，不精确重建 argv 边界——直接扫描全部
        /// NUL 结尾字符串做精确匹配（实测 CLI 进程的参数区有独立的
        /// "zcode-cli" 串；envp 串均为 KEY=VALUE 形式不会撞名；与 Windows
        /// 侧"命令行含 zcode.cjs"同宽口径）。64KB 覆盖常规进程；
        /// 解析失败/权限不足一律视为不匹配（不 panic）
        fn argv_has_cli_marker(pid: u32, buf: &mut [u8]) -> bool {
            let mib = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
            let mut len = buf.len();
            let ok = unsafe {
                sysctl(
                    mib.as_ptr(),
                    3,
                    buf.as_mut_ptr().cast::<c_void>(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ok != 0 || len < 4 {
                return false;
            }
            let mut pos = 4usize;
            while pos < len {
                while pos < len && buf[pos] == 0 {
                    pos += 1; // 跳过 NUL / 对齐填充
                }
                if pos >= len {
                    break;
                }
                let start = pos;
                while pos < len && buf[pos] != 0 {
                    pos += 1;
                }
                if &buf[start..pos] == b"zcode-cli" {
                    return true;
                }
            }
            false
        }

        /// 系统里是否存在任意 ZCode 进程（桌面端或 CLI 任一即算）。
        /// 供自动启动 follow 模式的待命检测用（autostart.rs）
        pub fn any_zcode_process() -> bool {
            unsafe {
                let n = proc_listallpids(std::ptr::null_mut(), 0);
                if n <= 0 {
                    return false;
                }
                let cap = (n + 16) as usize;
                let mut buf = vec![0i32; cap];
                let m = proc_listallpids(buf.as_mut_ptr().cast::<c_void>(), (cap * 4) as c_int);
                if m <= 0 {
                    return false;
                }
                let mut scratch = vec![0u8; 64 * 1024];
                for pid in &buf[..m as usize] {
                    if *pid > 0 && proc_is_zcode(*pid as u32, &mut scratch) {
                        return true;
                    }
                }
            }
            false
        }

        /// KERN_PROCARGS2 判定「ZCode 进程」：argv[0]（可执行路径，紧跟 4 字节
        /// nargs 头的第一个 NUL 结尾串）以 /ZCode 结尾 = 桌面端主进程；
        /// 参数区含精确串 zcode-cli = CLI 子进程（复用 discover 的口径）
        fn proc_is_zcode(pid: u32, buf: &mut [u8]) -> bool {
            let mib = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
            let mut len = buf.len();
            let ok = unsafe {
                sysctl(
                    mib.as_ptr(),
                    3,
                    buf.as_mut_ptr().cast::<c_void>(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ok != 0 || len < 4 {
                return false;
            }
            if let Some(end) = buf[4..len].iter().position(|b| *b == 0).map(|p| p + 4) {
                if buf[4..end].to_ascii_lowercase().ends_with(b"/zcode") {
                    return true;
                }
            }
            argv_has_cli_marker(pid, buf)
        }

        pub fn open_proc(pid: u32) -> Option<ProcHandle> {
            let ru = read_rusage(pid)?;
            Some(ProcHandle {
                pid,
                start_abstime: ru.ri_proc_start_abstime,
            })
        }

        /// 进程启动以来累计写字节（ri_diskio_byteswritten）。进程已退出或
        /// pid 被复用（start_abstime 变化）返回 None，由上层当拍剔除
        pub fn io_write_bytes(h: &ProcHandle) -> Option<u64> {
            let ru = read_rusage(h.pid)?;
            if ru.ri_proc_start_abstime != h.start_abstime {
                return None;
            }
            Some(ru.ri_diskio_byteswritten)
        }

        /// macOS 不做 tracked 文件扣除：实测 120s 探针中 rollout 目录 du 净变化
        /// 为负（CLI 清理轮转），负增量会反噬清洗流；且恒 0 免去每拍目录扫描
        pub fn tracked_files_total() -> u64 {
            0
        }
    }

    /// 其他平台：IO 探测不可用（面板回退到窗口/估算显示）
    #[cfg(not(any(windows, target_os = "macos")))]
    mod stub {
        pub struct ProcHandle;

        pub fn discover_cli_pids() -> Vec<u32> {
            Vec::new()
        }
        pub fn any_zcode_process() -> bool {
            false
        }
        pub fn open_proc(_pid: u32) -> Option<ProcHandle> {
            None
        }
        pub fn io_write_bytes(_h: &ProcHandle) -> Option<u64> {
            None
        }
        pub fn tracked_files_total() -> u64 {
            0
        }
    }

    #[cfg(windows)]
    pub use win::*;
    #[cfg(target_os = "macos")]
    pub use mac::*;
    #[cfg(not(any(windows, target_os = "macos")))]
    pub use stub::*;
}

pub struct LiveIo {
    procs: HashMap<u32, ProcRing>,
    last_refresh: Option<Instant>,
    /// (时刻 ms, tracked 文件累计字节)
    files_hist: VecDeque<(i64, u64)>,
    pending: VecDeque<Call>,
    cal: VecDeque<f64>,
    /// 清洗/校准参数（平台参数化，启动时锁定）
    params: CleanParams,
    bytes_per_token: f64,
    last_result: LiveNow,
    /// 会话 → 最近一次为其生成输出的 CLI 进程
    session_pid: HashMap<String, u32>,
    /// 最近一次清洗流速达到流式阈值的时刻（判停兜底的计时起点）
    last_stream_ms: Option<i64>,
    /// 当前关注的会话 = 最近完成调用的会话（仅作门控无调用基线时的判据种子）
    current_session: Option<String>,
    attributed: HashSet<String>,
    history_done: bool,
    /// 全部进行中的调用（Engine 由 message 表判定）：(会话, 调用开始时刻)，
    /// 多任务并发时实时速度按归属进程并集聚合
    inflight: Vec<(String, i64)>,
    /// 本段调用的幅度锚点（首个达到流式阈值的时刻，墙钟 ms）
    active_since: Option<i64>,
    /// 最近一次校准事件（供调试日志取用）
    pending_cal: Option<CalEvent>,
    /// 本进程生命周期内是否发现过 CLI 进程（区分"从未可用"与"已退出"）
    ever_saw_procs: bool,
}

impl LiveIo {
    pub fn new() -> Self {
        // 系数队列预置默认值为先验样本：冷启动阶段单个异常样本无法独占中位数，
        // 需要 2 个真实样本才能推动系数；满 5 个样本后先验自然被挤出
        let params = CleanParams::platform();
        let mut cal = VecDeque::with_capacity(5);
        cal.push_back(params.default_bpt);
        Self {
            procs: HashMap::new(),
            last_refresh: None,
            files_hist: VecDeque::new(),
            pending: VecDeque::new(),
            cal,
            params,
            bytes_per_token: params.default_bpt,
            last_result: LiveNow::default(),
            session_pid: HashMap::new(),
            last_stream_ms: None,
            current_session: None,
            attributed: HashSet::new(),
            history_done: false,
            inflight: Vec::new(),
            active_since: None,
            pending_cal: None,
            ever_saw_procs: false,
        }
    }

    /// 启动时注入今日已有调用，用于确定当前会话
    pub fn ingest_history(&mut self, calls: &[Call]) {
        if let Some(latest) = calls.iter().max_by_key(|c| c.completed_ms) {
            self.current_session = Some(latest.session.clone());
        }
        self.history_done = true;
    }

    pub fn history_done(&self) -> bool {
        self.history_done
    }

    pub fn observe(&mut self, new_calls: &[Call]) {
        for c in new_calls {
            self.pending.push_back(c.clone());
            // 最近完成调用的会话 = 当前关注的会话
            self.current_session = Some(c.session.clone());
        }
        while self.pending.len() > 8 {
            self.pending.pop_front();
        }
    }

    /// 每拍更新"调用进行中"信号（Engine 由 message 表与完成行比较得出；
    /// 多任务并发时为全部进行中会话）
    pub fn set_inflight(&mut self, inflight: Vec<(String, i64)>) {
        self.inflight = inflight;
    }

    /// 调试日志用：进行中会话的归属映射（会话 id → pid；无归属的会话省略）
    pub fn inflight_attr(&self) -> Vec<(String, u32)> {
        self.inflight
            .iter()
            .filter_map(|(s, _)| self.session_pid.get(s).map(|p| (s.clone(), *p)))
            .collect()
    }

    /// 当前生效的字节→token 系数（调试日志用）
    pub fn bytes_per_token(&self) -> f64 {
        self.bytes_per_token
    }

    /// 是否曾发现过 CLI 进程。区分"从未可用"（IO 探测不可用环境，允许估算回退）
    /// 与"发现过又全部退出"（CLI 已关闭，不应继续显示生成/估算）
    pub fn ever_saw_procs(&self) -> bool {
        self.ever_saw_procs
    }

    /// 取走最近一次校准事件（如有）
    pub fn take_calibration(&mut self) -> Option<CalEvent> {
        self.pending_cal.take()
    }

    /// 重新校准（当前速度卡手动按钮 / 轮均速漂移自动触发）：丢弃已学习的
    /// 系数样本，回到平台先验的冷启动状态（先验占位防单样本独占），由后续
    /// 完成调用的样本重新收敛。pending 调用保留——会话→进程归属仍需处理，
    /// 其携带的旧量级样本在滑动窗口下 1~2 轮即被新样本挤出。返回重置后系数
    pub fn reset_calibration(&mut self) -> f64 {
        self.cal.clear();
        self.cal.push_back(self.params.default_bpt);
        self.bytes_per_token = self.params.default_bpt;
        self.bytes_per_token
    }

    /// 导出系数样本队列（供持久化；重校准后为 [先验]，随队列变化落盘即可
    /// 让"重校准意图"跨重启保留）
    pub fn cal_state(&self) -> Vec<f64> {
        self.cal.iter().copied().collect()
    }

    /// 从持久化恢复系数样本：只收值域内的有限值，注入队列（容量与实时校准
    /// 一致，超出丢最旧），生效系数取恢复后队列的上中位数（与校准路径同
    /// 口径）。空/全非法时保持先验不动，返回实际接受数
    pub fn restore_cal(&mut self, samples: Vec<f64>) -> usize {
        let valid: Vec<f64> = samples
            .into_iter()
            .filter(|v| v.is_finite() && *v >= self.params.cal_min && *v <= self.params.cal_max)
            .collect();
        let n = valid.len();
        for v in valid {
            self.cal.push_back(v);
        }
        while self.cal.len() > CAL_QUEUE_CAP {
            self.cal.pop_front();
        }
        let mut sorted: Vec<f64> = self.cal.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        self.bytes_per_token = sorted[sorted.len() / 2];
        n
    }

    /// 每个轮询周期调用一次。now_ms 为墙钟毫秒（与 Engine 快照同源）
    pub fn measure(&mut self, now_ms: i64) -> LiveNow {
        let now = Instant::now();
        // 周期性刷新 CLI 进程集合；未发现任何进程、存在尚无归属记录的进行中
        // 会话（新开的第二个窗口——此时显示走全进程求和兜底，新进程没被发现
        // 的话读的是别的窗口的速度）、或 ≥2 个进行中会话（多任务并发，归属
        // 去重需要尽快看到新进程的字节分布）时缩短到 2s，而不是等满 30s
        let unattributed_inflight =
            !self.inflight.is_empty() && self.inflight.iter().any(|(s, _)| !self.session_pid.contains_key(s));
        let refresh_due = self
            .last_refresh
            .map_or(true, |t| now.duration_since(t) > REFRESH_EVERY);
        let quick_due = self
            .last_refresh
            .map_or(true, |t| now.duration_since(t) > Duration::from_secs(2));
        if refresh_due
            || ((self.procs.is_empty() || unattributed_inflight || self.inflight.len() >= 2) && quick_due)
        {
            self.last_refresh = Some(now);
            let found = platform::discover_cli_pids();
            self.procs.retain(|pid, _| found.contains(pid));
            for pid in found {
                if let Some(handle) = platform::open_proc(pid) {
                    self.procs.entry(pid).or_insert_with(|| ProcRing {
                        handle,
                        samples: VecDeque::new(),
                        min_delta: f64::MAX,
                    });
                }
            }
            if !self.procs.is_empty() {
                self.ever_saw_procs = true;
            }
        }

        // 采样本轮写字节与 tracked 文件总量（同拍成对，时间戳一致）
        self.procs.retain(|_, ring| {
            match platform::io_write_bytes(&ring.handle) {
                Some(w) => {
                    ring.samples.push_back((now_ms, w));
                    while ring.samples.len() > RING_CAP {
                        ring.samples.pop_front();
                    }
                    true
                }
                None => false, // 进程已退出
            }
        });
        // 同步清理失效 PID 对应的 session 映射，防止内存泄漏和 PID 复用脏归因
        let active_pids: HashSet<u32> = self.procs.keys().copied().collect();
        self.session_pid.retain(|_, pid| active_pids.contains(pid));
        if self.session_pid.len() > 200 {
            self.session_pid.clear();
        }
        let ft = platform::tracked_files_total();
        self.files_hist.push_back((now_ms, ft));
        while self.files_hist.len() > RING_CAP {
            self.files_hist.pop_front();
        }
        let files: Vec<(i64, u64)> = self.files_hist.iter().copied().collect();

        // 清洗后的拍序列（显示与校准共用同一条流，保证口径一致）
        let mut rows_by_pid: HashMap<u32, Vec<TickRow>> = HashMap::new();
        for (pid, ring) in self.procs.iter_mut() {
            let samples: Vec<(i64, u64)> = ring.samples.iter().copied().collect();
            // 每拍最小增量的低分位 × 2 作为该进程的心跳噪声底（字节/拍），
            // 样本太少时不用自适应底噪（避免冷启动吃掉起步信号）。
            // 使用时在 build_rows 内封顶，防持续流式期间被自身增量毒化
            let mut deltas: Vec<f64> = samples
                .windows(2)
                .map(|w| w[1].1.saturating_sub(w[0].1) as f64)
                .collect();
            if !deltas.is_empty() {
                deltas.sort_by(|a: &f64, b: &f64| a.partial_cmp(b).unwrap());
                ring.min_delta = if deltas.len() >= 20 {
                    deltas[deltas.len() / 10] * 2.0
                } else {
                    0.0
                };
            }
            rows_by_pid.insert(*pid, build_rows(&samples, ring.min_delta, &self.params));
        }

        // ---- 校准 + 会话→进程归属（调用完成后处理）----
        // 归属用原始字节（未清洗）：落盘错位/噪声扣除不影响"哪个进程在写"的判断。
        // 系数分子用与显示完全相同的清洗流在 [first_token, completed] 的积分，
        // 系统性扣除被系数抵消，显示收敛到真实 t/s
        while let Some(call) = self.pending.front().cloned() {
            if now_ms - call.completed_ms > 120_000 {
                self.pending.pop_front();
                continue;
            }
            // 延迟落盘宽限（mac）：磁盘写字节是页缓存异步落盘计数，completed 后
            // 等满 grace 再积分，让脏页进入计数。grace 期间留在队首；超 120s 的
            // 丢弃判定在前，保证不积压。Windows=0 时跳过本分支，当拍处理不变
            if self.params.cal_grace_ms > 0 && now_ms - call.completed_ms < self.params.cal_grace_ms
            {
                break;
            }
            let stream_start_ms = (call.completed_ms - call.gen_ms.min(300_000)).max(0);
            // 积分/统计窗口上限延长到 completed + grace：mac 的脏页滞后落盘，
            // 分子（clean）与分母口径（raw）同步放宽，pred 口径仍用真实 gen_ms
            let window_end_ms = call.completed_ms + self.params.cal_grace_ms;
            let mut raw_by_pid: HashMap<u32, u64> = HashMap::new();
            for (pid, ring) in self.procs.iter() {
                let mut acc = 0u64;
                for (a, b) in ring.samples.iter().zip(ring.samples.iter().skip(1)) {
                    if b.0 >= stream_start_ms && a.0 <= window_end_ms {
                        acc += b.1.saturating_sub(a.1);
                    }
                }
                raw_by_pid.insert(*pid, acc);
            }
            let raw_total = raw_by_pid.values().sum::<u64>() as f64;
            // 候选按字节降序、平局按 pid 升序（确定性），归属与事件记录共用
            let mut cands: Vec<(u32, u64)> = raw_by_pid.iter().map(|(p, b)| (*p, *b)).collect();
            cands.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let top_pid = cands.first().map(|(p, _)| *p);
            if !self.attributed.contains(&call.id) {
                self.attributed.insert(call.id.clone());
                if self.attributed.len() > 4_000 {
                    self.attributed.clear();
                }
                if raw_total > 20_000.0 {
                    // 其他进行中会话已占用的 pid（并发去重：平局时别往同一台挤；
                    // 两会话真实共享一台进程时次高只有噪声比例，不受影响）
                    let owned: HashSet<u32> = self
                        .inflight
                        .iter()
                        .filter(|(s, _)| s != &call.session)
                        .filter_map(|(s, _)| self.session_pid.get(s).copied())
                        .collect();
                    if let Some(pid) =
                        pick_attribution(self.session_pid.get(&call.session).copied(), &raw_by_pid, &owned)
                    {
                        self.session_pid.insert(call.session.clone(), pid);
                    }
                }
            }
            // 清洗积分：优先归属进程（与显示路径同一条聚合流，tracked 文件
            // 增长同样只扣一次），未归属时退化为全进程聚合
            let mut attr_pid = None;
            let clean_bytes = match self.session_pid.get(&call.session) {
                Some(pid) if rows_by_pid.contains_key(pid) => {
                    attr_pid = Some(*pid);
                    let streams = [&rows_by_pid[pid][..]];
                    integrate(
                        &merge_streams(&streams, &files),
                        stream_start_ms,
                        window_end_ms,
                    )
                    .0
                }
                _ => {
                    let streams: Vec<&[TickRow]> =
                        rows_by_pid.values().map(|v| &v[..]).collect();
                    integrate(
                        &merge_streams(&streams, &files),
                        stream_start_ms,
                        window_end_ms,
                    )
                    .0
                }
            };
            // 分子分母配对：clean 来自归属进程时，raw 基线也用归属进程的
            // （全进程 raw 会把他窗字节算进分母，clean/raw 守卫误判）。
            // 跨进程守卫基准：归属进程字节；无归属的全进程求和分支以 top 进程
            // 为基准——该分支的积分同样会混入他窗字节，不能放行
            let top_raw = top_pid
                .map_or(0.0, |p| *raw_by_pid.get(&p).unwrap_or(&0) as f64);
            let attr_raw = attr_pid
                .map_or(raw_total, |p| *raw_by_pid.get(&p).unwrap_or(&0) as f64);
            let guard_ref = attr_pid.map_or(top_raw, |_| attr_raw);
            let others_raw = (raw_total - guard_ref).max(0.0);
            let eff = call.effective_out();
            let true_tps = eff as f64 / (call.gen_ms.max(50) as f64 / 1000.0);
            let (mut in_cal, bpt_sample) =
                cal_sample(eff, clean_bytes, attr_raw, self.bytes_per_token, &self.params);
            if in_cal && !cross_pid_ok(others_raw, guard_ref) {
                in_cal = false;
            }
            if in_cal {
                self.bytes_per_token = median_bpt(&mut self.cal, bpt_sample, CAL_QUEUE_CAP);
            }
            self.pending_cal = Some(CalEvent {
                id: call.id.clone(),
                session: call.session.clone(),
                completed_ms: call.completed_ms,
                true_tps,
                gen_ms: call.gen_ms,
                eff,
                raw_bytes: raw_total,
                clean_bytes,
                bpt_sample: if in_cal { bpt_sample } else { 0.0 },
                bpt_now: self.bytes_per_token,
                cal_skipped: !in_cal,
                attr_pid,
                top_pid,
                others_bytes: others_raw,
            });
            self.pending.pop_front();
        }

        // 显示进程集合：进行中会话全部有存活归属 → 归属 pid 并集（多任务并发
        // 聚合为真实总吞吐）；任一会话尚无归属（新会话/子代理首个调用未完成过）
        // → 全进程求和兜底，避免漏掉尚无归属记录的新进程、显示成别的窗口的速度
        let pid_set = pick_pid_set(&self.inflight, &self.session_pid, &active_pids);
        // 聚合流：集合上逐拍字节求和 + tracked 文件增长整体只扣一次（时长也只
        // 计一次——逐进程分别积分会把墙钟求和，速率被摊薄为跨进程均值）
        let display_rows: Vec<TickRow> = {
            let streams: Vec<&[TickRow]> = match &pid_set {
                Some(set) => set
                    .iter()
                    .filter_map(|p| rows_by_pid.get(p).map(|v| &v[..]))
                    .collect(),
                None => rows_by_pid.values().map(|v| &v[..]).collect(),
            };
            merge_streams(&streams, &files)
        };
        let span = |from_ms: i64| -> (f64, f64) { integrate(&display_rows, from_ms, now_ms) };

        // 启停判定（调用门控）：message 表的 assistant 行在调用开始瞬间提交，
        // 行内 data 的 time.completed 在结束（含取消/出错）瞬间补写——门控直接
        // 信任该信号且不限会话（新开对话的首个调用当拍即亮，无需等首个完成行），
        // 结束/取消当拍归零。工具执行/待机期间管道同样有 UI 状态突发，门控
        // 将其可靠排除。进程守卫：进行中会话的归属进程全部退出（崩溃/关终端后
        // completed 无人补写）时强制判停，不留僵尸"生成中"；无归属的会话不判死。
        // 门控不可用时（尚无任何调用做基线）退化为纯字节判定。
        let proc_gone = {
            let mut any_alive = false;
            for (s, _) in &self.inflight {
                match self.session_pid.get(s) {
                    Some(pid) if self.procs.contains_key(pid) => any_alive = true,
                    Some(_) => {}
                    None => any_alive = true,
                }
            }
            !self.inflight.is_empty() && !any_alive
        };
        let (det_b, det_s) = span(now_ms - self.params.detect_ms);
        let detect_bps = if det_s > 0.0 { det_b / det_s } else { 0.0 };
        let gate_on = !proc_gone
            && (!self.inflight.is_empty()
                || (self.current_session.is_none() && detect_bps > STREAMING_BPS));
        if gate_on {
            // 幅度锚点：本段流式内首个清洗流速达到流式阈值的时刻。断流后复流
            // （判停兜底触发过/聚合段切换）重新起锚——30s 滑窗从新一段起算，
            // 不把静默段摊进来稀释读数
            if detect_bps > STREAMING_BPS {
                if !self.last_result.streaming || self.active_since.is_none() {
                    self.active_since = Some(now_ms);
                }
                // 判停兜底计时：清洗流速仍在流式阈值上时持续续期
                self.last_stream_ms = Some(now_ms);
            }
        } else {
            self.active_since = None;
            self.last_stream_ms = None;
        }

        // 幅度：30s 滑窗 ∩ [首字节拍, now] 的清洗流积分。首字节当拍即有真实读数
        // （此前为 TTFT，显示"统计中"）；稳态覆盖满 30s（平滑）；停止当拍归零。
        // 落盘 flush 错位产生的负拍在窗口内对消，仅在汇总处钳非负
        let pipe_bps = if gate_on {
            match self.active_since {
                Some(anchor) => {
                    let from = anchor.max(now_ms - WINDOW_MS);
                    let (b, s) = span(from);
                    if s > 0.0 {
                        (b / s).max(0.0)
                    } else {
                        0.0
                    }
                }
                None => 0.0, // 首字节未到（TTFT），显示统计中
            }
        } else {
            0.0
        };
        // 判停兜底：门控仍开（completed 未落盘）但锚点后清洗流断绝超过宽限 →
        // 生成实际已停，归零显示；上层 streaming=false 走 idle 分支，不再用
        // window 回退挂着"生成中"。字节恢复当拍自愈（计时随流刷新）
        let streaming =
            gate_on && !stale_stop(self.active_since, self.last_stream_ms, now_ms);
        let ramping = streaming
            && (self.active_since.is_none()
                || self
                    .active_since
                    .map_or(false, |a| now_ms - a < WINDOW_MS));
        // 启动提示：门控已开但首字节未到（TTFT，取最新开始的会话），限制在
        // 提示窗口内——窗口内显示"统计中…"，超窗仍无字节则由上层回退到估算
        // （管道静默调用）
        let awaiting = streaming
            && awaiting_hint(
                self.inflight.iter().map(|(_, t)| *t).max(),
                self.active_since,
                now_ms,
            );

        // 分任务明细：显示集合内每个进程在聚合窗口内的清洗速率。文件增长按
        // 各进程**正**字节占比分摊（负拍进程不参与分摊，其自身数值被钳 0，
        // 否则明细之和会大于聚合读数）；门控关闭/未流式/启动期（TTFT）为空
        let mut tasks: Vec<TaskLive> = Vec::new();
        let mut active_pid_count = 0usize;
        if streaming && !awaiting {
            let from = self
                .active_since
                .map_or(now_ms, |a| a.max(now_ms - WINDOW_MS));
            let (_, wall_s) = integrate(&display_rows, from, now_ms);
            let fg_w = integrate(&file_growth_rows(&files), from, now_ms).0;
            let pids: Vec<u32> = match &pid_set {
                Some(set) => set.clone(),
                None => rows_by_pid.keys().copied().collect(),
            };
            let mut per: Vec<(u32, f64, bool)> = Vec::with_capacity(pids.len());
            let mut sum_pos = 0.0;
            for pid in &pids {
                let Some(rows) = rows_by_pid.get(pid) else { continue };
                let (b, _) = integrate(rows, from, now_ms);
                sum_pos += b.max(0.0);
                let (db, ds) = integrate(rows, now_ms - self.params.detect_ms, now_ms);
                let dbps = if ds > 0.0 { db / ds } else { 0.0 };
                per.push((*pid, b, dbps > STREAMING_BPS));
            }
            // 窗口内贡献达到流式量级的进程数（漂移检测的"单进程轮"判据；
            // 空闲进程的底噪泄漏不计入）
            active_pid_count = per
                .iter()
                .filter(|(_, b, _)| *b > STREAMING_BPS * wall_s)
                .count();
            for (pid, b, is_stream) in per {
                let share = if sum_pos > 0.0 { b.max(0.0) / sum_pos } else { 0.0 };
                let tps = if wall_s > 0.0 && self.bytes_per_token > 0.0 {
                    ((b - fg_w * share) / wall_s).max(0.0) / self.bytes_per_token
                } else {
                    0.0
                };
                tasks.push(TaskLive {
                    pid,
                    session: None,
                    n_sessions: 0,
                    tps,
                    streaming: is_stream,
                });
            }
            // 会话标签与计数：进行中会话按归属填到对应进程；同进程多会话
            // （同一 ZCode 窗口新开任务复用 app-server）如实计为 n_sessions，
            // 速度为该进程合计——字节层无法拆分，标签取首个会话
            for (s, _) in &self.inflight {
                if let Some(pid) = self.session_pid.get(s) {
                    if let Some(t) = tasks.iter_mut().find(|t| t.pid == *pid) {
                        t.n_sessions += 1;
                        if t.session.is_none() {
                            t.session = Some(s.clone());
                        }
                    }
                }
            }
            // 空闲且无归属会话的进程不占明细位；按速度降序稳定排列
            tasks.retain(|t| t.streaming || t.session.is_some());
            tasks.sort_by(|a, b| b.tps.partial_cmp(&a.tps).unwrap_or(std::cmp::Ordering::Equal));
        }
        let n_pids = if streaming && !awaiting {
            active_pid_count
        } else {
            0
        };
        // 诊断：每台被跟踪进程的探测窗清洗速率（KB/s，tick 调试日志用）
        let proc_bps: Vec<(u32, f64)> = rows_by_pid
            .iter()
            .map(|(pid, rows)| {
                let (db, ds) = integrate(rows, now_ms - self.params.detect_ms, now_ms);
                (*pid, if ds > 0.0 { (db / ds / 1024.0 * 10.0).round() / 10.0 } else { 0.0 })
            })
            .collect();

        let result = LiveNow {
            available: !self.procs.is_empty(),
            streaming,
            ramping,
            awaiting,
            tps: if streaming {
                pipe_bps / self.bytes_per_token
            } else {
                0.0
            },
            pipe_bps,
            n_pids,
            tasks,
            proc_bps,
        };
        self.last_result = result.clone();
        result
    }
}

// ============ 测试 ============
#[cfg(test)]
mod tests {
    use super::*;

    const TICK: i64 = 700;

    /// 构造 (时刻, 累计字节) 采样序列：每拍增量由 deltas 给出
    fn series(start_ms: i64, deltas: &[f64]) -> Vec<(i64, u64)> {
        let mut out = Vec::with_capacity(deltas.len() + 1);
        let mut acc = 0u64;
        out.push((start_ms, acc));
        for (i, d) in deltas.iter().enumerate() {
            acc += *d as u64;
            out.push((start_ms + (i as i64 + 1) * TICK, acc));
        }
        out
    }

    #[test]
    fn burst_tick_dropped_entirely() {
        // 190KB 请求体突发拍被整拍丢弃（字节与时长都不进积分）；52KB 真实流式拍保留
        let s = series(0, &[190_000.0, 52_000.0, 52_000.0]);
        let rows = build_rows(&s, 0.0, &CleanParams::windows());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.bytes < 52_000.0));
        // 突发拍连时长都不贡献：两个保留拍的 dt 合计 1.4s
        let secs = integrate(&rows, i64::MIN, i64::MAX).1;
        assert!((secs - 1.4).abs() < 1e-9);
    }

    #[test]
    fn floor_capped_against_ring_poisoning() {
        // 自适应底噪被毒化到 50KB/拍（持续流式期间分位数抬高），
        // 封顶后每拍最多扣 FLOOR_CAP + BASE_NOISE×dt，52KB 流式拍仍保留大头
        let s = series(0, &[52_000.0; 4]);
        let rows = build_rows(&s, 50_000.0, &CleanParams::windows());
        let floor = FLOOR_CAP_BYTES + BASE_NOISE_BPS * (TICK as f64 / 1000.0);
        for r in &rows {
            assert!((r.bytes - (52_000.0 - floor)).abs() < 1e-6);
        }
    }

    /// 聚合流：文件增长整体只扣一次（多进程求和时逐进程各扣一遍会扣 N 倍），
    /// 墙钟时长也只计一次（逐进程分别积分会把时长求和，速率被摊薄为均值）
    #[test]
    fn merge_streams_deducts_file_growth_once_and_counts_secs_once() {
        // 两进程同拍各写 52KB、文件每拍增长 10KB
        let a = series(0, &[52_000.0; 4]);
        let b = series(0, &[52_000.0; 4]);
        let files: Vec<(i64, u64)> = (0..5)
            .map(|i| (a[0].0 + i as i64 * TICK, i as u64 * 10_000))
            .collect();
        let rows_a = build_rows(&a, 0.0, &CleanParams::windows());
        let rows_b = build_rows(&b, 0.0, &CleanParams::windows());
        let merged = merge_streams(&[&rows_a, &rows_b], &files);
        let floor = BASE_NOISE_BPS * (TICK as f64 / 1000.0);
        // 单拍 = 2×(52_000 − floor) − 10_000（文件只扣一次），而不是
        // 2×(52_000 − floor − 10_000)
        for r in &merged {
            assert!((r.bytes - (2.0 * (52_000.0 - floor) - 10_000.0)).abs() < 1e-6);
        }
        // 时长只计一次：4 拍 = 2.8s（逐进程分别积分再求和会得到 5.6s）
        let (b_sum, secs) = integrate(&merged, i64::MIN, i64::MAX);
        assert!((secs - 2.8).abs() < 1e-9, "墙钟时长应只计一次: {secs}");
        // 字节总和 = 2 进程 × 4 拍 × (52_000−floor) − 4 拍文件增长（各扣一次）
        assert!((b_sum - (8.0 * (52_000.0 - floor) - 40_000.0)).abs() < 1e-6);
        // 单条流输入与"逐行扣文件"的历史算术等价：flush 错位拍为负、区间对消
        let s = series(0, &[20_000.0, 60_000.0, 20_000.0]);
        let flush_files = vec![
            (s[0].0, 0u64),
            (s[1].0, 0u64),
            (s[2].0, 60_000u64),
            (s[3].0, 60_000u64),
        ];
        let single = merge_streams(&[&build_rows(&s, 0.0, &CleanParams::windows())], &flush_files);
        assert!(single[1].bytes < 0.0, "flush 拍应为负: {}", single[1].bytes);
        let (b2, _) = integrate(&single, i64::MIN, i64::MAX);
        assert!((b2 - (100_000.0 - 60_000.0 - 3.0 * floor)).abs() < 1e-6);
    }

    /// 显示进程集合：全部进行中会话有存活归属 → 归属 pid 并集；
    /// 任一会话无归属（新会话/子代理首个调用未完成过）→ None = 全进程求和兜底
    #[test]
    fn pick_pid_set_union_and_fallback() {
        let mut sp = HashMap::new();
        sp.insert("a".to_string(), 1u32);
        sp.insert("b".to_string(), 2u32);
        let active: HashSet<u32> = [1u32, 2u32, 3u32].into_iter().collect();
        // 两会话各自归属 → 并集（去重、升序）
        let inflight = vec![("b".to_string(), 5i64), ("a".to_string(), 3i64)];
        assert_eq!(
            pick_pid_set(&inflight, &sp, &active),
            Some(vec![1u32, 2u32])
        );
        // 两会话归属同一进程（主会话与其子代理）→ 单元素集合
        sp.insert("c".to_string(), 1u32);
        let inflight = vec![("a".to_string(), 3i64), ("c".to_string(), 9i64)];
        assert_eq!(pick_pid_set(&inflight, &sp, &active), Some(vec![1u32]));
        // 新会话无归属 → 兜底全进程求和（不能漏掉它自己的进程）
        let inflight = vec![("a".to_string(), 3i64), ("new".to_string(), 9i64)];
        assert_eq!(pick_pid_set(&inflight, &sp, &active), None);
        // 归属进程已死 → 同样兜底
        let dead: HashSet<u32> = [2u32].into_iter().collect();
        let inflight = vec![("a".to_string(), 3i64)];
        assert_eq!(pick_pid_set(&inflight, &sp, &dead), None);
        // 无进行中会话 → None（门控关闭）
        assert_eq!(pick_pid_set(&[], &sp, &active), None);
    }

    /// 归属切换迟滞：已有归属时仅当候选进程窗口内字节 ≥ 2 倍才切换，
    /// 杜绝并发窗口间逐调用翻转（2026-09-18 现场：同会话相邻调用归属摆动，
    /// 系数被污染在 262~764 间跳、读数偏差 2~3 倍）
    #[test]
    fn should_reattribute_hysteresis() {
        let raws: HashMap<u32, u64> = [(1u32, 100_000u64), (2u32, 150_000u64), (3u32, 300_000u64)]
            .into_iter()
            .collect();
        // 无现归属 → 直接采信 top
        assert_eq!(should_reattribute(None, Some(2), &raws), Some(2));
        // top 与现归属相同 → 不变
        assert_eq!(should_reattribute(Some(1), Some(1), &raws), Some(1));
        // top 只有 1.5 倍（不足迟滞）→ 保持现归属，不翻转
        assert_eq!(should_reattribute(Some(1), Some(2), &raws), Some(1));
        // top 达 3 倍（≥2 倍迟滞）→ 切换
        assert_eq!(should_reattribute(Some(1), Some(3), &raws), Some(3));
        // 现归属窗口内零字节（归属过期）→ 自愈切换到 top
        let raws0: HashMap<u32, u64> = [(9u32, 0u64), (3u32, 300_000u64)].into_iter().collect();
        assert_eq!(should_reattribute(Some(9), Some(3), &raws0), Some(3));
        // 无 top（全零字节）→ 不动
        assert_eq!(should_reattribute(Some(1), None, &raws), None);
    }

    /// 归属去重（并发平局）：首次归属时 top 已被另一进行中会话占用、次高
    /// 字节达一半 → 改归次高，两会话不挤到同一台进程（显示集合塌缩成单进程）；
    /// 次高只有噪声比例（真实共享一台 app-server 进程）→ 维持 top。
    /// 2026-09-18 现场：同一 ZCode 窗口新开任务复用同一 app-server（others 仅
    /// ~0.17 比例噪声），跨项目任务分到不同 app-server（并发平局 ~1）
    #[test]
    fn pick_attribution_dedup_on_tie() {
        // 首次归属，top(1) 被占用，次高(2) 字节 95% → 归 2
        let raws: HashMap<u32, u64> = [(1u32, 4_000_000u64), (2u32, 3_800_000u64)]
            .into_iter()
            .collect();
        let owned: HashSet<u32> = [1u32].into_iter().collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(2));
        // 次高只有噪声比例（~0.17）→ 维持共享 top
        let raws: HashMap<u32, u64> = [(1u32, 4_000_000u64), (2u32, 700_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(1));
        // 平局且字节相同 → 字节降序平局按 pid 升序，top=1 被占用 → 归 2
        let raws: HashMap<u32, u64> = [(2u32, 4_000_000u64), (1u32, 4_000_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(2));
        // top 未被占用 → 直接归 top（无去重介入）
        let no_owned: HashSet<u32> = HashSet::new();
        assert_eq!(pick_attribution(None, &raws, &no_owned), Some(1));
        // 次高字节太小（<20KB 门槛）→ 维持 top
        let raws: HashMap<u32, u64> = [(1u32, 4_000_000u64), (2u32, 30_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(None, &raws, &owned), Some(1));
    }

    /// 归属去重（占用自愈）：现归属被另一进行中会话占用时，top 未被占用且
    /// 字节达现归属一半即可切换（被占用场景 2 倍迟滞会把历史错归锁死）；
    /// 现归属未被占用 → 维持 2 倍迟滞原语义
    #[test]
    fn pick_attribution_relaxed_switch_when_owned() {
        let owned: HashSet<u32> = [1u32].into_iter().collect();
        // 现归属 1 被占用，top 2（窗口内字节最高）未被占用且达 95% → 切换
        //（原 2 倍迟滞会把这类历史错归锁死）
        let raws: HashMap<u32, u64> = [(2u32, 4_000_000u64), (1u32, 3_800_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(Some(1), &raws, &owned), Some(2));
        // top 不足现归属的一半 → 维持
        let raws: HashMap<u32, u64> = [(2u32, 1_900_000u64), (1u32, 4_000_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(Some(1), &raws, &owned), Some(1));
        // 现归属未被占用 → 原 2 倍迟滞（top 95% 不足 2 倍，不切换）
        let no_owned: HashSet<u32> = HashSet::new();
        let raws: HashMap<u32, u64> = [(2u32, 4_000_000u64), (1u32, 3_800_000u64)]
            .into_iter()
            .collect();
        assert_eq!(pick_attribution(Some(1), &raws, &no_owned), Some(1));
        // top 也被占用（两个 pid 都有主）→ 无处可去，维持现归属
        let both_owned: HashSet<u32> = [1u32, 2u32].into_iter().collect();
        assert_eq!(pick_attribution(Some(1), &raws, &both_owned), Some(1));
    }

    /// 校准样本跨进程守卫：窗口内其他进程字节 ≤ 归属进程的 20% 才入样
    #[test]
    fn cross_pid_guard_thresholds() {
        // 只有归属进程写字节 → 通过
        assert!(cross_pid_ok(0.0, 500_000.0));
        // 其他进程恰好 20% → 通过（边界含）
        assert!(cross_pid_ok(100_000.0, 500_000.0));
        // 超过 20% → 拒收（对方窗口并发流式）
        assert!(!cross_pid_ok(100_001.0, 500_000.0));
        // 归属进程零字节 → 拒收（无基准可配对）
        assert!(!cross_pid_ok(0.0, 0.0));
    }

    #[test]
    fn integrate_prorates_boundary_ticks() {
        let rows = vec![TickRow { dt_ms: 1000, bytes: 1000.0, end_ms: 10_000 }];
        // 只取该拍的后半 [9_500, 10_000]：字节与时长各计一半
        let (b, s) = integrate(&rows, 9_500, 10_000);
        assert!((b - 500.0).abs() < 1e-9);
        assert!((s - 0.5).abs() < 1e-9);
        // 完全不重叠的区间不贡献
        let (b, _) = integrate(&rows, 10_500, 11_000);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn median_bpt_sliding() {
        let mut s = VecDeque::new();
        assert!((median_bpt(&mut s, 600.0, 5) - 600.0).abs() < 1e-9);
        median_bpt(&mut s, 800.0, 5);
        median_bpt(&mut s, 400.0, 5);
        assert_eq!(s.len(), 3);
        // 奇数个样本取正中；偶数个取上中位元素（[400,500,600,800] → 600）
        assert!((median_bpt(&mut s, 500.0, 5) - 600.0).abs() < 1e-9);
    }

    #[test]
    fn prior_sample_prevents_single_sample_takeover() {
        // 冷启动：预置先验后，单个异常样本（如 bpt=179）不能独占系数
        let mut s = VecDeque::new();
        s.push_back(DEFAULT_BPT);
        let b1 = median_bpt(&mut s, 179.0, 5);
        assert!((b1 - DEFAULT_BPT).abs() < 1e-9, "单个样本不应撼动先验: {b1}");
        // 两个真实样本开始推动中位数（[179, 552, 600] → 552）
        let b2 = median_bpt(&mut s, 552.0, 5);
        assert!((b2 - 552.0).abs() < 1e-9);
    }

    #[test]
    fn awaiting_hint_window() {
        // 门控开启、首字节未到：提示窗口（20s）内为 true
        assert!(awaiting_hint(Some(1_000), None, 5_000));
        assert!(awaiting_hint(Some(1_000), None, 21_000));
        // 超窗 → 不再提示（上层回退估算：管道静默调用）
        assert!(!awaiting_hint(Some(1_000), None, 21_001));
        // 已有首字节锚点 → 不是启动期
        assert!(!awaiting_hint(Some(1_000), Some(2_000), 5_000));
        // 无进行中调用 → 不提示
        assert!(!awaiting_hint(None, None, 5_000));
        assert!(!awaiting_hint(None, Some(1_000), 5_000));
    }

    /// 判停兜底：门控开着（completed 未落盘）但锚点后清洗流断绝超宽限 → 判停。
    /// 现场实例（2026-09-17 日志）：调用已停、completed 落盘前窗口回退持续
    /// 挂"生成中 + ≈上轮速度"，用户观感"停了还在生成、慢慢降"
    #[test]
    fn stale_stop_after_silent_window() {
        // 锚点已建立、最近流时刻距今未超宽限 → 仍流式
        assert!(!stale_stop(Some(1_000), Some(16_000), 16_000));
        // 断绝恰好 15s → 未超（> 判定），仍流式
        assert!(!stale_stop(Some(1_000), Some(1_000), 16_000));
        // 断绝超 15s → 判停
        assert!(stale_stop(Some(1_000), Some(1_000), 16_001));
        // 锚点未建立（管道静默调用，全程无字节）→ 永不判停，由 window 回退服务
        assert!(!stale_stop(None, None, 100_000));
        assert!(!stale_stop(None, Some(1_000), 100_000));
        // 锚点在但从未记录到流时刻（理论不达：锚点建立即有流）→ 不判停
        assert!(!stale_stop(Some(1_000), None, 100_000));
    }

    /// 轮均速漂移：上轮均值 vs 之前连续 5 轮均值，双向 ≥3 倍触发重校准
    #[test]
    fn round_drift_triggers_on_threefold_jump() {
        let mut d = RoundDrift::new();
        for v in [40.0, 42.0, 38.0, 41.0, 39.0] {
            assert!(d.observe(v).is_none(), "基线积累期不应触发");
        }
        // 上轮 120 = 基线均值 40 的整 3 倍 → 触发，返回基线供日志
        let base = d.observe(120.0).expect("3 倍上跳应触发");
        assert!((base - 40.0).abs() < 1e-9);
        // 触发后历史清空：同量级下一轮不再触发
        assert!(d.observe(120.0).is_none());
    }

    #[test]
    fn round_drift_needs_five_round_history() {
        let mut d = RoundDrift::new();
        for _ in 0..4 {
            assert!(d.observe(40.0).is_none());
        }
        // 历史不足 5 轮：再极端的上跳也不触发，该轮照常入历史
        assert!(d.observe(4_000.0).is_none());
        // 凑满 5 轮后，混合基线（4×40 + 4000 = 832）与旧量级 40 差异仍超 3 倍
        assert!(d.observe(40.0).is_some());
    }

    /// 反向（换更快模型后回看，或快→慢）：基线 90 vs 上轮 30 = 1/3 → 同样触发
    #[test]
    fn round_drift_downward_jump_triggers() {
        let mut d = RoundDrift::new();
        for _ in 0..5 {
            assert!(d.observe(90.0).is_none());
        }
        assert!(d.observe(30.0).is_some());
    }

    /// 2.5 倍以内的正常波动不触发；滑窗只保留最近 5 轮，旧量级被自然挤出
    #[test]
    fn round_drift_moderate_change_and_window_cap() {
        let mut d = RoundDrift::new();
        for _ in 0..5 {
            assert!(d.observe(40.0).is_none());
        }
        assert!(d.observe(100.0).is_none(), "2.5 倍上跳不应触发");
        for _ in 0..5 {
            assert!(d.observe(100.0).is_none());
        }
        // 基线已全为 100，回跳 40 恰 2.5 倍 → 不触发
        assert!(d.observe(40.0).is_none());
    }

    /// 无实测拍的静默轮（均值 0）不参与检测、不污染基线
    #[test]
    fn round_drift_ignores_zero_round() {
        let mut d = RoundDrift::new();
        for v in [50.0, 0.0, 50.0, 0.0, 50.0, 0.0, 50.0] {
            assert!(d.observe(v).is_none());
        }
        // 4 个 50 入历史（0 全被忽略），第 5 个 50 凑满基线不触发
        assert!(d.observe(50.0).is_none());
        assert!(d.observe(200.0).is_some());
    }

    /// 重新校准：系数与样本队列回到平台先验（冷启动状态）
    #[test]
    fn reset_calibration_restores_prior() {
        let mut io = LiveIo::new();
        io.cal.clear();
        io.cal.extend([420.0, 380.0, 455.0]);
        io.bytes_per_token = 420.0;
        let bpt = io.reset_calibration();
        assert!((bpt - io.params.default_bpt).abs() < 1e-9);
        assert_eq!(io.cal.len(), 1, "队列应只余先验占位");
        assert!((io.bytes_per_token - io.params.default_bpt).abs() < 1e-9);
    }

    /// 重启恢复：越界/非有限值拒收，生效系数按恢复后队列的上中位数重算
    /// （与校准路径同口径）；空恢复不动先验
    #[test]
    fn restore_cal_filters_and_recomputes_median() {
        let mut io = LiveIo::new();
        // 30 越下界、99999 越上界（两平台 CAL_MAX 上界之上）、NaN 非有限 → 拒；
        // 500/540 入队得 [先验,500,540]
        let n = io.restore_cal(vec![500.0, 540.0, 30.0, 99_999.0, f64::NAN]);
        assert_eq!(n, 2);
        assert!((io.bytes_per_token() - 540.0).abs() < 1e-9);
        // 空恢复不动状态
        assert_eq!(io.restore_cal(vec![]), 0);
        assert!((io.bytes_per_token() - 540.0).abs() < 1e-9);
        // 容量挤出：再注入 5 个合法值，队列保最新 5 个（600/500/540 被挤出）
        let q0 = io.cal_state();
        assert_eq!(q0.len(), 3);
        io.restore_cal(vec![450.0, 460.0, 470.0, 480.0, 490.0]);
        assert_eq!(io.cal_state().len(), CAL_QUEUE_CAP);
        assert!(!io.cal_state().contains(&io.params.default_bpt));
        // [450,460,470,480,490] 上中位 = 470
        assert!((io.bytes_per_token() - 470.0).abs() < 1e-9);
    }

    /// 样本准入用例取自真实调试日志（2026-09-17 现场）：
    /// 管道静默调用会产生 ~7 B/token 的垃圾样本，必须整条拒绝而不是钳位后入队
    #[test]
    fn cal_sample_rejects_silent_pipe_calls() {
        // bpt_now 传 Windows 先验（离群拒绝在该平台禁用，取值不影响结果）
        // 静默调用：887 token，管道积分仅 5.9KB，原始字节 ~1MB → 拒绝
        let (ok, _) = cal_sample(887, 5_939.0, 1_048_576.0, DEFAULT_BPT, &CleanParams::windows());
        assert!(!ok);
        // 正常调用：1028 token，清洗 526.5KB / 原始 ~900KB → 接受，样本 ≈524
        let (ok, v) = cal_sample(
            1028,
            526.5 * 1024.0,
            900.0 * 1024.0,
            DEFAULT_BPT,
            &CleanParams::windows(),
        );
        assert!(ok);
        assert!((v - 524.0).abs() < 15.0);
        // 小调用：64 token → 拒绝
        assert!(!cal_sample(64, 50_000.0, 80_000.0, DEFAULT_BPT, &CleanParams::windows()).0);
        // 偏瘦但真实：449 token，清洗 67.5KB（比例 ~150 B/token，占原始 52%）→ 接受
        let (ok, v) = cal_sample(
            449,
            67.5 * 1024.0,
            130.0 * 1024.0,
            DEFAULT_BPT,
            &CleanParams::windows(),
        );
        assert!(ok);
        assert!((v - 150.0).abs() < 5.0);
        // 超界比例（>6000）→ 拒绝
        assert!(!cal_sample(
            500,
            500.0 * 6000.0 * 1.1,
            500.0 * 6000.0 * 1.2,
            DEFAULT_BPT,
            &CleanParams::windows()
        )
        .0);
    }

    /// mac 参数字面量（与 `CleanParams::macos()` 保持同值；字面量构造保证
    /// Windows 上测试也能编译运行）
    fn mac_params() -> CleanParams {
        CleanParams {
            burst_tick_bytes: u64::MAX as f64,
            base_noise_bps: 0.0,
            floor_cap_bytes: FLOOR_CAP_BYTES,
            cal_min: CAL_MIN,
            cal_max: 12_000.0,
            cal_min_tokens: CAL_MIN_TOKENS,
            default_bpt: 700.0,
            detect_ms: DETECT_MS,
            cal_grace_ms: 15_000,
            cal_outlier_ratio: 3.0,
        }
    }

    /// mac 离群拒绝（2026-09-17 对账实测）：延迟落盘的半截样本（34s 调用只
    /// 积分到一半字节 → 186 B/token）与生效系数 700 偏差超 3 倍边界即拒收，
    /// 不进中位数；正常样本（真值 614/724 一带）照常接受
    #[test]
    fn mac_outlier_sample_rejected() {
        let mac = mac_params();
        // 正常样本 ≈650 B/token，落在 [700/3, 700×3] → 接受
        let (ok, v) = cal_sample(1_000, 650_000.0, 900_000.0, 700.0, &mac);
        assert!(ok);
        assert!((v - 650.0).abs() < 1e-6);
        // 半截样本 186（>cal_min=100、clean/raw=62%，既有检查全过）：
        // 186 < 700/3≈233 → 离群拒收，样本记 0
        let (ok, v) = cal_sample(1_000, 186_000.0, 300_000.0, 700.0, &mac);
        assert!(!ok);
        assert_eq!(v, 0.0);
        // 偏高离群：2500 > 700×3=2100（仍在 cal_max=12000 内）→ 拒收
        assert!(!cal_sample(1_000, 2_500_000.0, 3_000_000.0, 700.0, &mac).0);
        // 同样的半截样本在 Windows（ratio=0 禁用）不拒收，与既有行为等价
        assert!(cal_sample(1_000, 186_000.0, 300_000.0, DEFAULT_BPT, &CleanParams::windows()).0);
    }

    /// mac 延迟落盘宽限：磁盘写字节是页缓存异步落盘计数，write() 后数秒~
    /// 数十秒才计入（实测 117s 长调用 96% 字节落在 completed 之后，用户盯着
    /// 0.7 t/s 两分钟而真值 65.3）。grace=15s 把校准积分窗口延长到
    /// completed+15s，滞后字节进入分子、样本恢复真值；Windows 口径的
    /// [.., completed] 窗口几乎全丢
    #[test]
    fn mac_grace_window_captures_delayed_disk_writes() {
        const TRUE_TPS: f64 = 50.0;
        const BPT_TRUE: f64 = 3_900.0; // mac 流式管道字节密度
        let gen_ms = 30_000i64;
        let n = (gen_ms / TICK) as usize; // 43 拍
        let total = TRUE_TPS * BPT_TRUE * (gen_ms as f64 / 1000.0); // 5.85MB
        let t0 = 1_000_000i64;
        // 调用期间磁盘计数几乎不动；脏页在 completed 后 ~7~9.8s 分 4 拍集中落盘
        let mut deltas = vec![0.0; n];
        deltas.extend(std::iter::repeat(0.0).take(10)); // 完成后静默 ~7s
        let chunk = total / 4.0;
        deltas.extend(std::iter::repeat(chunk).take(4));
        let samples = series(t0, &deltas);
        // mac 不做 files 扣除（tracked_files_total 恒 0）
        let files = samples.iter().map(|(t, _)| (*t, 0u64)).collect::<Vec<_>>();
        let rows = merge_streams(&[&build_rows(&samples, 0.0, &mac_params())], &files);
        let call_end = t0 + (n as i64) * TICK;
        let stream_start = call_end - gen_ms;
        let eff = (TRUE_TPS * (gen_ms as f64 / 1000.0)) as u64; // 1500 tok

        // Windows 口径 [.., completed]：字节都还没落盘，几乎全丢
        let (no_grace, _) = integrate(&rows, stream_start, call_end);
        assert!(
            no_grace < total * 0.5,
            "无宽限窗口不应看到大部分字节: {no_grace}"
        );
        // grace 窗口 [.., completed+15s]：滞后落盘字节全部计入，样本恢复真值
        let (clean, _) = integrate(&rows, stream_start, call_end + mac_params().cal_grace_ms);
        let bpt = clean / eff as f64;
        assert!(
            (bpt - BPT_TRUE).abs() / BPT_TRUE < 0.05,
            "宽限窗口样本 {bpt:.0} 应接近真值 {BPT_TRUE:.0}"
        );
    }

    /// 合成端到端：按 measure() 的口径驱动清洗/积分/校准，
    /// 断言「校准后显示值 ≈ 真值」。
    ///
    /// 场景对齐实测现场：30s 调用、真值 50 t/s、UI 管道 600 B/token、
    /// 落盘镜像 50% 流式字节、心跳底噪、被毒化的自适应底噪、首拍请求突发。
    #[test]
    fn synthetic_call_converges_to_true_tps() {
        const TRUE_TPS: f64 = 50.0;
        const BPT_TRUE: f64 = 600.0;
        const FLUSH_RATIO: f64 = 0.5; // 落盘镜像一半流式字节
        const NOISE: f64 = 1_500.0; // 心跳/日志底噪（并入写字节）

        let gen_ms = 30_000i64;
        let n = (gen_ms / TICK) as usize; // 43 拍
        let stream_per_tick = TRUE_TPS * BPT_TRUE * (TICK as f64 / 1000.0); // 21_000 B
        let mut deltas = Vec::with_capacity(n + 1);
        deltas.push(190_000.0); // 请求体上传（首拍突发，应被整拍剔除）
        for _ in 0..n {
            deltas.push(stream_per_tick + NOISE);
        }
        let t0 = 1_000_000i64;
        let samples = series(t0, &deltas);
        // 文件增长：与写入同拍可见
        let mut files = vec![(samples[0].0, 0u64)];
        for (i, d) in deltas.iter().enumerate() {
            let prev = files[i].1;
            files.push((samples[i + 1].0, prev + (*d * FLUSH_RATIO) as u64));
        }

        let rows = merge_streams(
            &[&build_rows(&samples, 40_000.0 /* 毒化的底噪 */, &CleanParams::windows())],
            &files,
        );
        let call_end = t0 + (deltas.len() as i64) * TICK;
        let stream_start = call_end - gen_ms;

        // 一致性校准：分子 = 同一条清洗流在调用区间的积分
        let (clean, cov_s) = integrate(&rows, stream_start, call_end);
        let eff = (TRUE_TPS * (gen_ms as f64 / 1000.0)) as u64; // 1500 tok
        let bpt = (clean / eff as f64).clamp(CAL_MIN, CAL_MAX);
        assert!(
            cov_s > (gen_ms as f64 / 1000.0) * 0.9,
            "积分应覆盖调用区间: {cov_s}"
        );

        // 稳态显示：30s 滑窗满窗
        let (wb, ws) = integrate(&rows, call_end - WINDOW_MS, call_end);
        let pipe_bps = (wb / ws).max(0.0);
        let shown = pipe_bps / bpt;
        assert!(
            (shown - TRUE_TPS).abs() / TRUE_TPS < 0.05,
            "校准后显示 {shown:.1} 应接近真值 {TRUE_TPS}"
        );
    }

    /// 落盘 flush 延迟成大块（错位最恶劣情形）：正负拍在区间总和对消，
    /// 一致性校准仍收敛到真值
    #[test]
    fn delayed_flush_still_converges() {
        const TRUE_TPS: f64 = 50.0;
        const BPT_TRUE: f64 = 600.0;
        let gen_ms = 30_000i64;
        let n = (gen_ms / TICK) as usize;
        let stream_per_tick = TRUE_TPS * BPT_TRUE * (TICK as f64 / 1000.0);
        let deltas: Vec<f64> = std::iter::once(190_000.0)
            .chain(std::iter::repeat(stream_per_tick).take(n))
            .collect();
        let t0 = 1_000_000i64;
        let samples = series(t0, &deltas);
        // 全部落盘字节延迟到最后一拍一次性可见（单块大 flush）
        let total_flush: u64 = (deltas.iter().sum::<f64>() * 0.5) as u64;
        let mut files = Vec::with_capacity(deltas.len() + 1);
        for (i, (t, _)) in samples.iter().enumerate() {
            let v = if i == samples.len() - 1 { total_flush } else { 0 };
            files.push((*t, v));
        }
        let rows = merge_streams(&[&build_rows(&samples, 0.0, &CleanParams::windows())], &files);
        let call_end = t0 + (deltas.len() as i64) * TICK;
        let (clean, _) = integrate(&rows, call_end - gen_ms, call_end);
        let eff = (TRUE_TPS * (gen_ms as f64 / 1000.0)) as u64;
        let bpt = (clean / eff as f64).clamp(CAL_MIN, CAL_MAX);
        let (wb, ws) = integrate(&rows, call_end - WINDOW_MS, call_end);
        let shown = ((wb / ws).max(0.0)) / bpt;
        assert!(
            (shown - TRUE_TPS).abs() / TRUE_TPS < 0.05,
            "延迟 flush 场景显示 {shown:.1} 应接近真值 {TRUE_TPS}"
        );
    }
}
