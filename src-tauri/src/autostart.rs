//! 自动启动（设置弹窗「自动启动」区）：三态 off / boot / follow。
//!
//! - **boot（开机自动启动）**：登录后直接常驻启动，按上次退出时的形态显示。
//! - **follow（跟随 ZCode 启动）**：登录后先**静默待命**（仅托盘图标，不显示
//!   窗口），由后台线程每 2s 检测一次系统中是否存在 ZCode 进程（桌面端
//!   ZCode.exe / CLI 子进程同名，一网打尽），检测到即自动亮出面板；亮出后
//!   不再自动隐藏，检测线程退出。面板被手动退出后不再自动复活（重新登录
//!   或手动打开恢复）。
//!
//! 实现不依赖三方 crate：Windows 直接读写 HKCU Run 注册表值（REG_SZ，
//! 带引号的 exe 全路径 + 可选参数，无管理员权限）；macOS 写
//! ~/Library/LaunchAgents/com.zcode.speedpanel.autostart.plist（手写 XML，
//! RunAtLoad=true）。**注册表/plist 即唯一事实源**——`current_mode()` 读真实
//! 状态回显 UI，不在 speed-panel-mode.txt 里另存一份（避免两处状态漂移）。
//!
//! 重复启动的交互：tauri-plugin-single-instance 的唤起回调会直接 show 已有
//! 实例窗口，`--zcode-follow` 参数只在开机无实例时生效，互不冲突。

/// follow 模式追加到自启动命令行的参数（也用于注册表值回读时区分 boot/follow）
pub const FOLLOW_ARG: &str = "--zcode-follow";

/// 自启动状态在注册表/plist 里的名字（Windows 值名 / macOS Label）
const AUTOSTART_NAME: &str = "zcode-speed-panel";
#[cfg(target_os = "macos")]
const MAC_PLIST_LABEL: &str = "com.zcode.speedpanel.autostart";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutostartMode {
    Off,
    Boot,
    Follow,
}

impl AutostartMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AutostartMode::Off => "off",
            AutostartMode::Boot => "boot",
            AutostartMode::Follow => "follow",
        }
    }
    /// 未知值一律回 Off（手改注册表/删 plist 都会自然落回 Off）
    pub fn parse(s: &str) -> AutostartMode {
        match s.trim() {
            "boot" => AutostartMode::Boot,
            "follow" => AutostartMode::Follow,
            _ => AutostartMode::Off,
        }
    }
}

/// 本次启动是否带了 --zcode-follow（开机 follow 模式的静默待命标志）。
/// tauri-plugin-single-instance 只在无已有实例时才让本进程跑到 setup，
/// 因此该参数不会误伤手动二次启动。
pub fn follow_requested() -> bool {
    std::env::args().any(|a| a == FOLLOW_ARG)
}

/// 系统里是否有 ZCode 进程在跑（桌面端或 CLI 任一即算）。进程枚举复用
/// liveio::platform 的平台原语：Windows 匹配进程名 zcode.exe（桌面壳与 CLI
/// 子进程同名）；macOS 匹配 KERN_PROCARGS2 的 argv[0] 以 /ZCode 结尾（桌面
/// 端）或参数区含 zcode-cli（CLI）。
pub fn zcode_running() -> bool {
    crate::liveio::platform::any_zcode_process()
}

/// 读当前生效的自启动模式（注册表 / LaunchAgent 即事实源）
pub fn current_mode() -> AutostartMode {
    read_registered().map_or(AutostartMode::Off, |cmd| {
        if cmd.contains(FOLLOW_ARG) {
            AutostartMode::Follow
        } else {
            AutostartMode::Boot
        }
    })
}

/// 设置自启动模式：Off 清除，Boot/Follow 写入（Follow 追加 --zcode-follow）。
/// 返回 Err 时前端如实提示（如注册表被组策略锁死）
pub fn set_mode(mode: AutostartMode) -> Result<(), String> {
    match mode {
        AutostartMode::Off => disable(),
        AutostartMode::Boot | AutostartMode::Follow => {
            let exe = std::env::current_exe()
                .map_err(|e| format!("无法定位程序路径: {e}"))?;
            let exe = exe.to_string_lossy().into_owned();
            enable(&exe, mode == AutostartMode::Follow)
        }
    }
}

// ---- Windows：HKCU\Software\Microsoft\Windows\CurrentVersion\Run ----

#[cfg(windows)]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

#[cfg(windows)]
fn read_registered() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(RUN_KEY).ok()?;
    key.get_value::<String, _>(AUTOSTART_NAME).ok()
}

#[cfg(windows)]
fn enable(exe: &str, follow: bool) -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // Run 值按惯例带引号包路径：含空格的安装路径（如 portable 版挪去
    // Program Files）不被 shell 按空格截断
    let cmd = if follow {
        format!("\"{exe}\" {FOLLOW_ARG}")
    } else {
        format!("\"{exe}\"")
    };
    let (key, _) = hkcu
        .create_subkey(RUN_KEY)
        .map_err(|e| format!("打开注册表失败: {e}"))?;
    key.set_value(AUTOSTART_NAME, &cmd)
        .map_err(|e| format!("写入注册表失败: {e}"))
}

#[cfg(windows)]
fn disable() -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY, winreg::enums::KEY_WRITE)
        .map_err(|e| format!("打开注册表失败: {e}"))?;
    match key.delete_value(AUTOSTART_NAME) {
        Ok(()) => Ok(()),
        // 值本就不存在 = 已经是 Off，不算失败（幂等）
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除注册表值失败: {e}")),
    }
}

// ---- macOS：~/Library/LaunchAgents/<label>.plist（手写 XML，无 plist 依赖）----

#[cfg(target_os = "macos")]
fn plist_path() -> Option<std::path::PathBuf> {
    crate::metrics::home_dir().map(|h| {
        h.join("Library")
            .join("LaunchAgents")
            .join(format!("{MAC_PLIST_LABEL}.plist"))
    })
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn read_registered() -> Option<String> {
    let path = plist_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    // 只需回读整条命令行形态（是否含 --zcode-follow），不完整解析 plist
    let mut reassembled = String::new();
    for seg in content.split('<').skip(1) {
        if let Some(v) = seg.strip_prefix("string>") {
            reassembled.push_str(v.split('<').next().unwrap_or(""));
            reassembled.push(' ');
        }
    }
    Some(reassembled.trim().to_string())
}

#[cfg(target_os = "macos")]
fn enable(exe: &str, follow: bool) -> Result<(), String> {
    let Some(path) = plist_path() else {
        return Err("无法定位用户目录".into());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 LaunchAgents 失败: {e}"))?;
    }
    let arg_line = if follow {
        format!("<string>{}</string>", xml_escape(FOLLOW_ARG))
    } else {
        String::new()
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\
<key>Label</key><string>{MAC_PLIST_LABEL}</string>\
<key>ProgramArguments</key><array><string>{}</string>{arg_line}</array>\
<key>RunAtLoad</key><true/>\
</dict></plist>\n",
        xml_escape(exe)
    );
    std::fs::write(&path, xml).map_err(|e| format!("写入 LaunchAgent 失败: {e}"))
}

#[cfg(target_os = "macos")]
fn disable() -> Result<(), String> {
    let Some(path) = plist_path() else {
        return Err("无法定位用户目录".into());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除 LaunchAgent 失败: {e}")),
    }
}

// ---- 其余平台（本项目只发 Windows/macOS 包，这里如实报不支持）----

#[cfg(not(any(windows, target_os = "macos")))]
fn read_registered() -> Option<String> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
fn enable(_exe: &str, _follow: bool) -> Result<(), String> {
    Err("当前平台不支持自动启动".into())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn disable() -> Result<(), String> {
    Err("当前平台不支持自动启动".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip() {
        for m in [AutostartMode::Off, AutostartMode::Boot, AutostartMode::Follow] {
            assert_eq!(AutostartMode::parse(m.as_str()), m);
        }
        assert_eq!(AutostartMode::parse("乱写"), AutostartMode::Off);
    }

    /// 注册表读写端到端（仅 Windows；dev/CI 机跑）。值名固定，测试结束恢复
    /// 用户原值（无则留在 Off），不在测试机器上留下持久副作用
    #[cfg(windows)]
    #[test]
    fn registry_enable_disable_cycle() {
        let saved = read_registered();
        let _ = disable();
        assert_eq!(current_mode(), AutostartMode::Off);

        set_mode(AutostartMode::Boot).unwrap();
        assert_eq!(current_mode(), AutostartMode::Boot);
        let cmd = read_registered().unwrap();
        assert!(cmd.starts_with('"') && cmd.contains(".exe\""), "boot 模式命令行应为带引号的 exe 路径: {cmd}");
        assert!(!cmd.contains(FOLLOW_ARG));

        set_mode(AutostartMode::Follow).unwrap();
        assert_eq!(current_mode(), AutostartMode::Follow);
        assert!(read_registered().unwrap().contains(FOLLOW_ARG));

        set_mode(AutostartMode::Off).unwrap();
        assert_eq!(current_mode(), AutostartMode::Off);

        // 恢复用户原值
        if let Some(prev) = saved {
            use winreg::enums::HKEY_CURRENT_USER;
            use winreg::RegKey;
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(RUN_KEY)
                .unwrap();
            key.set_value(AUTOSTART_NAME, &prev).unwrap();
        }
    }
}
