//! 快照防护：阻断 ZCode 工作区快照的静默上传（chflags uchg 目录不可变锁）。
//!
//! ## 背景（2026-09 本机验证）
//!
//! ZCode 登录后会把**整个工作区**（含 `.git/` 全历史）打成加密 tar.gz 写入
//! `~/.zcode/v2/checkpoints/<工作区hash>/pending/*.tar.gz.enc`，再经
//! zcode.z.ai 拿凭证直传阿里云 OSS；设置开关无效，且凭证 API 与模型 API
//! 同域，**不能靠封网络解决**。清空该目录后对目录本身 `chflags uchg`
//! （macOS 用户级不可变标志，用户自有目录无需 sudo）即可让 ZCode 写不进
//! 去——快照链路死亡，而模型对话/补全/工具调用完全正常；唯一损失是
//! 「检查点回滚 / 时间线」。`chflags nouchg` 随时可逆，目录留空时 ZCode
//! 会自动重建内容。（机制来源：ferster 博客《ZCode 静默上传工作区快照》）
//!
//! ## 实现要点
//!
//! - **不碰网络、不碰进程**：只做目录文件系统操作（remove/create/chflags，
//!   `std::process::Command` 调系统 chflags），对运行中的 ZCode 无侵入；
//! - **锁定检测 = 写入探测**：在目录里 create+delete 临时文件，创建失败
//!   即已锁。纯 std 实现，比解析 `ls -lO` / libc `st_flags` 干净；
//! - **知情同意在前端**（`#guard-confirm` 确认弹窗必须明示损失检查点回滚，
//!   见 key-rules #16）；apply/release 收到调用即执行、不再二次确认；
//! - **防护计数**：锁定时刻与 calls 基线持久化在
//!   `~/.zcode/speed-panel-guard.json`；poller 每拍按 calls 增量累计
//!   `blocked_rounds`，**基准 calls_seen 一并落盘**——否则重启后内存
//!   last_calls 归零，首拍会把全天计数整包计入（实测 15 分钟虚增至 3412）；
//!   跨天回退按 0 增量重置基准拍；
//! - **目录已锁但无记录**（用户看过文档后手动 chflags / 重装面板）：首拍
//!   探测到即补记基线，从该时刻起算轮次；
//! - **先留档再清空**：apply 删除 checkpoints 前把当时的上传记录行
//!   （每工作区最近一次快照）存入 `~/.zcode/speed-panel-ckpt-history.json`，
//!   防护期间前端可完整回看「防护前的原上传记录」（用户明确要求，
//!   2026-09-18）；重复开启按工作区合并（新记录覆盖同工作区旧行）；
//! - **仅 macOS**：Windows 无等价的用户级不可变标志，`supported=false`，
//!   apply/release 返回中文错误，前端按钮禁用并如实标注。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// chflags 仅 macOS（其他平台 apply/release 拒绝执行）
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// checkpoints 目录扫描节流（poller ~700ms 一拍，不必每拍走文件系统）
const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// 每拍随 metrics payload 推送前端的防护状态（serde camelCase）
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotGuardStatus {
    /// 本平台是否支持文件锁（chflags 仅 macOS；false 时前端禁用按钮）
    pub supported: bool,
    /// checkpoints 目录当前是否被锁定（写入探测失败）
    pub locked: bool,
    /// 锁定时刻（guard.json，epoch ms）；目录已锁但无记录时由 poller 补记
    pub locked_since_ms: Option<i64>,
    /// 防护开启后经过的对话轮次（poller 按 calls_today 增量累计）
    pub blocked_rounds: u64,
    /// 已积累工件数（`**/pending/*.enc`）
    pub artifact_count: u64,
    /// 工件总体积（字节）
    pub artifact_bytes: u64,
    /// 工作区目录数
    pub workspace_count: u64,
    /// Σ failureCount（ZCode 自己记录的上传失败计数）
    pub failure_count: u64,
    /// 防护前的原上传记录（apply 留档；防护期间前端完整回看）
    pub history: Vec<crate::metrics::CkptStat>,
}

/// 单工作区 state.json 里防护关心的字段（解析纯函数的输出）
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StateSummary {
    pub failure_count: u64,
}

/// checkpoints 目录扫描摘要
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ScanSummary {
    pub artifact_count: u64,
    pub artifact_bytes: u64,
    pub workspace_count: u64,
    pub failure_count: u64,
}

/// state.json → 摘要（纯函数，可测）：损坏 JSON / 非对象返回 None（调用方
/// 跳过该工作区的 failure 计数，工件与目录数仍如实统计）；failureCount
/// 缺失按 0
pub(crate) fn parse_state_summary(json: &str) -> Option<StateSummary> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if !v.is_object() {
        return None;
    }
    Some(StateSummary {
        failure_count: v.get("failureCount").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

/// 聚合（纯函数，可测）：逐工作区（state 解析结果 + pending 工件大小列表）
/// → 总摘要。state 损坏（None）的工作区仍计入工作区数/工件数/体积，
/// 只是不贡献 failureCount
pub(crate) fn summarize_scans(scans: &[(Option<StateSummary>, Vec<u64>)]) -> ScanSummary {
    let mut s = ScanSummary::default();
    for (state, enc_sizes) in scans {
        s.workspace_count += 1;
        for sz in enc_sizes {
            s.artifact_count += 1;
            s.artifact_bytes += *sz;
        }
        if let Some(st) = state {
            s.failure_count += st.failure_count;
        }
    }
    s
}

/// guard.json（~/.zcode/speed-panel-guard.json）：锁定时刻 + calls 基线 +
/// 累计轮次 + 最近一拍 calls_seen（重启后增量基准，防全天计数整包计入）。
/// 锁定四字段缺省即"未防护"，不落盘多余键
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct GuardFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    locked_since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    calls_baseline: Option<u64>,
    #[serde(default)]
    blocked_rounds: u64,
    #[serde(default)]
    calls_seen: u64,
}

/// blocked_rounds 增量累计（纯函数，可测）：以**持久化的** calls_seen 为基准
/// （而非内存 last_calls——重启归零会把全天计数整包计入），跨天回退饱和为 0
pub(crate) fn accrue_rounds(blocked: u64, calls_seen: u64, calls_today: u64) -> (u64, u64) {
    (blocked + calls_today.saturating_sub(calls_seen), calls_today)
}

/// checkpoints 下的工作区子目录名校验（纯函数，可测）：白名单字符 +
/// 长度上限——"打开目录"命令按它拼路径，必须拒绝路径穿越（..、斜杠、
/// 绝对路径、隐藏名等统统不放行）
pub(crate) fn valid_hash_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// checkpoints 目录（main.rs 的"打开目录"命令与防护共用）
pub(crate) fn checkpoints_dir() -> Option<PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("v2").join("checkpoints"))
}

fn guard_file_path() -> Option<PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("speed-panel-guard.json"))
}

/// 防护前的原上传记录留档（apply 清空前写入，防护期间可完整回看）
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GuardHistory {
    saved_at_ms: i64,
    rows: Vec<crate::metrics::CkptStat>,
}

fn history_file_path() -> Option<PathBuf> {
    crate::metrics::home_dir().map(|h| h.join(".zcode").join("speed-panel-ckpt-history.json"))
}

fn load_history() -> GuardHistory {
    let Some(path) = history_file_path() else { return GuardHistory::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_history(h: &GuardHistory) {
    if let Some(path) = history_file_path() {
        if let Err(e) = std::fs::write(&path, serde_json::to_string(h).unwrap_or_default()) {
            eprintln!("[zcode-speed-panel] 快照历史留档失败: {e}");
        }
    }
}

/// 历史合并（纯函数，可测）：新扫描的行覆盖同工作区旧行（"每工作区最近
/// 一次"语义），其余工作区保留；按记录时刻倒序，上限 500 行防爆档
pub(crate) fn merge_history(
    old: Vec<crate::metrics::CkptStat>,
    new: Vec<crate::metrics::CkptStat>,
) -> Vec<crate::metrics::CkptStat> {
    let mut map: HashMap<String, crate::metrics::CkptStat> =
        old.into_iter().map(|r| (r.workspace.clone(), r)).collect();
    for r in new {
        map.insert(r.workspace.clone(), r);
    }
    let mut rows: Vec<crate::metrics::CkptStat> = map.into_values().collect();
    rows.sort_by(|a, b| b.recorded_ms.cmp(&a.recorded_ms));
    rows.truncate(500);
    rows
}

/// 损坏/缺失回默认（未防护），不报错——状态探测按目录实际锁定为准
fn load_guard_file() -> GuardFile {
    let Some(path) = guard_file_path() else { return GuardFile::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_guard_file(f: &GuardFile) {
    if let Some(path) = guard_file_path() {
        if let Err(e) = std::fs::write(&path, serde_json::to_string(f).unwrap_or_default()) {
            eprintln!("[zcode-speed-panel] guard 状态落盘失败: {e}");
        }
    }
}

/// chflags uchg/nouchg（std::process::Command，用户自有目录无需 sudo）。
/// 仅在 SUPPORTED 平台被调用。recursive = 连同子目录/文件整树上锁
/// （保留模式必须递归：uchg 只管目录自身的条目表，只锁根目录挡不住
/// 已存在工作区子目录内的写入，见 key-rules #16）
fn set_immutable(dir: &Path, lock: bool, recursive: bool) -> Result<(), String> {
    let flag = if lock { "uchg" } else { "nouchg" };
    let mut cmd = std::process::Command::new("chflags");
    if recursive {
        cmd.arg("-R");
    }
    let st = cmd
        .arg(flag)
        .arg(dir)
        .status()
        .map_err(|e| format!("执行 chflags 失败: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("chflags {}{} 未成功（exit {:?}）", if recursive { "-R " } else { "" }, flag, st.code()))
    }
}

/// 写入探测：目录存在且无法在其中创建临时文件 = 已锁（uchg 阻止在目录内
/// 新建条目）。探测文件随即删除；目录不存在 = 未锁
fn probe_locked(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(".speed-panel-lock-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            false
        }
        Err(_) => true,
    }
}

/// 扫描 checkpoints：每工作区目录读 state.json（损坏跳过）+ pending/*.enc
/// 文件大小，聚合走纯函数 summarize_scans
fn scan_checkpoints(dir: &Path) -> ScanSummary {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return ScanSummary::default();
    };
    let mut scans: Vec<(Option<StateSummary>, Vec<u64>)> = Vec::new();
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let ws = e.path();
        let state = std::fs::read_to_string(ws.join("state.json"))
            .ok()
            .and_then(|t| parse_state_summary(&t));
        let mut enc_sizes = Vec::new();
        if let Ok(rd2) = std::fs::read_dir(ws.join("pending")) {
            for f in rd2.flatten() {
                if f.file_name().to_string_lossy().ends_with(".enc") {
                    if let Ok(m) = f.metadata() {
                        enc_sizes.push(m.len());
                    }
                }
            }
        }
        scans.push((state, enc_sizes));
    }
    summarize_scans(&scans)
}

/// 快照防护状态机：guard.json 内存镜像 + 上一拍 calls + 节流的扫描缓存。
/// poller 每拍 `tick`；apply/release 由 Tauri 命令调用（前端已过确认弹窗）
pub struct SnapshotGuard {
    file: GuardFile,
    /// 上一拍 calls_today（增量累计；跨天回退按 0 增量重置基准拍）
    last_calls: u64,
    scan: ScanSummary,
    last_scan: Option<std::time::Instant>,
    /// 防护前的原上传记录留档（apply 写入；随 status 推给前端回看）
    history: Vec<crate::metrics::CkptStat>,
}

impl SnapshotGuard {
    pub fn new() -> Self {
        let file = load_guard_file();
        // 增量基准从 guard.json 恢复：重启后首拍 delta = 真实增量，
        // 而不是 calls_today - 0（全天计数整包计入 blocked_rounds）
        let last_calls = file.calls_seen;
        SnapshotGuard {
            file,
            last_calls,
            scan: ScanSummary::default(),
            last_scan: None,
            history: load_history().rows,
        }
    }

    /// 供独立 status 命令取当前 calls 口径（不推进计数）
    pub fn last_calls_seen(&self) -> u64 {
        self.last_calls
    }

    fn status(&self, locked: bool) -> SnapshotGuardStatus {
        SnapshotGuardStatus {
            supported: SUPPORTED,
            locked,
            locked_since_ms: self.file.locked_since_ms,
            blocked_rounds: self.file.blocked_rounds,
            artifact_count: self.scan.artifact_count,
            artifact_bytes: self.scan.artifact_bytes,
            workspace_count: self.scan.workspace_count,
            failure_count: self.scan.failure_count,
            history: self.history.clone(),
        }
    }

    /// poller 每拍：写入探测定锁定态 → 维护 blocked_rounds（变化才落盘）→
    /// 节流扫描（5s）→ 组装 status
    pub fn tick(&mut self, calls_today: u64, now_ms: i64) -> SnapshotGuardStatus {
        let Some(dir) = checkpoints_dir() else {
            return SnapshotGuardStatus { supported: SUPPORTED, ..Default::default() };
        };
        let locked = probe_locked(&dir);
        if locked {
            if self.file.locked_since_ms.is_none() || self.file.calls_baseline.is_none() {
                // 目录已锁但没有记录（用户手动 chflags / 面板重装丢档）：
                // 从现在起补记基线
                self.file.locked_since_ms = Some(now_ms);
                self.file.calls_baseline = Some(calls_today);
                self.last_calls = calls_today;
                self.file.calls_seen = calls_today;
                save_guard_file(&self.file);
            } else {
                // calls_today 当日只增；跨天回退（saturating 后为 0 增量）
                // 并重置基准拍，从新一天的计数继续累加。基准用持久化的
                // calls_seen（new() 已恢复进 last_calls），重启不吃全天计数
                let (blocked, seen) = accrue_rounds(
                    self.file.blocked_rounds,
                    self.last_calls,
                    calls_today,
                );
                self.last_calls = seen;
                if blocked != self.file.blocked_rounds {
                    self.file.blocked_rounds = blocked;
                    self.file.calls_seen = seen;
                    save_guard_file(&self.file);
                } else if self.file.calls_seen != seen {
                    self.file.calls_seen = seen;
                    save_guard_file(&self.file);
                }
            }
        } else {
            self.last_calls = calls_today;
            if self.file.locked_since_ms.is_some() {
                // 有记录但目录已解锁（外部解除/手工 nouchg）：清档如实反映
                self.file = GuardFile::default();
                save_guard_file(&self.file);
            }
        }
        if self.last_scan.map_or(true, |t| t.elapsed() > SCAN_EVERY) {
            self.last_scan = Some(std::time::Instant::now());
            self.scan = scan_checkpoints(&dir);
        }
        self.status(locked)
    }

    /// 开启防护（前端已过确认弹窗，keep_files = 用户选择保留/删除现有快照）：
    ///
    /// - **保留模式**（keep_files=true）：上传记录清点后**递归锁定整棵树**
    ///   （`chflags -R uchg`）——快照文件原地保留（加密、只读），列表仍可
    ///   查看与打开；记录未销毁，不写历史留档。必须递归：uchg 只管目录自身
    ///   条目表，只锁根目录挡不住已存在子目录里的写入；
    /// - **删除模式**（keep_files=false）：**先留档再清空**（上传记录行合并
    ///   进 ckpt-history.json，防护期间可回看）→ 重建空目录 → uchg 锁根目录。
    ///
    /// 两条路都过写入探测校验后记录 guard.json（锁定时刻 + calls 基线）
    pub fn apply(
        &mut self,
        calls_today: u64,
        now_ms: i64,
        keep_files: bool,
    ) -> Result<SnapshotGuardStatus, String> {
        if !SUPPORTED {
            return Err("文件锁仅支持 macOS（chflags）".into());
        }
        let dir = checkpoints_dir().ok_or("无法定位用户目录")?;
        // 上代防护可能是递归锁（保留模式），先整树解锁才能改动（幂等）
        if probe_locked(&dir) {
            set_immutable(&dir, false, true)?;
        }
        if keep_files {
            std::fs::create_dir_all(&dir).map_err(|e| format!("重建 checkpoints 目录失败: {e}"))?;
            set_immutable(&dir, true, true)?;
        } else {
            // 留档：清空前把每工作区最近一次快照的记录行存下来（复用 netio
            // 的解析与行构建，口径与实时列表完全一致）
            let (_, obs) = crate::netio::scan_ckpt_states(&dir);
            let states: HashMap<String, _> = obs.into_iter().collect();
            let rows = crate::netio::ckpt_rows(&states);
            if !rows.is_empty() || !self.history.is_empty() {
                let merged = merge_history(std::mem::take(&mut self.history), rows);
                save_history(&GuardHistory { saved_at_ms: now_ms, rows: merged.clone() });
                self.history = merged;
            }
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|e| format!("清空 checkpoints 失败: {e}"))?;
            }
            std::fs::create_dir_all(&dir).map_err(|e| format!("重建 checkpoints 目录失败: {e}"))?;
            set_immutable(&dir, true, false)?;
        }
        if !probe_locked(&dir) {
            return Err("锁定未生效（写入探测仍成功），请检查目录权限".into());
        }
        self.file = GuardFile {
            locked_since_ms: Some(now_ms),
            calls_baseline: Some(calls_today),
            blocked_rounds: 0,
            calls_seen: calls_today,
        };
        self.last_calls = calls_today;
        self.scan = ScanSummary::default();
        self.last_scan = Some(std::time::Instant::now());
        // 保留模式下立即重扫一次，让状态行如实显示"快照已保留 N 个"
        if keep_files {
            self.scan = scan_checkpoints(&dir);
        }
        save_guard_file(&self.file);
        Ok(self.status(true))
    }

    /// 解除防护：递归 nouchg 解锁（兼容保留模式的整树锁）；文件一律不动——
    /// 删除模式目录本就为空，保留模式快照原地恢复可写，ZCode 自动续上。
    /// 清空 guard.json 计数
    pub fn release(&mut self, calls_today: u64) -> Result<SnapshotGuardStatus, String> {
        if !SUPPORTED {
            return Err("文件锁仅支持 macOS（chflags）".into());
        }
        let dir = checkpoints_dir().ok_or("无法定位用户目录")?;
        if probe_locked(&dir) {
            set_immutable(&dir, false, true)?;
        }
        self.file = GuardFile::default();
        self.last_calls = calls_today;
        save_guard_file(&self.file);
        let locked = probe_locked(&dir);
        Ok(self.status(locked))
    }
}

// ============ 测试 ============
#[cfg(test)]
mod tests {
    use super::*;

    /// state.json 解析 + 聚合：failureCount 求和、工件数/体积累计、
    /// 损坏 JSON 容错（跳过的工作区仍计工件与目录数，只是不贡献 failure）
    #[test]
    fn state_summary_parse_and_aggregate() {
        let ok = parse_state_summary(
            r#"{"workspacePath":"/Users/x/proj","failureCount":3,
                "lastCompressedSize":{"encryptedSizeBytes":100,"workspaceSizeBytes":200}}"#,
        )
        .expect("应解析成功");
        assert_eq!(ok.failure_count, 3);
        // failureCount 缺失按 0
        assert_eq!(parse_state_summary(r#"{"workspacePath":"x"}"#).unwrap().failure_count, 0);
        // 损坏 JSON / 非对象 → None（调用方跳过）
        assert!(parse_state_summary("{oops").is_none());
        assert!(parse_state_summary("[]").is_none());

        let scans = vec![
            (Some(ok), vec![100, 50]),                        // 2 个工件 150B，failure 3
            (None, vec![549_000_000]),                        // state 损坏：failure 不计
            (Some(StateSummary { failure_count: 7 }), vec![]), // 无工件的工作区
        ];
        let s = summarize_scans(&scans);
        assert_eq!(s.workspace_count, 3);
        assert_eq!(s.artifact_count, 3);
        assert_eq!(s.artifact_bytes, 549_000_150);
        assert_eq!(s.failure_count, 10);
        assert_eq!(summarize_scans(&[]), ScanSummary::default());
    }

    /// 防护状态字段的序列化契约：camelCase 键名 + guard.json 往返
    /// （前端 SnapshotPayload.guard 依赖键名；guard.json 是跨启动唯一持久化）
    #[test]
    fn guard_status_serializes_locked_fields() {
        let st = SnapshotGuardStatus {
            supported: true,
            locked: true,
            locked_since_ms: Some(1_788_000_000_000),
            blocked_rounds: 42,
            artifact_count: 302,
            artifact_bytes: 302_000_000,
            workspace_count: 23,
            failure_count: 11,
            history: vec![crate::metrics::CkptStat {
                workspace: "proj".into(),
                bytes: 175_400_000,
                recorded_ms: 1_788_000_000_000,
                accepted: true,
                uploading: false,
                hash: Some("ab12cd34".into()),
            }],
        };
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["supported"], true);
        assert_eq!(json["locked"], true);
        assert_eq!(json["lockedSinceMs"], 1_788_000_000_000i64);
        assert_eq!(json["blockedRounds"], 42);
        assert_eq!(json["artifactCount"], 302);
        assert_eq!(json["artifactBytes"], 302_000_000);
        assert_eq!(json["workspaceCount"], 23);
        assert_eq!(json["failureCount"], 11);
        assert_eq!(json["history"][0]["workspace"], "proj");
        assert_eq!(json["history"][0]["recordedMs"], 1_788_000_000_000i64);
        // 未防护默认值：locked=false、时刻为 null（前端按空隐藏"防护后"行）
        let def = serde_json::to_value(SnapshotGuardStatus::default()).unwrap();
        assert_eq!(def["locked"], false);
        assert_eq!(def["lockedSinceMs"], serde_json::Value::Null);

        // guard.json 往返：锁定字段保留（含 calls_seen 增量基准）；空对象全缺省；
        // 默认实例不落多余键
        let f = GuardFile { locked_since_ms: Some(123), calls_baseline: Some(456), blocked_rounds: 7, calls_seen: 456 };
        let round: GuardFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(round, f);
        let empty: GuardFile = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, GuardFile::default());
        assert!(!serde_json::to_string(&GuardFile::default()).unwrap().contains("lockedSinceMs"));
    }

    /// 增量累计：正常增量相加、跨天回退饱和为 0、基准为持久化 calls_seen
    /// （重启场景 delta 只算真实增量，不吃全天计数——事故：15 分钟虚增 3412）
    #[test]
    fn accrue_rounds_counts_real_delta_only() {
        assert_eq!(accrue_rounds(5, 100, 120), (25, 120)); // 正常 +20
        assert_eq!(accrue_rounds(5, 200, 50), (5, 50)); // 跨天回退：0 增量，重置基准
        // 重启：calls_seen 持久化 456，重启后首拍 calls_today 470 → 只 +14
        // （旧 bug：内存归零 → 470 - 0 = +470）
        assert_eq!(accrue_rounds(0, 456, 470), (14, 470));
        assert_eq!(accrue_rounds(0, 0, 0), (0, 0));
    }

    /// 历史合并：新行覆盖同工作区旧行、未知工作区保留、按时刻倒序、
    /// 上限截断（防护前记录"先留档再清空"，key-rules #16）
    #[test]
    fn merge_history_replaces_same_workspace_keeps_rest() {
        use crate::metrics::CkptStat;
        let row = |ws: &str, ms: i64, bytes: u64| CkptStat {
            workspace: ws.into(),
            bytes,
            recorded_ms: ms,
            accepted: true,
            uploading: false,
            hash: None,
        };
        let old = vec![row("a", 100, 10), row("b", 200, 20), row("c", 300, 30)];
        let new = vec![row("b", 900, 99), row("d", 800, 40)];
        let merged = merge_history(old, new);
        let names: Vec<&str> = merged.iter().map(|r| r.workspace.as_str()).collect();
        assert_eq!(names, vec!["b", "d", "c", "a"]); // 时刻倒序，b 已是 900 的新行
        assert_eq!(merged[0].bytes, 99);
        // 上限 500 截断
        let many = (0..600).map(|i| row(&format!("w{i}"), i, 1)).collect();
        assert_eq!(merge_history(Vec::new(), many).len(), 500);
        assert_eq!(merge_history(Vec::new(), Vec::new()), Vec::new());
    }

    /// "打开目录"的目录名白名单：路径穿越（../、斜杠、绝对路径、点开头）
    /// 一律拒绝，只放行 ZCode 生成的哈希形态
    #[test]
    fn valid_hash_name_rejects_traversal() {
        assert!(valid_hash_name("ab12cd34"));
        assert!(valid_hash_name("A-b_C9"));
        assert!(!valid_hash_name(""));
        assert!(!valid_hash_name(".."));
        assert!(!valid_hash_name("a/b"));
        assert!(!valid_hash_name("a\\b"));
        assert!(!valid_hash_name("/etc"));
        assert!(!valid_hash_name(".hidden"));
        assert!(!valid_hash_name("a b"));
        assert!(!valid_hash_name("哈希"));
        assert!(!valid_hash_name(&"x".repeat(129)));
    }
}
