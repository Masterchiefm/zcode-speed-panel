# 关键规则与踩坑详情

改代码前必读。每条都是本项目真实发生过的事故，含症状与根因；违反会导致难以察觉的功能失效。

## 1. `$()` 是 getElementById，找不到直接 throw

`src/main.ts` 的 `$()` 按 `document.getElementById` 查找，元素不存在时抛 `missing #xxx`。
**模块加载中途抛错 = 前端整体瘫痪且无提示**：metrics/mode 事件监听全部未注册，表现为数值全 0、模式切换失效；而挂在崩溃点之前的监听仍然生效，症状呈现"半坏"假象。
- 事故案例：顶栏是 `<header>` 标签没有 id，`$("header")` 直接炸（已修：`#app-header`）。
- 规则：HTML 新增节点必须带正确 id；改动 main.ts 后必跑 `npx tsc --noEmit` + 浏览器 mock 模式（`npm run dev` 后开 localhost:1420）验证渲染。

## 2. Tauri 2 权限白名单：未放行的窗口 API 一律被拒

前端调用窗口核心 API（minimize / toggleMaximize / startDragging…）必须在 `src-tauri/capabilities/default.json` 的 `permissions` 里显式放行（如 `core:window:allow-minimize`），否则调用被拒绝。
- 事故案例：— / ▢ 按钮无反应——权限缺失，且 `.catch(() => {})` 把报错静默吞掉（已修：补权限）。
- 规则：新增窗口 API 调用时同步加权限；**不要用空 catch 掩盖失败**，至少 `console.warn`。dev 版可 F12 看权限报错。自定义命令（`#[tauri::command]` + `generate_handler!`）不受此限制。

## 3. 实时速度的一致性校准：校准与显示必须同一条清洗流

字节→token 系数的分子必须来自与显示完全相同的清洗流（`build_rows` 产物在调用区间的积分），系统性扣除（噪声底/落盘镜像/突发剔除）才能被系数抵消。
- 事故案例：校准用未清洗总字节（~900-1400 B/token）、显示用清洗流（真值 ~350-550 B/token），系数被抬高 2~3 倍，实时读数只有真值的 1/5（~10 t/s vs 40-50）。
- 规则：改 liveio 采样/清洗逻辑时，`measure()` 内校准积分与显示幅度必须同源；合成端到端测试（`synthetic_call_converges_to_true_tps` 等）是守护，必须保持通过。

## 4. 逐拍清洗禁止钳非负（负拍对消）

落盘 flush 与 IO 计数存在错位：单拍可能出现"文件涨 60KB 但进程只写 20KB"。该拍必须记为负值，靠区间积分（`integrate`）与前后正拍对消；只在窗口汇总处 `max(0, ·)`。
- 事故案例：旧版逐拍 `max(0, delta-落盘)`，错位字节永久丢失，约一半调用读数塌缩到 ~130 B/token。
- 关联：自适应噪声底必须封顶（`FLOOR_CAP_BYTES=2KB`），否则持续流式期间分位数被流式增量毒化，扣掉自身输出。

## 5. 校准样本准入：静默调用会污染系数

相当一部分调用是"管道静默"——生成期间 UI 管道几乎无增量字节，全部在完成瞬间刷出。这类调用的管道积分 ÷ token 可低至 ~7 B/token，入样会把中位数系数拉低、实时读数虚高。
- 规则：样本必须经 `cal_sample` 准入（eff ≥ 300 token、clean ≥ raw 的 20%、B/token 在 [100, 6000] 且未触钳位），拒绝的整条丢弃（日志 `skipped=true`）。准入样本交给**新近加权中位数 + AR1 收缩估计器**（`cal_estimate`：16 样本窗、半衰期 3、两遍法修剪 [0.4, 2.5]、剪后向最新样本做一步对数收缩 ρ=0.3 且比值钳 [0.5, 2]——单样本影响 ≤ ×1.23，离群只拉动一拍、突变首样本先手）产出系数；队列预置 600 先验且窗口不足 3 样本时向先验线性收缩——冷启动单个异常样本无法独占系数。
- 噪声地板的量级与结构（2026-09-24 二轮定标）：本机 262 个真实样本 B/token p25~p75 = 466~740；log 空间 lag-1 自相关 ≈0.50、lag-2 ≈ 0.27 ≈ ρ²（**纯 AR(1)**——连续调用共享内容风格），任何系数预测器的逐调用误差地板 ≈20% 中位数——实时读数偏差两三成属正常波动，不要据此贸然调参。B/token 与真实速度负相关（r ≈ -0.3~-0.5，快调用渲染分批大、每 token 字节少）但**不可利用**：反推需要未知的当调用速度。改估计器必须过基准：真实重放 `python scripts/cal_bench.py`（改动前后对表，不得回退）+ liveio 测试 `benchmark_*`（稳态 AR(1)/突变收敛/离群有界拖动）。已重放否决的方案：分模型校准（两轮验证：GLM-5.3 与 Flash 样本分布一致；deepseek-v4-flash 样本 med≈221 但 n=3，量级差由速度驱动、样本太薄撑不起独立队列）、字节 ≈ a×token + c×时长的两参数模型（逐调用误差 med 48%~90%）、EWMA/全局收缩/更大均窗（半衰期 6）/eff 加权/AR1 界外跳过——无收益或更差，勿轻率重试。

## 6. 调试日志是排查的第一手数据

`~/.zcode/speed-panel-debug.jsonl`（JSONL 追加，8MB 轮转保留一代 `.jsonl.1`，轮转旧文件超 7 天启动时自动清理）记录四类事件：
- `tick`：显示值 `tps`、来源 `src`（io/window/idle）、启动期 `start`、清洗管道字节率 `pipe`、生效系数 `bpt`、曲线尾桶、窗口内达流式量级的进程数 `npids`；
- `call`：调用完成真值（`eff`/`gen_ms`/`true_tps`）；
- `cal`：校准对账（`true_tps` vs `pred_tps`、`raw_kb`/`clean_kb`、`bpt_sample`/`bpt_now`、`skipped`、归因诊断 `attr_pid`/`top_pid`、跨进程守卫诊断 `others_kb`——窗口内其他进程原始字节，与归属进程占比大时样本被拒收）；
- `cal_reset`：重新校准（`reason`=manual/auto、系数前后 `bpt_old`/`bpt_new`；auto 附触发时的轮均值 `round_avg` 与 5 轮基线 `base_avg`——排查"为什么系数突然回先验"看这里）。

实时准确性评估口径：`pred_tps / true_tps` → 1.00 为准。用 `python scripts/live_vs_true.py` 一键对账（≥300 token 且入校准的调用为达标样本）。诊断实时读数问题先看这里，不要靠猜。

## 7. examples 与 src 的编译耦合

`src-tauri/examples/{dump,verify}.rs` 通过 `#[path]` 直接 include `metrics.rs`/`liveio.rs` 编译。改这两个模块的公开 API（结构体字段、函数签名）时，examples 也必须同步更新，否则 `cargo test`/`cargo build --examples` 失败。

## 8. WebView2 原生 `<select>` 弹层跟随系统主题，深色 UI 里不可读

原生 select 的**下拉弹层**由 WebView2 按系统主题渲染：系统浅色时弹层白底，option 又继承了页面里的灰字样式，深色界面下几乎看不见；实测 `:root { color-scheme: dark }` 与 option 显式着色在 WebView2 弹层里均不生效。原生 checkbox/radio 同理（框体按系统浅色主题绘制），深色 UI 里一律自绘。
- 事故案例：悬浮窗样式下拉"看不见字"（已修：整个替换为自绘 `.dropdown`/`.dropdown-list`，与右键菜单同风格深色弹层）；桌宠"常显上轮均速"勾选框错位——`#float-menu button { all: unset; display: block }` 是"ID+元素"选择器（特异性 (1,0,1)），单 ID 的 `#float-menu-pet-last { display: flex }`（(1,0,0)）压不过它，flex 失效后勾选小方框退化为行内零尺寸（已修：改双 ID 选择器 `#float-menu #float-menu-pet-last`，隐藏规则同理再加权）。
- 规则：本项目 UI 需要下拉/勾选框一律自绘，不再新增原生 `<select>`/`<input type=checkbox>`；给带 `all: unset` 的复合选择器规则定义的控件改布局时，覆盖规则的选择器特异性必须高于它（双 ID 或 `#id button#id` 形态）；新增顶栏交互组件时，同步把它加入 enableDrag 拖动与双击最大化的排除选择器（`button, select, input, .dropdown`），否则会误触窗口拖动/最大化（用 `<button>` 承载新控件则天然命中）。

## 9. 启停门控必须用 message 行的 `completed` 字段，不能用 model_usage 完成行

调用启停判定以 usage 库 message 表 assistant 消息行为准：**行在调用开始瞬间提交（≤200ms），行内 data 的 `time.completed` 在结束瞬间补写（取消/出错也会补）**。`model_usage` 行只记 `status='completed'`——cancelled/error 的调用（实测库中 97+119 条）**永远没有完成行**，用完成行判停会卡"生成中"直到 10 分钟兜底；且它不区分会话，新开对话首个调用要等首个完成行落盘才可见（长调用可达数分钟）。
- 事故案例：旧口径"最新 assistant 创建时间 > 最新完成调用的 completed_at 且限最新完成调用的会话"——用户取消生成后面板持续显示"生成中 + 估算值"最长 10 分钟；新会话开聊全程无反应。
- 查询约束：message 表**没有 time_created 单列索引**（全局 `ORDER BY time_created DESC` 实测 ~200ms/次，700ms 轮询不可承受）——必须先取 `session.time_updated` 倒序前几个会话，再走 `(session_id, time_created)` 复合索引按会话查最新 assistant 行。
- 判停兜底（`liveio::stale_stop`）：`completed` 补写落盘可延迟数秒~分钟，期间门控仍开、window 回退持续挂"生成中 + ≈ 上轮速度"（2026-09-17 用户报告"对话停了还显示生成中、慢慢降"）。锚点出现后清洗流断绝 >15s（`SILENT_STOP_MS`）即强制判停；锚点未建立（管道静默调用）不受影响。纯函数测试 `stale_stop_after_silent_window` 守护。
- 守护：`inflight_from_rows`（metrics.rs）为门控纯函数单测（僵尸行/多会话/超龄）；`awaiting_hint`（liveio.rs）守护启动期提示窗口。改门控相关代码时这两个测试必须保持通过。

## 10. mac 平台差异（照搬 Windows 参数会静默失效）

实时测速的平台原语在 `liveio::platform`（win/mac/stub 三份 cfg），清洗/校准参数由 `CleanParams` 平台参数化。以下差异都是实测撞出来的，跨平台改 liveio 前必读：

- **mac 的 burst 即信号，必须禁用 BURST_TICK_BYTES**：Windows 上单拍 >100KB 是请求体上传（应整拍剔除）；mac 上流式本身就是单拍突发形态（实测单拍 +225KB~1.5MB 是常态，0,0,0,+大块 交替），沿用 100KB 阈值会把**全部**流式信号当突发丢掉，实时读数恒 0。mac 侧取 `u64::MAX` 禁用。
- **files 扣除在 mac 是方向性反噬**：Windows 的落盘扣除（tracked 文件增量从写字节中减去）在 mac 必须关闭——CLI 会清理轮转旧 rollout，实测 120s 探针里 rollout 目录 du **净变化为负**，负的文件增量会把清洗流反向抬高（而不是扣除）。mac 的 `tracked_files_total` 恒 0。
- **mac 进程识别不能用 proc_pidpath**：CLI 进程由 Electron Helper fork 而来，`proc_pidpath` 返回的是 `.../ZCode Helper`（与其他 Helper 进程同一路径，无法区分）；`ps` 显示的 "zcode-cli" 是 p_comm。正确口径：`KERN_PROCARGS2` 打包区里扫描独立的 NUL 结尾字符串精确匹配 `zcode-cli`（注意 argv[0] 之后有**对齐 NUL 填充**，不能按 nargs 连续解析，否则读到一堆空串）。曾经按 basename 匹配实现过一版，dump 冒烟 `live可用=false`。
- **FFI 偏移错位用 offset_of! 编译期断言防**：`proc_pid_rusage` 的 `rusage_info_v4` 结构镜像必须逐字段对照 SDK `sys/resource.h`，且用 `offset_of!` 断言 `ri_proc_start_abstime`=80、`ri_diskio_byteswritten`=152（新内核布局在 start_abstime 后多了 `ri_proc_exit_abstime`，老布局记忆是 144——就是这个坑）。断言不过必须修结构排布，**禁止删断言**；另 `#[link(name = "proc")]`（库文件是 libproc.dylib，链接名不带 lib 前缀，写 "libproc" 会 `ld: library not found for -llibproc`）。
- **mac 的磁盘写字节是页缓存异步落盘计数，校准必须延迟宽限**：`ri_diskio_byteswritten` 统计的是脏页实际写盘的字节，滞后 `write()` 数秒~数十秒。2026-09-17 真值对账（6 条 cal 事件，归因正确时 pred_tps 与 true_tps 完全一致 62.1=62.1）的三条证据：① 34s 调用 [start, completed] 窗口只积分到一半字节，样本 186 偏低入队污染中位数；② 1s 小调用里凭空多出 750KB——上一条调用的脏页这时才落盘；③ 117s 长调用 96% 字节没落进窗口，用户盯着 0.7 t/s 两分钟而真值 65.3。守护：`CleanParams.cal_grace_ms`（mac 15s / Windows 0 当拍处理）——pending 校准等满宽限再积分，校准积分与 raw 统计窗口上限延长到 completed+grace（分子分母同口径，pred 分母仍用真实 gen_ms）；`cal_outlier_ratio`（mac 3 倍 / Windows 0 禁用）拒收与生效系数偏差超倍的半截样本；CalEvent 记录 `attr_pid`/`top_pid` 供归因异常定位（多进程并发下偶发 clean≪raw 时看两者是否错位）。同源教训：mac 系数先验曾按 120s 探针取 2000（误判 ≈3900 B/token），真值对账实为 ~650（接受样本 614/724），冷启动 3 倍低估——现为 700。
- **Dock/Cmd+Tab 与退出的上游限制**：Tauri/macOS 上无边框窗口应用保留 Dock 图标，`hide()` 也无法把 Accessory 应用完全"藏起来"；本项目采用 **Accessory 模式**（`set_activation_policy`，setup 内尽早调用）+ 自定义菜单拦截 `Cmd+Q`（菜单不含任何 `PredefinedMenuItem::quit`）+ `RunEvent::ExitRequested { code: None }` 兜底 `prevent_exit`。三条防线合起来才保证"真退出只有托盘退出与悬浮窗右键退出两条路"——只做其中一两条，用户仍可能从系统菜单/快捷键把应用退掉，之后菜单栏入口消失、体验等于"应用丢了"。

## 11. SQLite WAL 锁与长期运行防抖设计

- **busy_timeout 与只读优化**：Engine 打开 `db.sqlite` 必须开启 `OpenFlags::SQLITE_OPEN_NO_MUTEX`，且连接初始化时必须设置 `busy_timeout(3000ms)` 并开启 `PRAGMA query_only = ON;`。否则当 ZCode CLI 高频写事务或 checkpoint 时，读连接会立即报 `database is locked (SQLITE_BUSY)` 丢当拍。
- **进程扫描 buffer 零分配**：macOS 的 `KERN_PROCARGS2` 必须复用 scratch buffer（64KB），禁止在 PID 循环中分配，避免每轮刷新引发 64MB 堆分配毛刺。
- **session_pid 随进程存活淘汰**：liveio 维护的会话-PID 映射在进程轮询检测退出时必须调用 `session_pid.retain` 清理，防止多会话长时间运行累积脏数据与 PID 复用误归因。

## 12. 窗口控制平台原生化与多屏安全最大化（macOS 副屏防跳屏）

- **外观原生化**：macOS 标志性的红黄绿交通灯位于顶栏最左侧（左起：红 `#ff5f56` 折叠悬浮窗、黄 `#ffbd2e` 最小化、绿 `#27c93f` 最大化），悬停显现微小符号（`✕`、`—`、`▢`）；Windows 环境保持右侧 `— ▢ ✕` 自绘按钮不变。前端通过 `navigator.userAgent.includes("Mac")` 为 `body` 注入 `platform-mac` class。
- **副屏最大化防跳屏（`toggle_maximize_safe`）**：无边框窗口（`decorations:false`）在 macOS 下直接调用系统 `toggleMaximize()` 会因为系统 `zoom:` 动作强行跳回主屏。解决方式为 Rust 端 `toggle_maximize_safe`：取窗口中心点所在显示器（`monitor_from_point`），按该显示器物理尺寸铺满（预留顶部系统菜单栏 28pt 避让高度 `(28.0 * scale) as i32`），并在 `AppState.saved_max_rect` 暂存最大化前的物理矩形；再次触发或双击顶栏时还原；在切换到悬浮窗（`switch_mode(Mode::Float)`）时清空暂存，保证状态干净。
- **DOM `dblclick` 与 `startDragging` 互斥（2026-09-28 事故：悬浮窗双击恢复自上线起静默失效两周）**：enableDrag 在 mousedown 调 `startDragging()`，Windows 上进入原生拖动循环吞掉鼠标序列，mouseup 由 tao 在 `WM_EXITSIZEMOVE` 才补发——两对完整 click 湊不齐，DOM `dblclick` 永不触发（9-17 上线的仪表/胶囊双击恢复因此从未生效，加桌宠双击才暴露）。双击类交互必须学 Tauri 内核 `drag.js` 的做法：在 `e.detail>=2` 的 mousedown 上直调命令（内核对 drag region 的双击最大化正是 detail 2 → `internal_toggle_maximize`；本项目悬浮窗为 detail ≥2 → `set_mode full`），该分支同时跳过 startDragging。连击计数按 mousedown 递增、不依赖前一 mouseup 已派发，detail===2 在拖动循环后仍可靠到达（内核双击最大化即依赖此）。

## 13. 应用内更新：CI 产物命名即匹配协议，且失效是静默的

- **产物名即协议**：`updater::pick_asset` 按 Release 资产名后缀匹配本平台安装包：`*_x64-setup.exe`（win-x64，portable 版不参与自动安装）、`*_x64.dmg`（mac-x64）、`*_aarch64.dmg`（mac-aarch64）。改产物命名、加新架构而不同步 `pick_asset` 的后果是**静默的**——检查正常返回但找不到安装包，用户永远收不到更新且没有任何报错（更新模块按设计宁可漏报不打扰，见 updater.rs 模块注释）。改名/加架构必须同一提交内同步 `asset_picking` 测试。
- **版本号三处同步**：`tauri.conf.json`（运行时权威：`package_info().version` 用于显示与比较）、`Cargo.toml`、`package.json`。发版漏 bump `tauri.conf.json` 时新 Release 的 tag 与旧版本号相等 → 判"已是最新"，更新功能同样静默失效。
- **GitHub API 必带 User-Agent**（无 UA 直接拒绝）；403 限流与断网同按"无更新"处理。HTTP 客户端为 ureq（同步阻塞 + rustls，无 OpenSSL，mac 交叉构建友好），全部网络操作在后台线程，失败不触碰 UI。

## 14. 多任务并发的实时链路：单选会话、归因盲区/翻转、求和口径都是坑

多 CLI 进程并发（多窗口、子代理并行）时实时链路曾经的四个静默缺口（2026-09-18 定位，证据为当日 debuglog：主/子代理调用块、attr_pid 翻转、bpt 262~764 摆动）：

- **门控单选**：`inflight_from_rows` 曾只取"最新未完成行"的**一个**会话（`max_by_key`），其余并发任务的流量不进读数——用户观感"sub-agent 没统计进来 / 分不清多个任务"。
- **归因盲区**：会话→进程归属在首个调用完成时才建立（top-writer + raw>20KB）。新会话（含子代理首调用）在归属建立前，显示退回"最近完成调用的会话"的进程——多窗口时那是**别的窗口**的速度。
- **归因翻转污染系数**：归属按调用窗口内 top-writer 更新，两窗口并发流式时逐调用翻转（现场：同会话相邻两次调用归属在两个 pid 间摆动，系数被对方窗口字节污染，读数偏差 2~3 倍）。
- **求和分支双重失真**（存量 bug）：全进程求和曾把 tracked 文件增量**逐进程各扣一遍**（N 倍过度扣除），且把各进程积分的**墙钟时长也求和**（速率被摊薄成跨进程均值而非总吞吐；一进程流式另一进程待机时读数直接减半）。

现行方案与守护（改并发相关代码前必读）：

- 门控返回**全部**进行中会话（`inflight_from_rows` 纯函数测试守护）；显示进程集合 = 归属 pid 并集，任一会话无归属 → None = 全进程求和兜底（`pick_pid_set` 纯函数测试守护，不能漏掉尚无归属的新进程）。
- 聚合流 `merge_streams`：同拍字节求和、**墙钟时长只计一次、tracked 文件增量只扣一次**；单条流输入与"逐行扣文件"的历史算术逐字节等价（`merge_streams_deducts_file_growth_once_and_counts_secs_once` 守护）。校准积分走同一条聚合流（key-rules #3 同源原则在多进程下的延伸）。
- 归属切换迟滞 `should_reattribute`（已有归属时候选进程窗口内字节 ≥ 现归属 2 倍才切，测试守护）；**归属去重 `pick_attribution`**（2026-09-18 二次事故：同一 ZCode 窗口新开任务复用**同一 app-server 进程**、跨项目才分进程——并发平局下 `max_by` 取 top 依赖 HashMap 遍历序，两会话挤到同一 pid 后显示集合塌缩成单进程、任务卡不出。修法：首次归属时 top 已被另一进行中会话占用且次高字节达 top 一半（`ATTR_DEDUP_RATIO=0.5`，并发流 ~1 vs 空闲进程噪声 ~0.1）改归次高；现归属被占用时切换迟滞放宽到一半。测试 `pick_attribution_dedup_on_tie` / `pick_attribution_relaxed_switch_when_owned` 守护）；校准样本跨进程守卫 `cross_pid_ok`（他窗字节 > 基准进程 20% 拒收——基准 = 归属进程；**无归属的全进程求和分支以 top 进程为基准，该分支的积分同样会混入他窗字节，不能放行**；cal 日志 `others_kb` 诊断）；`cal_sample` 的 clean/raw 守卫分子分母**配对**（clean 来自归属进程时 raw 也用归属进程的）。
- **同进程多会话如实合计**：同一 app-server 进程承载的多个会话（同窗口新开任务）在字节层不可拆分——任务行计为一行、`n_sessions` 标注会话数计入任务总数（前端按 Σ会话数 ≥2 触发显示），速度为该进程合计。排查多任务问题直接看 tick 日志的 `pids`（每台被跟踪进程探测窗 KB/s）/`infl`（进行中会话数）/`attr`（会话→pid 映射）三件套，不要只看 npids。
- 轮漂移只吃**单进程轮**（main.rs `round_tps` 第三位记录 saw_multi，任一实测拍 npids>1 整轮跳过；npids = 窗口内贡献达流式量级的进程数，空闲进程的底噪泄漏不计入）——任务数变化带来的吞吐差不是系数漂移，1 任务 40 t/s 与 3 任务 120 t/s 是同一系数，误触发会把好系数重置回先验。

## 15. 网络字节的平台原语限制：按进程直测无公开原语，口径必须分层

背景：netio 最初为区分 ZCode 会话/非会话上传而建（2026-09-17 曾发现 ZCode 静默上传整仓快照到阿里云 OSS；该功能后被 ZCode 下线，netio 的快照工件监测层与快照防护卡已于 2026-09-28 一并移除，但本条平台原语结论仍然成立并约束 netio/liveio 的任何新口径）：

- **Winsock 收发字节不进进程 IO 计数器的任何一项**：curl 下载 20MB 期间 `ReadTransferCount` 仅 7.8KB（`Write` 12MB 全是往 NUL 设备的落盘镜像，`Other` 82KB 是杂项 IOCTL）。=> ① liveio 的 `WriteTransferCount` 流式测速**天然不受网络污染**，勿往里加网络语义；② 想从 IO 计数器反推网络字节是死路。旧文档把"调用开始瞬间 >100KB 写突发"解释为"请求体上传"是误判——实为提示文本写 message 行/rollout 的落盘。
- **TCP ESTATS 已坏（2026-09-18 二次复验归档 `scripts/estats_probe.py`，勿再空手重试）**：`SetPerTcpConnectionEStats`/`GetPerTcpConnectionEStats`（注意**导出名是 EStats 大写 S**，与 MSDN 文档名 Estat 不同，直接 `#[link]` 会 LNK2019，须 LoadLibrary+GetProcAddress）。首验（代码未留档）：所有连接含本进程自有的返回 `ERROR_NOT_SUPPORTED`(50)，管理员也一样。复验（build 26200、普通权限）：v4/v6 共 4 个导出符号全部存在；`Set`（启用 Data 采集）对**自有与他人进程的连接一律 `ERROR_ACCESS_DENIED`(5)**——启用采集是特权操作、与连接归属无关；未启用时 `Get` 恒失败（`ERROR_INVALID_USER_BUFFER`(1784) 居多、个别连接 50），换调用姿势（Get 附 Rw 缓冲 / Set 附 Rod 缓冲）不改变结果。=> 唯一的每连接字节数公开 API 在普通权限下不可用（面板不以管理员为前提），**按进程真实网络速度因此不可实现**，只能走估算口径（整机真值 + 会话 token 估算两层）。
- **ETW 内核网络事件需要管理员**，桌面工具不可依赖。
- **Windows 接口计数器（`GetIfTable` 的 dwIn/dwOutOctets）是 32 位**：必须**逐接口**模 2³² 差分——各接口回绕时机不同，先求和再差分在任一接口回绕后即错（初版实现就是这个 bug，靠测试抓住）。mac `getifaddrs` 的 ifi_*bytes 为 64 位，但**每接口按地址族返回多行，必须按接口名去重**否则字节翻倍。
- **缓存命中的提示不重发**：98% 命中下整机当日上传仅数十 KB——会话上传估算的分子必须用 `input − cache_read`（未缓存部分），按全量重发估算会虚高数十倍。会话下载密度实测 ~731 B/token（整机含杂流上界）/ UI 管道 bpt≈320（下界），估算系数取 400。
- **"与任务管理器对不上"是口径差异，不是计数 bug**（2026-09-18 用户报告过一次）：① 单位——任务管理器为比特（Mbps/Kbps），面板为字节，×8 才可比；② 范围——任务管理器 Wi-Fi 页仅所选适配器，面板为全部非回环接口求和（虚拟网卡/VPN 隧道流量内外层各计一次）；③ 时间——面板 ~1s 滑窗平均（`NET_WINDOW_MS`，对齐任务管理器刷新节奏），突发被摊平且采样时刻不同。整机当日累计只含面板在场时段（重启基线重打）。

=> 现行方案（netio.rs）：整机接口计数（真实）+ 会话 token 估算（≈）两层分层，UI 逐项标注口径；连接归属用 `GetExtendedTcpTable`(OWNER_PID) 按进程分组（命令行含 `zcode.cjs` = 会话组，其余 `zcode.exe` = 桌面端非会话组）。守护测试：`wrap_delta_handles_32bit_wrap_and_resets`（回绕/重置钳 0）、`sess_est_scales_with_tokens`（估算系数契约）。

## 16. mac 窗口全屏/Dock：只信官方身份与默认行为，别跟 AppKit 抠细节（2026-09-19 全屏攻坚定论）

用户要求 mac 绿色交通灯为原生全屏。攻坚过程与教训（一天内多轮实测）：

- **Accessory（菜单栏常驻）应用的窗口永远拿不到原生 Space 全屏**——绿键只给辅助全屏（铺满但菜单栏不隐藏）。`collectionBehavior=FullScreenPrimary`、窗口改不透明、运行时绑 `toggleFullScreen:` 全部无效。**唯一解 = Regular 应用身份**：完整面板 `set_activation_policy(Regular)`（亮 Dock 图标）+ 原生 Overlay 标题栏，绿键即原生全屏；收起悬浮窗切回 Accessory（藏 Dock、应用不退出），两态动态切换已实装。
- **裸 objc FFI 三坑**：① Cocoa 属性 getter 无 `get` 前缀（`collectionBehavior`，写 `getCollectionBehavior` → unrecognized selector → **ObjC 异常穿过 Rust extern "C" 直接 abort 进程**，且第一现场 panic 信息被吞，要用 lldb 断 `panic_cannot_unwind`/读 `~/Library/Logs/DiagnosticReports/*.ips` 的 `lastExceptionBacktrace` 定位）；② 同一 `#[link_name="objc_msgSend"]` 声明多个不同签名会告警并存隐患，fn 指针 transmute 在新 rustc 有运行期检查；③ 运行时创建 ObjC 类做按钮 target/action，回调里再调 tauri 窗口 API + `setPresentationOptions:` 会抛 NSException 崩溃（自建"沉浸全屏"方案因此废弃，代码已剥离）。**结论：与 AppKit 交互只用 tauri 公开 API；官方没有的能力（原生全屏）靠换应用身份解决，不硬造。**
- **窗口实验要能秒回滚**：本轮多组未提交实验靠 `git stash` 一键回到已验证状态；"改完先起 dev 让用户看效果，确认后才 build/push"是固定流程（用户明确要求）。
