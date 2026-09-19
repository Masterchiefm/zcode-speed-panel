//! 网络流量与上传监控：整机速度/当日总量（真实）+ 会话/非会话上传拆分。
//!
//! ## 分层口径（为什么不能按进程直测网络字节）
//!
//! 2026-09-18 本机实验（详见 docs/key-rules.md #15）：
//! - **Winsock 收发字节不进 `GetProcessIoCounters` 的任何计数**（20MB 下载期间
//!   Read 仅 7.8KB，Write 12MB 是落盘镜像）——进程 IO 计数器只含文件/管道/
//!   设备，liveio 的流式测速因此天然不受网络污染；
//! - **TCP ESTATS（`SetPerTcpConnectionEStats`）已坏**：对所有连接（含本进程
//!   自有的）返回 ERROR_NOT_SUPPORTED，管理员也一样；
//! - ETW 内核网络事件需要管理员。
//!
//! => 非管理员下两平台都没有"按进程的网络收发字节"公开原语。本模块按三层
//! 诚实分层：
//!
//! 1. **整机上传/下载（真实值）**：接口计数器求和（Windows `GetIfTable` 的
//!    32 位 octets 做模差；macOS `getifaddrs` 的 ifi_*bytes），均排除回环。
//!    速度 = ~1s 滑窗差分（对齐任务管理器 ~1s 的刷新节奏）；当日累计跨重启
//!    持久化（`speed-panel-net.json`）。
//!    注意：本机若走本地代理（ZCode → 127.0.0.1 代理进程 → 外网），整机口径
//!    含代理隧道加密开销、且混合其他应用流量。
//! 2. **上传构成拆分**：
//!    - **会话流量（估算 ≈）**：usage 库 token 数 × 字节系数（CLI 进程承载的
//!      API 对话流量；请求体 ≈ input × 5 B/token、流式响应 ≈ output × 8
//!      B/token，量级参考值，前端带 ≈ 标注）；
//!    - **非会话上传（真实下界）**：轮询 `~/.zcode/v2/checkpoints/*/state.json`，
//!      `lastAcceptedManifestHash` 变化 = 快照工件被服务端接受，按
//!      `lastCompressedSize.encryptedSizeBytes`（加密压缩后字节）计入当日；
//!      `activeUpload` 存在 = 上传进行中。目录被 ACL 封锁时如实显示"不可读"。
//!      口径为**面板观测期**：未运行期间的接受仅在当日首次启动时按
//!      recordedAt 回补一次；两个轮询拍之间的多次跳变按末态计（下界）。
//! 3. **ZCode 连接归属（真实值，仅 Windows）**：TCP 连接表（OWNER_PID）按
//!    进程分组——命令行含 `zcode.cjs` 的 CLI 进程 = 会话组（API 流量），其余
//!    `zcode.exe`（Electron 桌面端主/渲染/工具进程）= 非会话组（快照上传、
//!    遥测等），各组显示 ESTABLISHED 连接数与远端。整机上传速度飙升 +
//!    非会话组连接出现 + activeUpload = 快照上传的现场证据链。

use chrono::{Datelike, Local};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 整机速度滑窗（对齐任务管理器 ~1s 的刷新节奏；短窗读数比长窗更跳，属预期）
const NET_WINDOW_MS: i64 = 1_000;
/// 整机采样环容量（~4.5min @700ms）
const NET_RING_CAP: usize = 400;
/// 进程分组刷新周期（Toolhelp + 命令行读取，不逐拍）
const PROC_REFRESH_EVERY: Duration = Duration::from_secs(5);
/// checkpoint 目录扫描周期
const CKPT_SCAN_EVERY: Duration = Duration::from_secs(2);
/// 当日累计落盘节流（退出时另有强制保存）
const NET_SAVE_EVERY: Duration = Duration::from_secs(30);


/// 会话上传估算系数（字节/token）：请求体为 JSON 转义后的**未缓存**提示
/// 增量（实测 98% 缓存命中下整机上传仅数十 KB——缓存命中的提示部分不重发），
/// 英文/代码 ~4 字符/token + 转义开销，取 5。量级参考值（前端带 ≈ 标注）
pub const SESS_UP_BPT: f64 = 5.0;
/// 会话下载估算系数：SSE 事件流密度。2026-09-18 实测标定：流式期整机下载
/// ÷ token ≈ 731 B/token（含其他应用流量的上界）、UI 管道系数 bpt≈320
/// （下界），取 400 居中。量级参考值
pub const SESS_DOWN_BPT: f64 = 400.0;

/// 会话流量估算（纯函数）。上传分子用**未缓存提示**（input 已含缓存命中
/// 部分，缓存命中不重发——按全量重发估算会虚高数十倍，实测整机当日上传
/// 仅数十 KB 可证）；output = 输出+思考 token
pub fn sess_bytes_est(uncached_input_tokens: u64, output_tokens: u64) -> (u64, u64) {
    (
        (uncached_input_tokens as f64 * SESS_UP_BPT) as u64,
        (output_tokens as f64 * SESS_DOWN_BPT) as u64,
    )
}

/// 工作区实况列表（纯函数，可测）：状态表 → 快照上传记录行，
/// 排序 = 上传中 > 待传（未接受）> 已接受，同状态按记录时刻倒序。
/// 不截断——全部工作区都要能列出（用户明确要求，2026-09-18）
pub(crate) fn ckpt_rows(states: &HashMap<String, CkptState>) -> Vec<crate::metrics::CkptStat> {
    let mut rows: Vec<crate::metrics::CkptStat> = states
        .iter()
        .map(|(hash, s)| crate::metrics::CkptStat {
            workspace: if s.workspace.is_empty() { "?".into() } else { s.workspace.clone() },
            bytes: s.artifact_bytes,
            recorded_ms: s.recorded_at.unwrap_or(0),
            accepted: s.accepted_hash.is_some(),
            uploading: s.uploading,
            // 子目录名 = 工作区哈希，前端"打开目录"按它拼路径
            hash: Some(hash.clone()),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.uploading
            .cmp(&a.uploading)
            .then(a.accepted.cmp(&b.accepted))
            .then(b.recorded_ms.cmp(&a.recorded_ms))
    });
    rows
}

/// 扫描 checkpoints 目录 → (状态, 观测列表)。NetIo 每拍观测与
/// snapshot_guard 的 apply 留档（先留档再清空，key-rules #16）共用
pub(crate) fn scan_ckpt_states(base: &std::path::Path) -> (String, Vec<(String, CkptState)>) {
    let rd = match std::fs::read_dir(base) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ("missing".into(), Vec::new()),
        // 权限拒绝（如 ACL 封锁）或其他错误：如实上报 blocked
        Err(_) => return ("blocked".into(), Vec::new()),
    };
    let mut obs = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Ok(text) = std::fs::read_to_string(e.path().join("state.json")) {
            if let Some(st) = parse_ckpt_state(&text) {
                obs.push((name, st));
            }
        }
    }
    ("ok".into(), obs)
}

/// 接口计数器差分（纯函数，可测）：wrap>0 时按模数做回绕差分（Windows
/// 32 位 octets），wrap=0 时为裸差分（mac 64 位）并对回退钳 0（计数器重置）。
/// 单接口单拍增量超过 2^31 视为异常（重置/索引复用），钳 0 防假流量
pub(crate) fn wrap_delta(new: u64, old: u64, wrap: u64) -> u64 {
    let d = if wrap > 0 {
        ((new as i64 - old as i64).rem_euclid(wrap as i64)) as u64
    } else {
        new.saturating_sub(old)
    };
    if d >= (1u64 << 31) {
        0
    } else {
        d
    }
}

/// 每拍产出的网络监控快照（build_payload 填入 Snapshot 推送前端）
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetNow {
    /// 整机接口计数是否可用（stub 平台 false）
    pub available: bool,
    pub up_bps: f64,
    pub down_bps: f64,
    /// 整机当日累计（真实，跨重启持久化续算）
    pub up_today: u64,
    pub down_today: u64,
    /// 连接归属是否可用（仅 Windows）
    pub conns_available: bool,
    /// 会话组（CLI 进程）去重后的 ESTABLISHED 远端条数
    pub cli_conns: u32,
    /// 非会话组（Electron 桌面端进程）去重后的远端条数
    pub app_conns: u32,
    /// 两组的连接明细（远端 + 归属 pid + 进程类型标签，按远端+pid 去重排序；
    /// tooltip 逐条展示"哪个进程连了哪里"）
    pub cli_conn_list: Vec<crate::metrics::ConnStat>,
    pub app_conn_list: Vec<crate::metrics::ConnStat>,
    /// checkpoints 目录状态：ok / missing（无目录）/ blocked（不可读，如 ACL 封锁）
    pub ckpt_status: String,
    /// 有 activeUpload 进行中
    pub ckpt_uploading: bool,
    /// 当日接受的快照工件字节（加密压缩后，面板观测期下界）
    pub ckpt_today_bytes: u64,
    pub ckpt_today_count: u32,
    /// 当日已接受工件名单（时间/工作区/大小——回答"是哪几个"）
    pub ckpt_today_list: Vec<crate::metrics::CkptStat>,
    /// 快照上传记录（每工作区最近一次工件的实况，**不设行数上限**——
    /// 全部列出，前端列表限高滚动；上传中 > 待传 > 已接受，同状态按记录时刻倒序）
    pub ckpt_list: Vec<crate::metrics::CkptStat>,
}

// ============ 平台原语（win / mac / stub 三份，对外统一 netio::platform::*） ============

pub mod platform {
    /// Windows：GetIfTable 求和接口 octets（32 位计数器，调用侧做模差）；
    /// GetExtendedTcpTable (OWNER_PID) 枚举 v4+v6 连接按进程分组；
    /// Toolhelp + PEB 命令行区分 CLI（zcode.cjs）与 Electron 桌面端。
    /// 进程/命令行识别口径与 liveio::platform::win 一致（两处独立实现：
    /// liveio 只发现 CLI 进程，这里还要拿"其余 zcode.exe"做非会话组）
    #[cfg(windows)]
    mod win {
        use std::collections::{HashMap, HashSet};
        use std::ffi::c_void;

        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn CloseHandle(h: *mut c_void) -> i32;
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
        #[link(name = "iphlpapi")]
        extern "system" {
            fn GetIfTable(table: *mut c_void, size: *mut u32, order: i32) -> u32;
            fn GetExtendedTcpTable(
                table: *mut c_void,
                size: *mut u32,
                order: i32,
                family: u32,
                class: u32,
                reserved: u32,
            ) -> u32;
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

        const PROCESS_QUERY_LIMITED: u32 = 0x1410;
        const TH32CS_SNAPPROCESS: u32 = 2;

        /// MIB_IFROW 镜像（布局自 NT4 起未变；关键字段偏移编译期钉死）。
        /// 仅用 dwType/dwInOctets/dwOutOctets，但表布局需要完整 sizeof
        #[repr(C)]
        struct MibIfRow {
            wsz_name: [u16; 256],
            dw_index: u32,
            dw_type: u32,
            dw_mtu: u32,
            dw_speed: u32,
            dw_phys_addr_len: u32,
            b_phys_addr: [u8; 8],
            dw_admin_status: u32,
            dw_oper_status: u32,
            dw_last_change: u32,
            dw_in_octets: u32,
            dw_out_octets: u32,
            dw_in_ucast_pkts: u32,
            dw_in_nucast_pkts: u32,
            dw_in_discards: u32,
            dw_in_errors: u32,
            dw_in_unknown_protos: u32,
            dw_out_ucast_pkts: u32,
            dw_out_nucast_pkts: u32,
            dw_out_discards: u32,
            dw_out_errors: u32,
            dw_out_qlen: u32,
            dw_descr_len: u32,
            b_descr: [u8; 256],
        }

        const _: () = {
            assert!(std::mem::offset_of!(MibIfRow, dw_type) == 516);
            assert!(std::mem::offset_of!(MibIfRow, dw_in_octets) == 552);
            assert!(std::mem::offset_of!(MibIfRow, dw_out_octets) == 556);
            assert!(std::mem::size_of::<MibIfRow>() == 860);
        };

        /// IF_TYPE_SOFTWARE_LOOPBACK
        const IF_TYPE_LOOPBACK: u32 = 24;

        /// 接口计数器回绕模数：dwIn/dwOutOctets 为 32 位，逐接口做模 2^32 差分
        pub const NET_COUNTER_WRAP: u64 = 1 << 32;

        /// 全部非回环接口的计数行：(接口索引, 累计上传, 累计下载)。
        /// 回绕修正由调用侧逐接口差分（各接口回绕时机不同，先求和再差分会错）
        pub fn net_ifaces() -> Option<Vec<(String, u64, u64)>> {
            unsafe {
                let mut size = 0u32;
                if GetIfTable(std::ptr::null_mut(), &mut size, 0) != 122 || size == 0 {
                    return None;
                }
                let mut buf = vec![0u8; size as usize];
                if GetIfTable(buf.as_mut_ptr().cast(), &mut size, 0) != 0 {
                    return None;
                }
                let n = buf.as_ptr().cast::<u32>().read_unaligned() as usize;
                let rows = buf.as_ptr().add(4).cast::<MibIfRow>();
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let r = rows.add(i).read_unaligned();
                    if r.dw_type == IF_TYPE_LOOPBACK {
                        continue;
                    }
                    out.push((r.dw_index.to_string(), r.dw_out_octets as u64, r.dw_in_octets as u64));
                }
                Some(out)
            }
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct TcpRow {
            state: u32,
            local_addr: u32,
            local_port: u32,
            remote_addr: u32,
            remote_port: u32,
            pid: u32,
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Tcp6Row {
            state: u32,
            local_addr: [u8; 16],
            local_scope: u32,
            local_port: u32,
            remote_addr: [u8; 16],
            remote_scope: u32,
            remote_port: u32,
            pid: u32,
        }

        const TCP_TABLE_OWNER_PID_ALL: u32 = 5;
        const AF_INET: u32 = 2;
        const AF_INET6: u32 = 23;
        const MIB_TCP_STATE_ESTAB: u32 = 5;

        fn port(p: u32) -> u16 {
            ((p & 0xff) << 8 | (p >> 8) & 0xff) as u16
        }

        fn ipv4(v: u32) -> String {
            format!("{}.{}.{}.{}", v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff, (v >> 24) & 0xff)
        }

        /// 未压缩 IPv6 文本（::ffff: 映射地址也按完整形式展示——仅 tooltip 用）
        fn ipv6(b: &[u8; 16]) -> String {
            let mut s = String::new();
            for i in 0..8 {
                if i > 0 {
                    s.push(':');
                }
                s.push_str(&format!("{:02x}{:02x}", b[i * 2], b[i * 2 + 1]));
            }
            s
        }

        /// 命令行 → 进程类型标签（Electron 壳的 --type 参数区分各子进程；
        /// CLI 判定在前，渲染进程命令行里不会出现 zcode.cjs）
        pub(crate) fn proc_label(cmd: &str) -> &'static str {
            if cmd.contains("zcode.cjs") {
                "CLI 会话进程"
            } else if cmd.contains("crashpad") {
                "崩溃报告进程"
            } else if cmd.contains("--type=renderer") {
                "渲染进程"
            } else if cmd.contains("--type=gpu-process") {
                "GPU 进程"
            } else if cmd.contains("--type=utility") {
                "工具进程"
            } else {
                "主进程"
            }
        }

        /// ZCode 相关进程的 ESTABLISHED 连接（远端, 归属 pid），按
        /// (cli_pids, app_pids) 两组返回（各自按 remote+pid 去重、排序）。
        /// 连接表读不到时返回 None（连接归属不可用）
        pub fn zcode_conns(
            cli_pids: &HashMap<u32, String>,
            app_pids: &HashMap<u32, String>,
        ) -> Option<(Vec<(String, u32)>, Vec<(String, u32)>)> {
            let mut cli: HashSet<(String, u32)> = HashSet::new();
            let mut app: HashSet<(String, u32)> = HashSet::new();
            unsafe {
                let mut size = 0u32;
                GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0);
                if size > 0 {
                    let mut buf = vec![0u8; size as usize];
                    if GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0) == 0 {
                        let n = buf.as_ptr().cast::<u32>().read_unaligned() as usize;
                        let rows = buf.as_ptr().add(4).cast::<TcpRow>();
                        for i in 0..n {
                            let r = rows.add(i).read_unaligned();
                            if r.state != MIB_TCP_STATE_ESTAB {
                                continue;
                            }
                            let target = if cli_pids.contains_key(&r.pid) {
                                &mut cli
                            } else if app_pids.contains_key(&r.pid) {
                                &mut app
                            } else {
                                continue;
                            };
                            target.insert((format!("{}:{}", ipv4(r.remote_addr), port(r.remote_port)), r.pid));
                        }
                    }
                }
                let mut size6 = 0u32;
                GetExtendedTcpTable(std::ptr::null_mut(), &mut size6, 0, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0);
                if size6 > 0 {
                    let mut buf = vec![0u8; size6 as usize];
                    if GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut size6, 0, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0) == 0 {
                        let n = buf.as_ptr().cast::<u32>().read_unaligned() as usize;
                        let rows = buf.as_ptr().add(4).cast::<Tcp6Row>();
                        for i in 0..n {
                            let r = rows.add(i).read_unaligned();
                            if r.state != MIB_TCP_STATE_ESTAB {
                                continue;
                            }
                            let target = if cli_pids.contains_key(&r.pid) {
                                &mut cli
                            } else if app_pids.contains_key(&r.pid) {
                                &mut app
                            } else {
                                continue;
                            };
                            target.insert((format!("[{}]:{}", ipv6(&r.remote_addr), port(r.remote_port)), r.pid));
                        }
                    }
                }
            }
            let sort = |s: HashSet<(String, u32)>| {
                let mut v: Vec<(String, u32)> = s.into_iter().collect();
                v.sort();
                v
            };
            Some((sort(cli), sort(app)))
        }

        /// 与 liveio::platform::win 相同的 PEB → ProcessParameters →
        /// CommandLine(UNICODE_STRING @ 0x70) 读取链
        fn process_command_line(pid: u32) -> Option<String> {
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED, 0, pid);
                if h.is_null() {
                    return None;
                }
                let rd = |addr: usize, buf: &mut [u8]| -> bool {
                    let mut n = 0usize;
                    ReadProcessMemory(h, addr as *const c_void, buf.as_mut_ptr().cast(), buf.len(), &mut n) != 0
                };
                let read_cmdline = || -> Option<String> {
                    let mut pbi = [0u8; 48];
                    let mut ret: u32 = 0;
                    if NtQueryInformationProcess(h, 0, pbi.as_mut_ptr().cast(), 48, &mut ret) != 0 {
                        return None;
                    }
                    #[cfg(target_pointer_width = "64")]
                    {
                        let peb = usize::from_ne_bytes(pbi[8..16].try_into().ok()?);
                        if peb == 0 {
                            return None;
                        }
                        let mut pp_ptr = [0u8; 8];
                        if !rd(peb + 0x20, &mut pp_ptr) {
                            return None;
                        }
                        let pp = usize::from_ne_bytes(pp_ptr.try_into().ok()?);
                        if pp == 0 {
                            return None;
                        }
                        let mut us = [0u8; 16];
                        if !rd(pp + 0x70, &mut us) {
                            return None;
                        }
                        let len = u16::from_ne_bytes([us[0], us[1]]) as usize;
                        let buf_ptr = usize::from_ne_bytes(us[8..16].try_into().ok()?);
                        if len == 0 || buf_ptr == 0 {
                            return None;
                        }
                        let mut wbuf = vec![0u8; len];
                        if !rd(buf_ptr, &mut wbuf) {
                            return None;
                        }
                        let u16s: Vec<u16> = wbuf
                            .chunks_exact(2)
                            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                            .collect();
                        Some(String::from_utf16_lossy(&u16s))
                    }
                    #[cfg(not(target_pointer_width = "64"))]
                    {
                        None
                    }
                };
                let out = read_cmdline();
                CloseHandle(h);
                out
            }
        }

        /// 发现 ZCode 进程并分组（pid + 进程类型标签）：
        /// (CLI 进程 = 会话组, 其余 zcode.exe = 桌面端组)。CLI = exe 名
        /// zcode.exe（大小写不敏感）且命令行含 zcode.cjs；不含 zcode.cjs 的
        /// zcode.exe = Electron 桌面端（主/渲染/GPU/工具进程——快照上传等
        /// 非会话流量的承载者）。两组都是 ZCode 自身进程，不含其他应用
        pub fn zcode_pid_groups() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
            let mut cli = Vec::new();
            let mut app = Vec::new();
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap == -1 {
                    return (cli, app);
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
                if Process32FirstW(snap, &mut entry) != 0 {
                    loop {
                        let exe = String::from_utf16_lossy(
                            &entry.exe_file[..entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260)],
                        );
                        if exe.eq_ignore_ascii_case("zcode.exe") {
                            match process_command_line(entry.process_id) {
                                Some(cmd) => {
                                    let label = proc_label(&cmd).to_string();
                                    if cmd.contains("zcode.cjs") {
                                        cli.push((entry.process_id, label));
                                    } else {
                                        app.push((entry.process_id, label));
                                    }
                                }
                                None => {} // 命令行读不到（权限/竞态）不计入任何组
                            }
                        }
                        if Process32NextW(snap, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snap as *mut c_void);
            }
            (cli, app)
        }
    }

    /// macOS：getifaddrs 求和接口 ifi_obytes/ifi_ibytes（64 位，排除 lo0）。
    /// 连接归属（按进程分组 TCP 连接）mac 侧未实现——收益集中在 Windows 桌面
    /// 端（快照上传监控的取证链），mac 面板如实显示"连接明细仅 Windows"
    #[cfg(target_os = "macos")]
    mod mac {
        use std::collections::{HashMap, HashSet};
        use std::ffi::{c_char, c_int, c_void};

        #[link(name = "System")]
        extern "C" {
            fn getifaddrs(ptr: *mut *mut IfAddrs) -> c_int;
            fn freeifaddrs(ptr: *mut IfAddrs);
        }

        /// struct ifaddrs 镜像（flags 为 4 字节，其后指针需 8 字节对齐有填充）
        #[repr(C)]
        struct IfAddrs {
            next: *mut IfAddrs,
            name: *const c_char,
            flags: u32,
            pad: u32,
            addr: *mut c_void,
            netmask: *mut c_void,
            dstaddr: *mut c_void,
            data: *mut c_void,
            spare: *mut c_void,
        }

        /// struct if_data64（macOS 64 位）镜像：ifi_ibytes=64 / ifi_obytes=72
        /// 对照 xnu SDK net/if.h，断言钉死；SDK 布局变化直接编译失败，禁删断言
        #[repr(C)]
        struct IfData64 {
            ifi_type: u8,
            ifi_typelen: u8,
            ifi_physical: u8,
            ifi_addrlen: u8,
            ifi_hdrlen: u8,
            ifi_recvquota: u8,
            ifi_xmitquota: u8,
            ifi_unused1: u8,
            ifi_mtu: u32,
            ifi_metric: u32,
            ifi_baudrate: u64,
            ifi_ipackets: u64,
            ifi_ierrors: u64,
            ifi_opackets: u64,
            ifi_oerrors: u64,
            ifi_collisions: u64,
            ifi_ibytes: u64,
            ifi_obytes: u64,
        }

        const _: () = {
            assert!(std::mem::offset_of!(IfData64, ifi_ibytes) == 64);
            assert!(std::mem::offset_of!(IfData64, ifi_obytes) == 72);
        };

        /// 接口计数器回绕模数：ifi_*bytes 为 64 位，实际不回绕（0 = 裸差分）
        pub const NET_COUNTER_WRAP: u64 = 0;

        /// 全部非回环接口的计数行：(接口名, 累计上传, 累计下载)。
        /// getifaddrs 对每接口按地址族返回多行，必须按接口名去重
        /// （否则字节翻倍）；排除回环 lo0
        pub fn net_ifaces() -> Option<Vec<(String, u64, u64)>> {
            unsafe {
                let mut head: *mut IfAddrs = std::ptr::null_mut();
                if getifaddrs(&mut head) != 0 {
                    return None;
                }
                let mut seen: HashSet<String> = HashSet::new();
                let mut out = Vec::new();
                let mut p = head;
                while !p.is_null() {
                    let ifa = &*p;
                    if !ifa.name.is_null() && !ifa.data.is_null() {
                        let name = std::ffi::CStr::from_ptr(ifa.name).to_string_lossy().into_owned();
                        if name != "lo0" && seen.insert(name.clone()) {
                            let d = &*(ifa.data as *const IfData64);
                            out.push((name, d.ifi_obytes, d.ifi_ibytes));
                        }
                    }
                    p = ifa.next;
                }
                freeifaddrs(head);
                Some(out)
            }
        }

        /// 连接归属仅 Windows 实现；mac 返回 None（面板显示"不可用"）
        pub fn zcode_conns(
            _cli_pids: &HashMap<u32, String>,
            _app_pids: &HashMap<u32, String>,
        ) -> Option<(Vec<(String, u32)>, Vec<(String, u32)>)> {
            None
        }

        pub fn zcode_pid_groups() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
            (Vec::new(), Vec::new())
        }
    }

    /// 其他平台：接口计数与连接归属均不可用（面板显示"不支持"）
    #[cfg(not(any(windows, target_os = "macos")))]
    mod stub {
        use std::collections::HashMap;

        pub const NET_COUNTER_WRAP: u64 = 0;

        pub fn net_ifaces() -> Option<Vec<(String, u64, u64)>> {
            None
        }
        pub fn zcode_conns(
            _cli: &HashMap<u32, String>,
            _app: &HashMap<u32, String>,
        ) -> Option<(Vec<(String, u32)>, Vec<(String, u32)>)> {
            None
        }
        pub fn zcode_pid_groups() -> (Vec<(u32, String)>, Vec<(u32, String)>) {
            (Vec::new(), Vec::new())
        }
    }

    #[cfg(windows)]
    pub use win::*;
    #[cfg(target_os = "macos")]
    pub use mac::*;
    #[cfg(not(any(windows, target_os = "macos")))]
    pub use stub::*;
}

// ============ checkpoint 工件证据（解析与差分为纯函数，可测） ============

/// 单个 workspace 的 checkpoints state.json 摘要
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CkptState {
    /// 工作区显示名（workspacePath 末段）
    pub workspace: String,
    /// 最近一次压缩加密工件字节数（lastCompressedSize.encryptedSizeBytes）
    pub artifact_bytes: u64,
    /// 最近工件的 manifest 哈希
    pub artifact_hash: Option<String>,
    /// 服务端已接受的 manifest 哈希（lastAcceptedManifestHash）
    pub accepted_hash: Option<String>,
    /// 是否有 activeUpload 进行中
    pub uploading: bool,
    /// 工件记录时刻（lastCompressedSize.recordedAt，epoch ms）
    pub recorded_at: Option<i64>,
}

/// state.json → 摘要。字段缺失/损坏返回 None（该 workspace 本拍跳过）
pub(crate) fn parse_ckpt_state(json: &str) -> Option<CkptState> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let lc = v.get("lastCompressedSize")?;
    let workspace = v
        .get("workspacePath")
        .and_then(|x| x.as_str())
        .map(|p| {
            p.rsplit(['\\', '/'])
                .find(|s| !s.is_empty())
                .unwrap_or(p)
                .to_string()
        })
        .unwrap_or_default();
    Some(CkptState {
        workspace,
        artifact_bytes: lc.get("encryptedSizeBytes").and_then(|x| x.as_u64()).unwrap_or(0),
        artifact_hash: lc.get("manifestHash").and_then(|x| x.as_str()).map(String::from),
        accepted_hash: v
            .get("lastAcceptedManifestHash")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        uploading: v.get("activeUpload").map_or(false, |x| !x.is_null()),
        recorded_at: lc.get("recordedAt").and_then(|x| x.as_i64()),
    })
}

/// checkpoint 差分事件（调用方补时间与文案）
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CkptEvent {
    pub kind: &'static str,
    pub workspace: String,
    pub bytes: u64,
    /// accepted 事件带工件记录时刻（recordedAt；上传起止事件为 0）——
    /// 供"今日 N 个工件"名单的时间列显示
    pub recorded_ms: i64,
}

/// 逐 workspace 应用一轮观测（纯函数，可测）：
/// - `states`：目录名 → 上次观测（函数内更新为本次观测）
/// - `day_start_ms`：本地今日 0 点（recordedAt 归属判定）
/// - `count`：true = 全新一天的首扫（回补当日已发生但面板未在场的接受）
///
/// 接受判定：accepted_hash 变化为某个新值（含首见且 count）→ 该工件字节计
/// 入当日（recordedAt ≥ 今日 0 点才计——跨天去重靠该守卫自然成立：同一哈希
/// 的 recordedAt 永远早于新一天的 0 点）。上传开始/结束只产生事件不计数。
pub(crate) fn apply_ckpt_obs(
    states: &mut HashMap<String, CkptState>,
    obs: Vec<(String, CkptState)>,
    day_start_ms: i64,
    count: bool,
) -> Vec<CkptEvent> {
    let mut events = Vec::new();
    for (key, new) in obs {
        let old = states.insert(key, new.clone());
        let accepted_now = || {
            new.accepted_hash.is_some()
                && new.recorded_at.map_or(true, |t| t >= day_start_ms)
        };
        match old {
            None => {
                // 首见：全新一天（count=true）回补今天记录的接受；
                // 面板今天已运行过（count=false）只建基线
                if count && accepted_now() {
                    events.push(CkptEvent {
                        kind: "accepted",
                        workspace: new.workspace.clone(),
                        bytes: new.artifact_bytes,
                        recorded_ms: new.recorded_at.unwrap_or(0),
                    });
                }
            }
            Some(old) => {
                if old.accepted_hash != new.accepted_hash && accepted_now() {
                    events.push(CkptEvent {
                        kind: "accepted",
                        workspace: new.workspace.clone(),
                        bytes: new.artifact_bytes,
                        recorded_ms: new.recorded_at.unwrap_or(0),
                    });
                }
                if !old.uploading && new.uploading {
                    events.push(CkptEvent {
                        kind: "upload_start",
                        workspace: new.workspace.clone(),
                        bytes: new.artifact_bytes,
                        recorded_ms: 0,
                    });
                } else if old.uploading && !new.uploading {
                    // 结束时刻不判成功失败：接受与否由 accepted_hash 差分判定
                    events.push(CkptEvent { kind: "upload_end", workspace: new.workspace.clone(), bytes: 0, recorded_ms: 0 });
                }
            }
        }
    }
    events
}

// ============ 主状态机 ============

fn local_ymd() -> (i32, u32, u32) {
    let n = Local::now();
    (n.year(), n.month(), n.day())
}

fn ymd_str((y, m, d): (i32, u32, u32)) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

/// 本地今日 0 点（epoch ms）。失败退化为 now - 24h（守卫偏松不影响正确性：
/// 只是回补计数的归属边界）
fn local_day_start_ms() -> i64 {
    use chrono::NaiveTime;
    let now = Local::now();
    now.date_naive()
        .and_time(NaiveTime::MIN)
        .and_local_timezone(now.timezone())
        .single()
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| now.timestamp_millis() - 86_400_000)
}

fn net_file() -> Option<std::path::PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("speed-panel-net.json"))
}

pub struct NetIo {
    /// (时刻 ms, 解回绕后的整机累计上传, 累计下载)——单调递增，
    /// 速度窗口差分可直接相减
    ring: VecDeque<(i64, u64, u64)>,
    /// 上一拍各接口计数快照（逐接口差分：各接口回绕时机不同，
    /// 先求和再差分在任一接口回绕后就会错）
    ifaces: HashMap<String, (u64, u64)>,
    acc_up: u64,
    acc_down: u64,
    today_ymd: (i32, u32, u32),
    up_today: u64,
    down_today: u64,
    ckpt_today_bytes: u64,
    ckpt_today_count: u32,
    /// 当日已接受工件名单（workspace/bytes/recorded_ms；跨天清零、随
    /// speed-panel-net.json 持久化）——"今日 N 个工件"要能看出是哪几个
    today_uploads: Vec<crate::metrics::CkptStat>,
    /// pid → 进程类型标签（"CLI 会话进程"/"主进程"/"渲染进程"/…）
    cli_pids: HashMap<u32, String>,
    app_pids: HashMap<u32, String>,
    proc_refresh: Option<Instant>,
    ckpt_scan: Option<Instant>,
    ckpt_states: HashMap<String, CkptState>,
    ckpt_status: String,
    ckpt_uploading: bool,
    last_save: Option<Instant>,
    dirty: bool,
    /// 待写入调试日志的事件（main.rs 每拍取走）
    pending_events: VecDeque<serde_json::Value>,
}

impl NetIo {
    pub fn new() -> Self {
        let mut io = NetIo {
            ring: VecDeque::new(),
            ifaces: HashMap::new(),
            acc_up: 0,
            acc_down: 0,
            today_ymd: local_ymd(),
            up_today: 0,
            down_today: 0,
            ckpt_today_bytes: 0,
            ckpt_today_count: 0,
            today_uploads: Vec::new(),
            cli_pids: HashMap::new(),
            app_pids: HashMap::new(),
            proc_refresh: None,
            ckpt_scan: None,
            ckpt_states: HashMap::new(),
            ckpt_status: String::new(),
            ckpt_uploading: false,
            last_save: None,
            dirty: false,
            pending_events: VecDeque::new(),
        };
        let fresh_day = io.load_persisted();
        // 基线扫描：全新一天回补"今天已发生但面板未在场"的接受（计入当日）；
        // 面板今天已运行过（恢复了当日累计）只建状态基线
        let (status, obs) = io.scan_ckpt();
        io.ckpt_status = status.clone();
        let events = apply_ckpt_obs(&mut io.ckpt_states, obs, local_day_start_ms(), fresh_day);
        io.handle_ckpt_events(events, fresh_day);
        io
    }

    /// 恢复当日累计。返回是否"全新一天"（true = 持久化不存在/不是今天）
    fn load_persisted(&mut self) -> bool {
        let Some(path) = net_file() else { return true };
        let Ok(raw) = std::fs::read_to_string(path) else { return true };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            eprintln!("[zcode-speed-panel] net 累计文件损坏，从零起算");
            return true;
        };
        let day = v.get("day").and_then(|x| x.as_str()).unwrap_or("");
        if day != ymd_str(self.today_ymd) {
            return true; // 昨天的累计：跨天自然清零
        }
        self.up_today = v.get("up").and_then(|x| x.as_u64()).unwrap_or(0);
        self.down_today = v.get("down").and_then(|x| x.as_u64()).unwrap_or(0);
        self.ckpt_today_bytes = v.get("ckpt").and_then(|x| x.as_u64()).unwrap_or(0);
        self.ckpt_today_count = v.get("ckpt_count").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        self.today_uploads = v.get("uploads")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .take(100)
                    .map(|u| crate::metrics::CkptStat {
                        workspace: u.get("ws").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
                        bytes: u.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
                        recorded_ms: u.get("at").and_then(|x| x.as_i64()).unwrap_or(0),
                        accepted: true,
                        uploading: false,
                        hash: None, // 持久化 JSON 只存名单字段，无目录名
                    })
                    .collect()
            })
            .unwrap_or_default();
        false
    }

    fn save(&mut self, force: bool) {
        let due = force || self.last_save.map_or(true, |t| t.elapsed() > NET_SAVE_EVERY);
        if !due || !self.dirty {
            return;
        }
        self.last_save = Some(Instant::now());
        self.dirty = false;
        if let Some(path) = net_file() {
            let json = serde_json::json!({
                "day": ymd_str(self.today_ymd),
                "up": self.up_today,
                "down": self.down_today,
                "ckpt": self.ckpt_today_bytes,
                "ckpt_count": self.ckpt_today_count,
                // 当日已接受工件名单（旧版文件无此键 → 空名单，只有累计数）
                "uploads": self.today_uploads.iter().map(|u| serde_json::json!({
                    "ws": u.workspace, "bytes": u.bytes, "at": u.recorded_ms,
                })).collect::<Vec<_>>(),
            });
            if let Err(e) = std::fs::write(&path, json.to_string()) {
                eprintln!("[zcode-speed-panel] net 累计落盘失败: {e}");
            }
        }
    }

    /// 扫描 checkpoints 目录。返回 (状态, 观测列表)
    fn scan_ckpt(&self) -> (String, Vec<(String, CkptState)>) {
        let Some(home) = crate::metrics::home_dir() else {
            return ("missing".into(), Vec::new());
        };
        scan_ckpt_states(&home.join(".zcode").join("v2").join("checkpoints"))
    }

    /// 差分事件 → 当日累计 + 调试日志事件（工作区实况列表由 `ckpt_rows`
    /// 从状态表另出，事件文案不重复进 UI）。backfill=true 表示当日首扫回补
    fn handle_ckpt_events(&mut self, events: Vec<CkptEvent>, backfill: bool) {
        for ev in events {
            let ws = if ev.workspace.is_empty() { "?".to_string() } else { ev.workspace.clone() };
            let mb = (ev.bytes as f64 / 1048576.0 * 10.0).round() / 10.0;
            match ev.kind {
                "accepted" => {
                    self.ckpt_today_bytes += ev.bytes;
                    self.ckpt_today_count += 1;
                    self.today_uploads.push(crate::metrics::CkptStat {
                        workspace: ev.workspace.clone(),
                        bytes: ev.bytes,
                        recorded_ms: ev.recorded_ms,
                        accepted: true,
                        uploading: false,
                        hash: None, // 今日名单按工作区记，不掺目录名
                    });
                    while self.today_uploads.len() > 100 {
                        self.today_uploads.remove(0);
                    }
                    self.dirty = true;
                    self.pending_events.push_back(serde_json::json!({
                        "kind": "net", "ev": "ckpt_accepted", "ws": ws,
                        "mb": mb, "backfill": backfill,
                    }));
                }
                "upload_start" => {
                    self.pending_events.push_back(serde_json::json!({
                        "kind": "net", "ev": "ckpt_upload_start", "ws": ws, "mb": mb,
                    }));
                }
                _ => {
                    self.pending_events.push_back(serde_json::json!({
                        "kind": "net", "ev": "ckpt_upload_end", "ws": ws,
                    }));
                }
            }
        }
    }

    /// 取走待写调试日志的事件
    pub fn take_events(&mut self) -> VecDeque<serde_json::Value> {
        std::mem::take(&mut self.pending_events)
    }

    /// 退出前强制落盘（save_all 调用）
    pub fn save_forced(&mut self) {
        self.dirty = true;
        self.save(true);
    }

    /// 每拍调用（poller ~700ms）
    pub fn tick(&mut self, now_ms: i64) -> NetNow {
        // 跨天清零（整机累计与工件累计都只算今天）
        let ymd = local_ymd();
        if ymd != self.today_ymd {
            self.today_ymd = ymd;
            self.up_today = 0;
            self.down_today = 0;
            self.ckpt_today_bytes = 0;
            self.ckpt_today_count = 0;
            self.today_uploads.clear();
            self.dirty = true;
        }

        // 整机接口计数 → 逐接口差分（回绕修正见 wrap_delta）→ 环 + 当日累计。
        // 环里存解回绕后的单调累计，速度窗口直接相减
        let available;
        if let Some(rows) = platform::net_ifaces() {
            available = true;
            let (mut du, mut dd) = (0u64, 0u64);
            for (k, up, down) in &rows {
                if let Some(&(ou, od)) = self.ifaces.get(k) {
                    du += wrap_delta(*up, ou, platform::NET_COUNTER_WRAP);
                    dd += wrap_delta(*down, od, platform::NET_COUNTER_WRAP);
                }
            }
            self.ifaces = rows.iter().map(|(k, u, d)| (k.clone(), (*u, *d))).collect();
            self.acc_up += du;
            self.acc_down += dd;
            if du > 0 || dd > 0 {
                self.up_today += du;
                self.down_today += dd;
                self.dirty = true;
            }
            self.ring.push_back((now_ms, self.acc_up, self.acc_down));
            while self.ring.len() > NET_RING_CAP {
                self.ring.pop_front();
            }
        } else {
            available = false;
        }

        // 约 1s 滑窗差分速度（窗口内最早的样本 vs 最新；累计值单调，直接相减；
        // 对齐任务管理器 ~1s 刷新的口径，读数更跳属预期）
        let (up_bps, down_bps) = {
            let r = &self.ring;
            match (r.front(), r.back()) {
                (Some(&(t0, _, _)), Some(&(t1, u1, d1))) if t1 > t0 => {
                    let from = now_ms - NET_WINDOW_MS;
                    let (bt, bu, bd) = r
                        .iter()
                        .find(|&&(t, _, _)| t >= from)
                        .copied()
                        .unwrap_or((t0, u1, d1));
                    let secs = (t1 - bt).max(1) as f64 / 1000.0;
                    (u1.saturating_sub(bu) as f64 / secs, d1.saturating_sub(bd) as f64 / secs)
                }
                _ => (0.0, 0.0),
            }
        };

        // 进程分组刷新（连接表每拍枚举很便宜；进程+命令行扫描 5s 一次）
        let due = self.proc_refresh.map_or(true, |t| t.elapsed() > PROC_REFRESH_EVERY);
        if due {
            self.proc_refresh = Some(Instant::now());
            let (cli, app) = platform::zcode_pid_groups();
            self.cli_pids = cli.into_iter().collect();
            self.app_pids = app.into_iter().collect();
        }

        // 连接归属（仅 Windows 实现返回 Some）：每条连接标注归属 pid，
        // 组装 ConnStat 时带上进程类型标签（同进程可有多条连接）
        let conns = platform::zcode_conns(&self.cli_pids, &self.app_pids);
        let conns_available = conns.is_some();
        let mut cli_conn_list: Vec<crate::metrics::ConnStat> = Vec::new();
        let mut app_conn_list: Vec<crate::metrics::ConnStat> = Vec::new();
        if let Some((cli_raw, app_raw)) = conns {
            for (remote, pid) in cli_raw {
                let proc = self.cli_pids.get(&pid).cloned().unwrap_or_default();
                cli_conn_list.push(crate::metrics::ConnStat { remote, pid, proc });
            }
            for (remote, pid) in app_raw {
                let proc = self.app_pids.get(&pid).cloned().unwrap_or_default();
                app_conn_list.push(crate::metrics::ConnStat { remote, pid, proc });
            }
        }
        let cli_conns = cli_conn_list.len() as u32;
        let app_conns = app_conn_list.len() as u32;

        // checkpoint 扫描（2s 节流）
        if self.ckpt_scan.map_or(true, |t| t.elapsed() > CKPT_SCAN_EVERY) {
            self.ckpt_scan = Some(Instant::now());
            let (status, obs) = self.scan_ckpt();
            self.ckpt_status = status;
            let events = apply_ckpt_obs(&mut self.ckpt_states, obs, local_day_start_ms(), false);
            self.ckpt_uploading = self.ckpt_states.values().any(|s| s.uploading);
            if !events.is_empty() {
                self.handle_ckpt_events(events, false);
            }
        }

        self.save(false);

        NetNow {
            available,
            up_bps: up_bps.max(0.0),
            down_bps: down_bps.max(0.0),
            up_today: self.up_today,
            down_today: self.down_today,
            conns_available,
            cli_conns,
            app_conns,
            cli_conn_list,
            app_conn_list,
            ckpt_status: self.ckpt_status.clone(),
            ckpt_uploading: self.ckpt_uploading,
            ckpt_today_bytes: self.ckpt_today_bytes,
            ckpt_today_count: self.ckpt_today_count,
            ckpt_today_list: self.today_uploads.clone(),
            ckpt_list: ckpt_rows(&self.ckpt_states),
        }
    }
}

// ============ 测试 ============
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sess_est_scales_with_tokens() {
        // 上传按未缓存提示计（缓存命中不重发），下载按输出 token × 400
        let (up, down) = sess_bytes_est(1000, 2000);
        assert!((up as f64 - 5000.0).abs() < 1e-6);
        assert!((down as f64 - 800_000.0).abs() < 1e-6);
    }

    #[test]
    fn parse_ckpt_state_fields() {
        let json = r#"{
            "workspacePath": "C:\\Users\\moqiq\\PycharmProjects\\primer_re",
            "lastCompressedSize": {
                "encryptedSizeBytes": 574522944,
                "workspaceSizeBytes": 100,
                "manifestHash": "abc123",
                "recordedAt": 1788290688837
            },
            "lastAcceptedManifestHash": "abc123",
            "activeUpload": null
        }"#;
        let s = parse_ckpt_state(json).expect("应解析成功");
        assert_eq!(s.workspace, "primer_re");
        assert_eq!(s.artifact_bytes, 574522944);
        assert_eq!(s.artifact_hash.as_deref(), Some("abc123"));
        assert_eq!(s.accepted_hash.as_deref(), Some("abc123"));
        assert!(!s.uploading);
        assert_eq!(s.recorded_at, Some(1788290688837));
        // 损坏 JSON / 缺 lastCompressedSize → None
        assert!(parse_ckpt_state("{").is_none());
        assert!(parse_ckpt_state(r#"{"workspacePath":"x"}"#).is_none());
        // activeUpload 非空 → uploading；空 lastAcceptedManifestHash 视为无
        let json2 = r#"{
            "workspacePath": "/tmp/w",
            "lastCompressedSize": {"encryptedSizeBytes": 5, "manifestHash": "h1"},
            "lastAcceptedManifestHash": "",
            "activeUpload": {"encryptedArtifactPath": "x.enc"}
        }"#;
        let s2 = parse_ckpt_state(json2).expect("应解析成功");
        assert!(s2.uploading);
        assert_eq!(s2.accepted_hash, None);
    }

    /// 接受差分：accepted_hash 变化才计数；recordedAt 早于今日 0 点不计
    /// （跨天去重守卫）；上传起止只出事件；全新一天首扫回补
    #[test]
    fn apply_ckpt_obs_counts_acceptance_diffs() {
        let day_start = 1_000_000i64;
        let mk = |acc: Option<&str>, bytes: u64, rec: i64, up: bool| CkptState {
            workspace: "ws".into(),
            artifact_bytes: bytes,
            artifact_hash: Some("h".into()),
            accepted_hash: acc.map(String::from),
            uploading: up,
            recorded_at: Some(rec),
        };
        let mut states = HashMap::new();
        // 拍 1：基线（无接受）
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(None, 100, day_start - 10, false))], day_start, false);
        assert!(ev.is_empty());
        // 拍 2：接受（recordedAt 今天）→ 计数 + 事件
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h"), 100, day_start + 5, false))], day_start, false);
        assert_eq!(ev, vec![CkptEvent { kind: "accepted", workspace: "ws".into(), bytes: 100, recorded_ms: day_start + 5 }]);
        // 拍 3：无变化 → 无事件
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h"), 100, day_start + 5, false))], day_start, false);
        assert!(ev.is_empty());
        // 拍 4：换新工件并被接受 → 再计一次
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h2"), 250, day_start + 9, false))], day_start, false);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].bytes, 250);
        // 上传开始/结束：事件不计数
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h2"), 250, day_start + 9, true))], day_start, false);
        assert_eq!(ev, vec![CkptEvent { kind: "upload_start", workspace: "ws".into(), bytes: 250, recorded_ms: 0 }]);
        let ev = apply_ckpt_obs(&mut states, vec![("a".into(), mk(Some("h2"), 250, day_start + 9, false))], day_start, false);
        assert_eq!(ev, vec![CkptEvent { kind: "upload_end", workspace: "ws".into(), bytes: 0, recorded_ms: 0 }]);
        // 昨天的接受（recordedAt < 今日 0 点）不计数——跨天去重
        let mut states2 = HashMap::new();
        let ev = apply_ckpt_obs(&mut states2, vec![("b".into(), mk(Some("old"), 999, day_start - 1, false))], day_start, false);
        assert!(ev.is_empty());
        // 全新一天的首扫回补：今天记录的接受要计（count=true）
        let mut states3 = HashMap::new();
        let ev = apply_ckpt_obs(&mut states3, vec![("c".into(), mk(Some("n"), 42, day_start + 1, false))], day_start, true);
        assert_eq!(ev, vec![CkptEvent { kind: "accepted", workspace: "ws".into(), bytes: 42, recorded_ms: day_start + 1 }]);
        // 面板今天运行过（count=false）的首见不回补
        let mut states4 = HashMap::new();
        let ev = apply_ckpt_obs(&mut states4, vec![("d".into(), mk(Some("n"), 42, day_start + 1, false))], day_start, false);
        assert!(ev.is_empty());
    }

    /// 接口计数差分：32 位回绕取模得到正确增量；回退（重置）钳 0；
    /// 巨大异常增量（≥2^31，索引复用/重置误判为回绕）也钳 0
    #[test]
    fn wrap_delta_handles_32bit_wrap_and_resets() {
        const W: u64 = 1 << 32;
        // 正常增量
        assert_eq!(wrap_delta(500, 100, W), 400);
        // 回绕：从 2^32−300 跨零点走到 196，真实增量 = 300 + 196
        assert_eq!(wrap_delta(196, 4_294_967_296 - 300, W), 496);
        // mac（wrap=0）：计数器回退（重置）钳 0，不产生假流量
        assert_eq!(wrap_delta(100, 500, 0), 0);
        assert_eq!(wrap_delta(900, 500, 0), 400);
        // 计数器重置（百万级回退到小值）：按回绕解释会得到 ≥2^31 的假增量，钳 0。
        // 注：回退幅度 <2^31 的重置与回绕在 32 位计数器下固有不可区分
        assert_eq!(wrap_delta(1000, 1_000_000, W), 0);
    }

    /// 进程类型标签（Windows 口径）：CLI 判定在 --type 之前——渲染进程
    /// 命令行里不会出现 zcode.cjs，两类判定不冲突
    #[test]
    #[cfg(windows)]
    fn proc_label_by_command_line() {
        use crate::netio::platform::proc_label;
        assert_eq!(proc_label(r#""C:\...\zcode.exe" "C:\...\zcode.cjs" app-server"#), "CLI 会话进程");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=renderer --field-trial-handle=x"#), "渲染进程");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=gpu-process"#), "GPU 进程");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=utility --utility-sub-type=net"#), "工具进程");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --type=crashpad-handler"#), "崩溃报告进程");
        assert_eq!(proc_label(r#""C:\...\ZCode.exe" --js-flags=..."#), "主进程");
    }

    /// 快照上传记录列表：上传中 > 待传 > 已接受，同状态按时刻倒序；
    /// 不截断——全部工作区都列出
    #[test]
    fn ckpt_rows_sorted_all_workspaces() {
        let mut st = HashMap::new();
        let mk = |ws: &str, bytes: u64, rec: i64, acc: bool, up: bool| {
            (
                ws.to_string(),
                CkptState {
                    workspace: ws.into(),
                    artifact_bytes: bytes,
                    artifact_hash: Some("h".into()),
                    accepted_hash: acc.then(|| "h".to_string()),
                    uploading: up,
                    recorded_at: Some(rec),
                },
            )
        };
        st.insert(mk("old-acc", 100, 1, true, false).0.clone(), mk("old-acc", 100, 1, true, false).1);
        st.insert(mk("new-acc", 200, 9, true, false).0.clone(), mk("new-acc", 200, 9, true, false).1);
        st.insert(mk("pending", 300, 5, false, false).0.clone(), mk("pending", 300, 5, false, false).1);
        st.insert(mk("flying", 400, 3, false, true).0.clone(), mk("flying", 400, 3, false, true).1);
        let rows = ckpt_rows(&st);
        assert_eq!(
            rows.iter().map(|r| r.workspace.as_str()).collect::<Vec<_>>(),
            vec!["flying", "pending", "new-acc", "old-acc"]
        );
        // 不截断：20 个工作区全部列出
        let mut big = HashMap::new();
        for i in 0..20 {
            let (k, v) = mk(&format!("ws{i}"), 1, i, true, false);
            big.insert(k, v);
        }
        assert_eq!(ckpt_rows(&big).len(), 20);
        // 空表 → 空列表
        assert!(ckpt_rows(&HashMap::new()).is_empty());
    }
}
