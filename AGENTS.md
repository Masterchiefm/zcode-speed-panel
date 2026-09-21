# AGENTS.md

ZCode 速度仪表盘：Tauri 2 + Rust 桌面工具（Windows / macOS 双平台），实时实测 ZCode CLI 的流式输出速度并统计今日 token 用量，附带网速监控（整机实测 + 会话估算拆分）与「快照防护与上传记录」卡（快照相关 UI 的唯一归处：防护开关 mac chflags / win ACL 拒绝 ACE 阻断工作区快照静默上传 + 今日快照上传 + 上传记录列表）。前端为无框架 TypeScript + Canvas（Vite），后端 Rust 通过 `~/.zcode/cli/db/db.sqlite`（只读）、进程 IO 计数、网络接口计数与 `~/.zcode/v2/checkpoints/` 状态文件取数。

## 命令

```bash
npx tsc --noEmit                    # 前端类型检查（改 src/*.ts 后必跑）
npm run build                       # tsc + vite build
cd src-tauri && cargo test          # Rust 单元测试（41 个，含合成端到端、更新/网络监控/快照防护纯函数）
cd src-tauri && cargo check         # 后端编译检查
npm run tauri dev                   # 开发运行（用户常驻一个 dev 实例，改码会热重启它）
npm run tauri build                 # 正式版（Windows NSIS / macOS dmg，按宿主平台）
# mac 交叉构建 Apple Silicon 包（Intel Mac 上即可）：
rustup target add aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=11.0 npm run tauri build -- --target aarch64-apple-darwin --bundles dmg
python scripts/live_vs_true.py      # 对账：实时读数 vs 落盘真值
python scripts/estats_probe.py      # 复验 TCP ESTATS 每连接字节可用性（key-rules #14 证据；已定论普通权限不可用）
```

## 发版流程（v* 标签 → CI 自动出 Release）

1. **版本号三处同步 bump**：`src-tauri/tauri.conf.json`（运行时权威，更新功能靠它比较）、`src-tauri/Cargo.toml`、`package.json`，再 `cd src-tauri && cargo check` 刷新 `Cargo.lock`；
2. 提交并推送 main（普通推送不触发 CI，省机时）；
3. **打标签推送**：`git tag vX.Y.Z && git push origin vX.Y.Z` —— CI 自动构建并创建 GitHub Release（Windows setup/portable exe + macOS 双架构 dmg，Release 名与占位正文由 build.yml 模板生成）；
4. 构建完成后完善 release note：`gh release edit vX.Y.Z --notes-file <变更摘要.md>`（写本版用户可感的变化；首次发布新架构/改产物命名须同步 `updater::pick_asset`，见 key-rules #12 与 features.md「CI 与发布」）。

注意：漏 bump `tauri.conf.json` 会让新 Release 的 tag 与应用内版本号相等 → 更新功能静默判"已是最新"（key-rules #12）。

## 架构概要

```
src-tauri/src/metrics.rs   数据层：usage 库轮询 + 当日聚合（Engine/Aggregator，纯函数可测）
src-tauri/src/liveio.rs    实时测速：跨平台（platform 子模块：Windows 进程句柄 IO / macOS libproc+rusage）+ 平台参数化清洗（CleanParams）/积分/校准 + 多任务进程集合聚合（pick_pid_set/merge_streams，归属迟滞/跨进程样本守卫）+ 轮均速漂移自动重校准（RoundDrift）
src-tauri/src/netio.rs     网速监控：整机接口计数（win GetIfTable 32 位逐接口模差 / mac getifaddrs 去重）+ 会话/非会话上传拆分（token 估算 ≈ + checkpoints 工件真实下界）+ TCP 连接按进程分组（仅 win）+ 当日累计持久化
src-tauri/src/snapshot_guard.rs  快照防护：锁定 ~/.zcode/v2/checkpoints 阻断工作区快照静默上传（mac chflags uchg / win icacls 拒绝 ACE·SID 必须 whoami 取·故意不拒删——拒 D 连读都会被以 DELETE 打开的工具阻断；写入探测定状态、calls 增量计轮次、guard.json 持久化；双平台）
src-tauri/src/updater.rs   应用内更新：GitHub Releases 检查/下载/安装（纯函数 parse_version/is_newer/pick_asset 可测；产物命名耦合见 key-rules #12）
src-tauri/src/autostart.rs 自动启动：设置弹窗三态（关闭 / 开机自启 boot / 跟随 ZCode follow）；注册表 HKCU Run（winreg，仅 win 目标）/ LaunchAgent plist（mac 手写 XML）即唯一事实源、不落本地配置；follow 靠 `--zcode-follow` 启动参数静默待命 + 2s 轮询 `any_zcode_process`（liveio platform）唤醒（mac 分支未实机验证）
src-tauri/src/main.rs      应用层：轮询线程、更新检查线程、窗口模式/位置持久化、托盘（mac 菜单栏 + 动态 Dock 两态：完整面板 Regular/悬浮窗 Accessory）、DebugLog
src-tauri/examples/        dump/verify 调试工具（#[path] include src，改公开 API 须同步）
src-tauri/capabilities/    Tauri 前端权限白名单（窗口 API 必须在此放行）
src/                       前端：main.ts 装配 + 模块显隐设置（顶栏 ⚙，localStorage modules.v1）+ 设置弹窗「自动启动」区（autostart_get/autostart_set）+ 快照卡记录区 renderSnapshot / gauges.ts 绘制 / model_stats.ts 模型趋势弹窗（图例 chips 多选）/ guard.ts 快照卡防护控制区 / pet.ts 桌宠 / mock.ts 预览
public/pets/               宠物包资源；scripts/*.py 调试日志分析；.github/workflows/ CI
```

数据流：poller 线程每 ~700ms 一拍 → `Engine.poll`（SQLite 增量摄取）→ `Engine.snapshot`（当日聚合 + 90 桶曲线）→ `LiveIo.measure`（进程集合清洗/门控/一致性校准 → 覆写实时值；多任务并发时聚合总吞吐 + 分任务明细）→ `NetIo.tick`（整机接口差分 + 连接归属 + checkpoints 工件事件 → 网络卡字段、会话流量估算与快照卡记录数据）→ `SnapshotGuard.tick`（快照防护锁定探测 + 轮次累计 → guard 字段）→ 托盘状态项更新 → DebugLog（JSONL）→ `emit("metrics")` → 前端渲染。

启动路径（与上面这条轮询链并列）：`--zcode-follow`（autostart.rs follow 模式）时 setup 跳过 `window.show()` 静默待命（只留托盘），检测线程每 2s 查 `any_zcode_process()`、命中即 `show_main` 亮出面板；无参数则照常 `window.show()`。

## 规则与踩坑（全在 docs，AGENTS 不留副本）

全部关键规则与踩坑案例集中在 **[docs/key-rules.md](docs/key-rules.md)**，改代码前必读。速览：`$()` 启动崩溃、Tauri 权限白名单、一致性校准口径、负拍对消、校准样本准入、调试日志排查法、examples 编译耦合、原生 select 弹层不可读、启停门控用 message 行 completed 字段（禁用 model_usage 完成行）、mac 平台差异（burst 即信号/files 扣除反噬/进程识别口径/FFI 偏移断言/退出兜底/延迟落盘宽限）、SQLite WAL 锁与长期运行防抖（busy_timeout/query_only/扫描 buffer 复用/session_pid 淘汰）、更新功能与 CI 产物命名/版本号三处同步的耦合、多任务并发实时链路（门控全量会话/进程集合聚合/归属迟滞+并发去重——同一 ZCode 窗口新开任务复用同一 app-server 进程、跨项目才分进程/求和口径——文件增量只扣一次、时长只计一次）、网络字节平台原语限制（socket 不进进程 IO 计数/ESTATS 已坏/32 位接口计数逐接口模差/缓存命中不重发——按进程直测无公开原语，netio 分层口径）、窗口控制平台原生化与多屏安全最大化、快照防护知情同意（锁定/删除是破坏性动作必须先过确认弹窗、明示损失检查点回滚——不能封网络，唯一解是目录写入锁：mac chflags / win 拒绝 ACE 故意不拒删）——共 16 条，每条含事故案例与守护措施。

## 约定

- 提交信息用中文，首行概括根因/行为。
- mac 为原生 Overlay 标题栏（真·系统交通灯，绿点=原生全屏——只有 Regular 应用身份才给全屏，Accessory 恒为辅助全屏）；Dock 两态：完整面板亮出图标（Regular）、收起悬浮窗自动隐藏（Accessory，应用不退出）。Windows 无系统标题栏：顶栏自绘（`#app-header` + `data-tauri-drag-region`，左侧为 `app-icon.png` 应用图标）；点 ✕ = 收起为悬浮窗，退出走托盘/右键菜单。mac Cmd+Q 也折叠为悬浮窗——真退出只有托盘"退出"与悬浮窗右键"退出程序"（三条防线：自定义菜单无 quit 项、ExitRequested 兜底、prevent_exit）。
- 平台差异集中在 `liveio.rs` 的 `platform` 子模块与 `CleanParams`（Windows/mac 参数表见 `docs/features.md`），改清洗逻辑须核对两平台口径；FFI 结构镜像必须带 `offset_of!` 编译期断言（见 key-rules #10）。
- UI 下拉一律自绘（`.dropdown`），禁用原生 `<select>`——WebView2 弹层跟随系统浅色主题，深色界面里看不见字（key-rules #8）；顶栏新增交互组件须加入拖动/双击排除选择器。
- 仪表配色：速度表分档色定义在 `src/gauges.ts` 顶部 `SPEED_TIERS`（六档：0–40 绿 / 40–80 黄绿 / 80–160 黄 / 160–240 橙 / 240–320 红 / 320+ 品红，整弧换色不分段，背景轨道恒灰），主表、迷你仪表、"上轮"角标小表（`BadgeGauge`）与胶囊/桌宠的上轮读数共用（`speedColor()` 统一取色）；浮动窗口尺寸改动须同步 `main.rs` 的 `FLOAT_*_SIZE`、`docs/features.md` 与 README。桌宠窗口为"正方形精灵区 + 顶部 `PET_BUBBLE_RESERVE` 气泡预留带"，多任务（≥2 进程，3 拍防抖）期间再向上加高 `PET_TASK_EXTRA`——尺寸口径分布在 `apply_mode`/`set_float_size`/`apply_pet_size` 三处（`pet.ts` 按画布短边定位精灵区），改其一须同步其余；气泡向上生长、精灵不缩小。
- CI 不随推送自动触发（省机时）：出包走 `v*` 标签（自动发 Release：Windows exe + macOS 双架构 dmg）或 Actions 页手动 Run workflow（Artifacts：windows / macos-x86_64-apple-darwin / macos-aarch64-apple-darwin）；改动 workflow 触发逻辑须同步 README 与 `docs/features.md`。**发版 = 三处版本号同步 bump（`tauri.conf.json` 权威 / `Cargo.toml` / `package.json`）+ `v*` 标签**——应用内更新按 Release 资产名后缀匹配安装包，build.yml 产物命名与 `updater::pick_asset` 是同一协议，改其一须同步另一个（key-rules #12）。
- 完整面板与悬浮窗位置各自独立记忆（`~/.zcode/speed-panel-mode.txt`）；悬浮窗尺寸用逻辑像素，物理换算走 `scale_factor()`，多屏定位必须 `clamp_to_screen`。
- 完整面板模块显隐与排序在顶栏 ⚙ 设置（localStorage `modules.v1`；默认显示仪表盘/网速监控/曲线，**快照防护卡默认隐藏**）。每模块包一层 `.module-wrap[data-module]`（`display:contents`，卡片仍是 main 的 flex 项）——**新增面板模块须同步**：index.html 加包装层 + main.ts 的 `MODULE_DEFS`/`MODULES_DEFAULT_ORDER`/`MODULES_DEFAULT_HIDDEN`，详见 features.md「模块显隐与排序」。
- 自动启动状态**不落本地配置文件**：Windows 注册表 `HKCU\…\Run` / macOS LaunchAgent plist 即唯一事实源，设置界面回读真实状态（手改注册表、删 plist 如实反映），不在 `speed-panel-*.txt` 里另存一份——避免两处状态漂移。平台差异集中在 `autostart.rs` 的 `#[cfg]` 分支，mac 分支未实机验证（见 features.md「自动启动」）。

## 文档索引（按需阅读）

| 文档 | 内容 |
|---|---|
| [docs/key-rules.md](docs/key-rules.md) | 关键规则与踩坑详情（事故案例、症状、守护测试） |
| [docs/features.md](docs/features.md) | 功能详情：统计口径、实时速度回退链、悬浮窗/桌宠行为、图表参数、自动启动、应用内更新、日志、CI |
| [README.md](README.md) | 面向用户的功能说明与实时测速原理 |
| [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) | 第三方素材与许可 |

## 文档维护（每次改码必做）

**任何程序修改（行为、功能、UI、口径、参数、文件格式）必须在同一提交内同步更新文档**：

- 改功能行为/参数/交互 → 更新 `docs/features.md`；改架构/规则/坑 → 更新本文件与 `docs/key-rules.md`；
- 用户可见的变化 → 更新 `README.md`；
- 新增调试/分析脚本 → 在本文件命令区与 `docs/features.md` 登记。

文档与代码不一致视为改动未完成。提交前自查：`git diff` 里的每处行为变化，是否都有对应文档改动。
