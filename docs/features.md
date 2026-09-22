# 功能详情

当前已实现功能的精确行为与口径。改功能前先核对此文档（它应与代码同步更新）；用户视角的说明见 `README.md`。

## 数据与统计口径

- **今日总量** = `input + output + reasoning + cache_creation`（与 ZCode 官方统计同口径）；**缓存命中 cache_read 是提示复用，不计入总量**，以命中率展示：`cache_read ÷ (input + cache_creation)`。注意 usage 库的 `input` 本身就是全部提示 token、已含缓存命中的部分（`raw_usage_json` 中 `totalTokens = inputTokens + outputTokens`，全库 `cache_read ≤ input`、`cache_creation = 0` 可证），分母不能再加 cache_read，否则重复计数、命中率被摊薄约一半（曾把 98% 显示成 49%）。
- **平均速度 avg_tps** = Σ(output+reasoning) ÷ Σ纯生成时长；分母用 `completed_at - first_token_at`（排除首 token 等待/排队），`first_token_at` 缺失退化为 `duration_ms`，下限 50ms。
- **上一轮速度 last_call_tps** = 完成时刻最晚的那条调用的 (output+reasoning) ÷ 其纯生成时长（与平均速度同口径、但只看最后一条，非实时 IO 读数）；今日无已完成调用时为 0。取"完成时刻最晚"而非 ingest 顺序末条，DB 查询排序变化不影响结果。
- 今日归属按调用完成时刻，跨天自动清零（rollover）。
- 数据源：`~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表，只读打开（WAL 不影响运行中的 CLI）。

## 实时速度（liveio.rs）

- 30s 滑窗 ∩ 活跃段的清洗管道字节率 ÷ 自校准系数（字节→token）。
- **多任务并发聚合（多窗口 / 子代理并行）**：门控返回**全部**进行中会话；实时速度 = 各会话归属进程**并集**上的聚合流（`merge_streams`：同拍字节求和、墙钟时长只计一次、tracked 文件增长整体只扣一次）——当前读数是真实的**总吞吐**，不再是"最新一条未完成行的那个会话"。任一进行中会话尚无归属记录（新会话/子代理的首个调用未完成过）时退化为**全进程求和**兜底（`pick_pid_set` 返回 None：不能漏掉新进程、显示成别的窗口的速度；空闲进程经底噪清洗贡献 ≈ 0）。同进程内并行的多个子代理在字节层不可拆分，按该进程合计计。
- **平台原语**（`liveio::platform`，按 cfg 三份实现：win / mac / stub，examples 复用同一路径）：
  - **Windows**：Toolhelp32 枚举 + `zcode.exe` 命令行含 `zcode.cjs` 过滤；`GetProcessIoCounters` 的 `WriteTransferCount` 累计写字节；tracked 文件（rollout/日志/WAL）总量扫描供落盘扣除。
  - **macOS**：`proc_listallpids` 枚举 + `KERN_PROCARGS2` 命令行参数精确匹配 `zcode-cli`（CLI 由 Electron Helper fork 而来，可执行路径与其他 Helper 相同，proc_pidpath 无法区分，实测 CLI 进程参数区有独立的 `zcode-cli` 串）；`proc_pid_rusage(RUSAGE_INFO_V4)` 的 `ri_diskio_byteswritten` 累计磁盘写字节，句柄持有打开时抓取的 `ri_proc_start_abstime` 防 pid 复用（不一致视为进程退出剔除）；**不做 tracked 文件扣除**——实测 rollout 目录 du 净变化可为负（CLI 清理轮转），负增量会反噬清洗流，且恒 0 免去每拍目录扫描。rusage_info_v4 为逐字段 `#[repr(C)]` 镜像，关键字段偏移（start_abstime=80、diskio_byteswritten=152）用 `offset_of!` 编译期断言钉死，SDK 布局变化直接编译失败。
  - **其他平台**：stub（空列表/None），面板回退窗口/估算显示。
- **CleanParams 平台参数表**（清洗/校准参数化，`CleanParams::platform()` 启动时锁定；Windows 列为长期实测原值禁改，mac 列为 120s 探针实测初值**须实测复核**）：

  | 参数 | Windows | macOS | 原因 |
|---|---|---|---|
  | burst_tick_bytes（突发剔除） | 100_000 | u64::MAX（禁用） | mac 流式即单拍突发（225KB~1.5MB 常态），100KB 阈值会丢弃全部信号 |
  | base_noise_bps（静态底噪） | 3_000 | 0 | mac idle 实测 17s 严格 0 字节 |
  | floor_cap_bytes（自适应底噪封顶） | 2_000 | 2_000 | 封顶只防毒化；mac idle 恒 0 时自适应自行降 0 |
  | default_bpt（系数先验） | 600 | 700 | mac 真值对账（2026-09-17，6 条 cal 事件）接受样本 614/724；旧值 2000 源自探针误判，冷启动 3 倍低估 |
  | cal_min / cal_max（样本区间） | 100 / 6_000 | 100 / 12_000 | mac 覆盖对账实测 ~650 留余量 |
  | cal_min_tokens（样本门槛） | 300 | 300 | 平台无关 |
  | detect_ms（锚点探测窗） | 2_500 | 2_500 | 首版不动；mac 若状态抖动再调 5_000 |
  | cal_grace_ms（延迟落盘宽限） | 0（当拍处理） | 15_000 | mac 的 ri_diskio_byteswritten 是页缓存异步落盘计数，滞后 write() 数秒~数十秒（实测 117s 调用 96% 字节落在 completed 后）；调用完成后等满宽限再积分，校准积分与 raw 统计窗口上限同步延长到 completed+grace，分子分母同口径 |
  | cal_outlier_ratio（样本离群拒绝） | 0（禁用） | 3.0 | 样本 B/token 与当前生效系数偏差超 3 倍即拒收：延迟落盘的半截样本（实测 186 偏低入队污染中位数）与归因异常样本不进中位数 |

- **启停门控**：usage 库 message 表的 assistant 消息行——调用开始瞬间提交（≤200ms 可读），行内 `time.completed` 在结束（**含取消/出错**）瞬间补写。扫描最近活跃会话（`session.time_updated` 倒序前 16 个——多任务聚合要覆盖全部进行中会话，>6 个并发子代理不能漏计，各自走 `(session_id, time_created)` 复合索引取最新 assistant 行）：其中最新行未带 `completed` 且 10 分钟内的**全部**会话视为进行中（按开始时刻降序，多任务并发各自计入；新开对话首个调用当拍即亮）；带 `completed` → 当拍归零。进行中会话的归属进程**全部**退出时强制判停（崩溃后无人补写 `completed` 的僵尸行兜底；无归属的会话不判死）。**判停兜底（`stale_stop`）**：CLI 补写 `completed` 可延迟数秒~分钟，期间 window 回退会一直挂着"生成中 + ≈ 上轮速度"——流式锚点出现后清洗流速（探测窗口径）持续低于流式阈值达 15s（`SILENT_STOP_MS`）即判定生成已停、读数归零；锚点未建立的管道静默调用不受影响，误判时字节恢复当拍自愈。断流后复流（判停触发过）重新起锚，30s 滑窗从新一段起算不被静默段稀释。
- **分任务明细（`snapshot.tasks`）**：流式期间按 CLI 进程逐个给出实时速度（`LiveNow.tasks`；文件增长按各进程**正**字节占比分摊——负拍进程不参与分摊、数值钳 0，保证明细之和不因负拍偏大）。**任务总数 = Σ各行 `n_sessions`（未归属行计 1）**，≥2 时完整面板在仪表行与曲线之间显示"并发任务"卡：状态点复用 live/idle 样式、数值按 `SPEED_TIERS` 分档着色、会话标签为归属会话 id 尾 6 位（无归属显示"未归属进程 pid"；**同进程承载多会话计为一行合计**，标注"N 会话（同进程合计）"——同一 ZCode 窗口新开任务会复用同一 app-server 进程，字节层不可拆分）；连续 3 拍（~2s）任务总数不足 2 才隐藏（1↔2 边界防抖）。**桌宠**气泡在任务总数 ≥2 时展开分任务行（每进程一行：会话尾 6 位/N会话/进程 pid + 分档着色速度，非流式显示"待机"，后端同时把桌宠窗口向上加高 96px 给行让位，见"悬浮窗与桌宠"）；**迷你仪表/胶囊仍只显示聚合值**；`live_source=window`（≈ 回退）、启动期（TTFT）与待机时 tasks 为空。归属带并发去重（`pick_attribution`：top 进程已被另一进行中会话占用且次高字节达一半时改归次高，防止平局错归把两会话挤到同一 pid，见 key-rules #13）。
- **启动提示（is_starting）**：门控已开但首字节未到（TTFT，20s 窗口内；多会话并发时取**最新开始**的会话起算）→ 表盘/迷你仪表/胶囊/桌宠气泡显示 **"…"**（青色呼吸脉冲弧），不显示估算值；超窗仍无字节 → 视为管道静默调用，回退 ≈ 估算。
- 状态来源 `live_source`：`io`（实测流式）→ `window`（门控判定生成中但管道静默，显示近期已完成调用的真实速度，前端加 ≈ 标记）→ `idle`（归零）。
- **IO 不可用时**（进程从未发现：探测环境不可用/刚启动）按门控显示估算或"统计中"，而不是按调用间隔盲估；**CLI 全部退出后**立即归零（旧行为按间隔中位数可空转"估算中"最长 240s）。
- 首字节后读数当拍可用（此前为 TTFT "…" 提示）；进程发现：常驻 30s 刷新，无任何进程、存在尚无归属记录的进行中会话（新开的第二个窗口——兜底期间读到的是别的窗口的速度）、或 ≥2 个进行中会话（多任务并发，归属去重需要尽快看到新进程的字节分布）时缩短到 2s。
- 系数冷启动为 600（mac 700）先验，完成 1~2 个 ≥300 token 的调用后收敛；管道静默调用不入样（见 key-rules #5）。**系数样本持久化**：队列随变化（入样/重校准）落盘 `~/.zcode/speed-panel-cal.json`（`updated_ms` + `samples`），重启/热重启热启动（生效系数 = 恢复后队列上中位数，与校准路径同口径；值域外样本拒收），无文件/损坏/超 14 天（模型换代后旧样本即过期噪声）回先验重新收敛——此前每次重启从先验 600 起步，系数真值偏离先验的会话（实测高速档 ~160）重启后读数偏低 2~3 倍、收敛约 25 分钟。重校准后队列 [先验] 同样落盘，"重校准意图"跨重启保留。
- **重新校准（手动 + 漂移自动）**：`LiveIo::reset_calibration` 丢弃已学习的系数样本、回到平台先验的冷启动状态（pending 调用保留以维护会话→进程归属，其旧量级样本在滑动窗口下 1~2 轮即被新样本挤出）。① 手动：完整面板当前速度卡左上角 ⟳ 按钮（`recalibrate` 命令）；② 自动（轮均速漂移，`liveio::RoundDrift`）：一轮 = 门控"进行中"信号连续的一段（相邻调用间无空拍则并为一轮），轮均值 = 该段内 `is_live` 且 >0 的显示速度算术平均（管道静默/估算轮无实测拍、不参与）；上轮均值与之前连续 5 轮均值差异 ≥3 倍（双向，`DRIFT_ROUNDS`/`DRIFT_RATIO`）判定量级突变（换模型/分词器，旧系数过期）自动触发，触发后漂移历史清空重新积累。**多进程聚合轮不参与漂移检测**（轮内任一实测拍覆盖 >1 个进程即整轮跳过）：任务数变化带来的吞吐差不是系数漂移，1 任务 40 t/s 与 3 任务 120 t/s 是同一系数。两类触发均写 `cal_reset` 调试日志并向前端发 `recalibrated` 事件（按钮闪 ✓）。
- **延迟落盘宽限与离群拒绝（mac）**：调用完成后 pending 校准事件等满 15s 再处理（`cal_grace_ms`，Windows=0 当拍处理），校准积分与 raw 统计窗口上限同步延长到 `completed + 15s`（`stream_start` 下限不变，仍为 completed − min(gen_ms, 300s)；pred_tps 口径不变，分母仍用真实 gen_ms）；样本 B/token 与当前生效系数偏差超 3 倍即拒收（`cal_outlier_ratio`，Windows=0 禁用），防延迟落盘半截样本与归因异常样本污染中位数。cal 日志含 `attr_pid`（clean 积分实际用的进程，全进程求和分支为 null）与 `top_pid`（raw 最大进程）供归因异常定位。
- **归属与样本准入的并发守卫**：会话→进程归属带**切换迟滞**（`should_reattribute`：已有归属时，仅当候选进程在调用窗口内原始字节 ≥ 现归属的 2 倍才切换，杜绝并发窗口间逐调用翻转；现归属进程窗口内零字节时直接采信 top 自愈；归属建立门槛为窗口原始字节 > 20KB）；校准样本另带**跨进程守卫**（`cross_pid_ok`：窗口内其他进程原始字节 > 基准进程的 20% 即拒收——对方窗口并发流式时积分必然混入外来字节。基准 = 归属进程；无归属的全进程求和分支以 top 进程为基准，该分支同样不能放行），cal 日志 `others_kb` 供诊断；`cal_sample` 的 clean/raw≥0.2 守卫分子分母**配对**（clean 来自归属进程时 raw 也用归属进程的字节，不用全进程 raw）。
- 精度：达标调用 `pred/true` 应在 0.8~1.25（实测 1.00~1.05）；对账命令 `python scripts/live_vs_true.py`。

## 网速监控（netio.rs）

完整面板「网速监控」卡（默认排在仪表行下方；显隐与顺序可在顶栏 ⚙ 设置中调整，见「模块显隐与排序」节。单行三段横排：速度居左 · 进程连接数居中 · 今日上传/下载最右，窄窗口自动换行；`main` 另有兜底滚动；接口计数不可用的平台整卡隐藏）。监控 ZCode 的上传/下载流量并区分**会话流量**（CLI 进程承载的 API 对话流量）与**非会话上传**（Electron 桌面端的快照上传等），背景是 2026-09-17 的发现：ZCode 会把整个工作区（含完整 `.git/` 历史）打包加密上传到阿里云 OSS（单个工件实测 549MB）。**2026-09-20 起快照相关的一切 UI（防护开关 / 今日快照上传 / 快照上传记录列表）移入下方「快照防护与上传记录」卡**——模块名与内容对应，本卡只留网络流量本身；快照层的取数口径不变（仍由 netio.rs 供数）。

### 为什么是分层口径（平台限制）

**Windows 非管理员下没有"按进程的网络收发字节"公开原语**，2026-09-18 本机实验定论（详见 key-rules #15）：

- Winsock 收发字节**不进** `GetProcessIoCounters` 的任何计数（20MB 下载期间 Read 仅 7.8KB）——进程 IO 计数器只含文件/管道/设备，liveio 的流式测速因此天然不受网络污染；
- TCP ESTATS（`Set/GetPerTcpConnectionEStats`，唯一的每连接字节 API）不可用——2026-09-18 复验归档（`python scripts/estats_probe.py`，build 26200、普通权限）：v4/v6 共 4 个导出符号都存在，但 `Set`（启用 Data 采集）对自有/他人连接一律 `ERROR_ACCESS_DENIED`(5)（启用需特权、与连接归属无关），未启用时 `Get` 恒失败（`ERROR_INVALID_USER_BUFFER`(1784) 居多、个别连接 `ERROR_NOT_SUPPORTED`(50)），调用姿势变体（Get 附 Rw / Set 附 Rod 缓冲）无效；
- ETW 内核网络事件需要管理员。

因此按三层诚实分层，前端 UI 上逐项标注口径：

| 层 | 内容 | 口径 |
|---|---|---|
| 整机上传/下载速度 + 当日总量 | 接口计数器求和（排除回环） | **真实值** |
| 会话流量（当日上传/下载） | token 数 × 字节系数 | **估算 ≈** |
| 非会话上传（快照工件） | checkpoints `state.json` 的接受事件 | **真实下界** |

### 整机实测（真实值）

- **Windows**：`GetIfTable` 全部非回环接口的 `dwIn/dwOutOctets` 求和。计数器为 **32 位**，必须**逐接口**做模 2³² 差分（各接口回绕时机不同，先求和再差分会错——测试 `wrap_delta_handles_32bit_wrap_and_resets` 守护）；单接口单拍增量 ≥2³¹ 视为计数器重置/索引复用，钳 0。`MIB_IF_ROW` 镜像关键字段偏移（dwInOctets=552 / dwOutOctets=556）编译期断言。
- **macOS**：`getifaddrs` 求和 `ifi_obytes/ifi_ibytes`（64 位不回绕，裸差分、回退钳 0），排除 `lo0`；每接口按地址族返回多行，**必须按接口名去重**否则字节翻倍。`if_data64` 镜像 ibytes=64/obytes=72 编译期断言（按 xnu SDK 布局，mac 侧换 SDK 后若断言失败须实测复核）。
- 速度 = ~1s 滑窗差分（对齐任务管理器 ~1s 的刷新节奏；比旧 10s 窗读数更跳，属预期）；当日累计跨天清零、跨重启持久化续算（`~/.zcode/speed-panel-net.json`，30s 节流落盘 + 退出强制保存）。
- **整机口径的含义（UI 显式标注）**：整机 = **本机全部应用**的网络流量（含系统/浏览器/代理隧道加密开销），**非仅 ZCode**——速度值标签写明"全部应用"、卡片右上角注明"整机 = 本机全部应用流量（非仅 ZCode）"、速度块悬停 tooltip 有完整说明。它回答"这台机器今天上传了多少"，不回答"其中多少是 ZCode"（后者由下两层回答）；本机走本地代理时（ZCode → 127.0.0.1 代理 → 外网）尤其须注意隧道开销的放大。
- **与任务管理器对比**：任务管理器网卡页显示**比特**（Mbps/Kbps，×8 ≈ 字节）且仅统计**所选适配器**；本面板为**字节**、全部非回环接口求和（虚拟网卡/VPN 隧道流量内外层各计一次，可大于单网卡读数）且为 ~1s 滑窗平均（突发被摊平、采样时刻也不同）——两边瞬时读数对不上是口径差异而非计数错误，UI tooltip 已注明。

### 会话流量（估算 ≈）

由 usage 库当日 token 数 × 字节系数得出（`sess_bytes_est` 纯函数，前端带 ≈ 标注）：

- **上传** ≈ 未缓存提示 token × 5 B/token。分子用 `input + cache_creation − cache_read`（**缓存命中的提示部分不重发**——实测 98% 命中下整机当日上传仅数十 KB，按全量重发估算会虚高数十倍）。
- **下载** ≈ 输出+思考 token × 400 B/token（SSE 事件流密度；2026-09-18 实测标定：流式期整机下载 ÷ token ≈ 731 B/token 为上界、UI 管道系数 bpt≈320 为下界，取居中值）。

### 非会话上传 = 快照工件（真实下界）

轮询 `~/.zcode/v2/checkpoints/*/state.json`（2s 一拍）：

- **接受事件**：`lastAcceptedManifestHash` 变化 = 快照工件被服务端接受，按 `lastCompressedSize.encryptedSizeBytes`（加密压缩后字节）计入当日累计；`recordedAt` 早于本地今日 0 点的不计（跨天去重守卫——同一哈希永远归属其记录当天）。**当日已接受工件带名单**（workspace/字节/记录时刻，封顶 100 条随 `speed-panel-net.json` 持久化、跨重启/回补保留）——状态行「今日 N 个」下方逐行列出工作区名单（按工作区聚合，件数 >1 时附「n 个 · 字节小计」；该工作区最新工件越近排越前——回补扫描顺序不保证按时间，取组内最大 recordedMs 排序；悬停名单行看该工作区今日逐条明细），复制/导出报告同样包含。
- **上传进行中**：任一 workspace 的 `activeUpload` 非空 → 状态行脉冲提示「快照上传进行中」。
- **回补**：当日首次启动时，把 recordedAt 在今天、但面板未在场观测到的接受按当前 `lastCompressedSize` 回补计入（面板今天已运行过则只建基线不回补）。
- **口径为面板观测期**：面板未运行期间的接受只在上述回补时计入；两拍之间的多次跳变按末态计（下界）。
- **目录状态**：`ok` / `missing`（无目录）/ `blocked`（不可读——用户用 ACL 封锁 checkpoints 后的如实显示，此时监控不到新上传）。
- 事件同时写调试日志（`kind:"net"`，`ev`=ckpt_accepted / ckpt_upload_start / ckpt_upload_end，含 `mb` 与 `ws` 工作区名）。
- **快照上传记录列表**（快照防护与上传记录卡内，2026-09-20 从网络卡右栏移入）：每个 workspace 一行 = 记录时间（今天 HH:MM，跨天 MM-DD HH:MM）/ 工作区名（workspacePath 末段）/ 加密后大小 / 状态（**上传中 ⬆ / 待传 / 已接受 ✓**），排序 = 上传中 > 待传 > 已接受（同状态按记录时刻倒序），**固定显示 5 行（行高 18px：5×18 + 4×2 间隙 = 98px 限高），其余列表内上下滚动看完**（不把页面整页撑开；`ckpt_rows` 纯函数，测试 `ckpt_rows_sorted_all_workspaces` 守护）。列表读的是 checkpoints 实况（state.json 只保留各工作区最近一次工件，更早历史不可考），面板未运行期间的最后状态启动即见。
- **复制与导出**：列表文字可选中复制（全局 `user-select:none` 的例外区）；列表头有 **复制 / 导出** 按钮——复制把整份纯文本报告（表头 + 逐行记录 + 当日汇总 + ZCode 两组连接实况）写入剪贴板（`navigator.clipboard`，失败退回 `execCommand`），导出走 `export_text_file` 命令写入 `~/.zcode/speed-panel-exports/zcode快照上传记录-日期-时间.txt`（文件名白名单清洗防路径穿越，右下角轻提示完整路径；浏览器预览模式退化为浏览器下载）。

### ZCode 连接归属（真实值，仅 Windows）

TCP 连接表（`GetExtendedTcpTable` OWNER_PID，v4+v6，仅 ESTABLISHED）按进程分组。**两组都是 ZCode 自身进程的连接，不含其他应用**（界面标签即"ZCode 会话进程 / ZCode 桌面端"，悬停标题有完整说明）：

- **会话组（ZCode 会话进程）**：命令行含 `zcode.cjs` 的 CLI（app-server）进程——对话 API 流量的承载者；进程发现口径与 liveio 一致，5s 刷新一次 pid 分组，连接表每拍枚举；
- **桌面端组（ZCode 桌面端）**：其余 `zcode.exe` = ZCode 桌面端的 Electron 壳（主/渲染/GPU/工具/崩溃报告进程）——快照上传、遥测等**非对话**流量的承载者。

每条连接都标注**归属进程**（类型标签 + pid，按 Electron `--type` 参数区分：CLI 会话进程 / 主进程 / 渲染进程 / GPU 进程 / 工具进程 / 崩溃报告进程，`proc_label` 纯函数、测试 `proc_label_by_command_line` 守护）；两组各行显示按 远端+pid 去重的条数，**悬停 tooltip 逐条列出 `远端 ip:port · 进程类型(pid)`**。**证据链用法**：整机上传速度飙升 + 桌面端组出现新连接 + activeUpload = 快照上传正在发生的现场证据。mac 侧连接归属未实现（面板隐藏该行，接口计数仍可用）。**两组连接不显示各自的速度**：按进程网络字节在非管理员下无公开原语（见上"平台限制"，ESTATS 已复验定论），真实速度只有整机层可测。

## 快照防护（snapshot_guard.rs + src/guard.ts）

完整面板的「**快照防护与上传记录**」卡（默认排序在网络监控卡下方；**默认隐藏——在顶栏 ⚙ 设置中勾选后才显示**，见「模块显隐与排序」节）：**阻断 ZCode 工作区快照的静默上传**，同时是快照相关内容在界面上的唯一归处——防护控制区（徽标/按钮/统计，guard.ts 渲染）+ 今日快照上传状态行 + 快照上传记录列表（main.ts 的 `renderSnapshot` 渲染，数据仍来自 netio.rs；2026-09-20 从网络卡并入）。后端无 `guard` 字段（浏览器预览/mock）时防护控制区隐藏、只看记录。

- **机制**：ZCode 登录后会把整个工作区（含 `.git/` 全历史）打成加密 tar.gz 写入 `~/.zcode/v2/checkpoints/<工作区hash>/pending/*.tar.gz.enc`，经 zcode.z.ai 拿凭证直传阿里云 OSS；设置开关无效，凭证 API 与模型 API 同域**不能封网络**（2026-09 本机验证，[机制分析文章](https://blog.ferstar.org/posts/zcode-silent-workspace-snapshot-upload/)）。防护 = 目录写入锁——macOS `chflags uchg` 不可变标志（用户级，无需 sudo）/ Windows NTFS 拒绝 ACE（`icacls /deny *<SID>:(OI)(CI)(WD,AD)`，拒绝优先于一切允许：当前用户在树内创建/写入全被拒、读取不受影响）——ZCode 写不进去，快照链路死亡。**不碰网络、不碰进程**，对运行中的 ZCode 无侵入。
- **两种开启模式**（确认弹窗二选一，2026-09-19）：
  - **保留并锁定**（推荐）：现有快照**原地保留（加密、只读）**，**递归锁整棵树**（mac `chflags -R uchg`——uchg 只锁目录自身的条目表，只锁根目录挡不住已存在子目录内的写入；win 可继承拒绝 ACE 由系统自动传播到已存在子树，等价递归；key-rules #16）。上传记录照常实时显示、行尾 📂 可打开快照目录；
  - **删除并锁定**：先留档记录清单再清空全部快照，重建空目录后锁根目录——原始上传记录随之消失（仅剩留档），确认弹窗明示。
  - 共同代价：损失**「检查点回滚 / 时间线」**（state.json 无法更新）；对话/补全/工具调用不受影响；随时解除（保留模式快照原地恢复可写，删除模式空目录由 ZCode 自动重建）。
- **卡片口径**：
  - 状态徽标 🔒 已防护（绿色描边）/ 🔓 未防护，由**写入探测**判定（在目录里 create+delete 临时文件，创建失败 = 已锁；目录不存在 = 未锁）；
  - 数据行三态：未防护 = `本地已积累加密快照 N 个 · X · 覆盖 M 个项目 · 上传失败 K 次`（N = `**/pending/*.enc` 逐文件实测，K = 各 `state.json` 的 `failureCount` 求和；扫描 5s 节流，**保留模式锁定后只读扫描照常工作**）；防护中·快照已保留 = `防护生效中（快照已保留）：N 个只读锁定（共 X）…`；防护中·快照已删除 = `防护生效中（快照已删除）：目录已清空并锁定…（留档 N 条可回看）`；
  - 防护中追加 `防护开启后 P 轮对话 · 新快照落盘 0 个`——P 为锁定后的对话轮次：锁定时刻的 `calls_today` 基线存 `~/.zcode/speed-panel-guard.json`，poller 每拍按 calls 增量累计并落盘。**增量基准 `calls_seen` 同样落盘并在启动时恢复**（只存内存时重启归零，首拍把全天计数整包计入——实测 15 分钟虚增至 3412；差分走纯函数 `accrue_rounds`，跨天回退按 0 增量重置基准拍）；目录已锁但 guard.json 无记录（用户手动锁定 / 重装面板）时首拍补记基线。
- **与上传记录联动**：
  - "快照上传记录"行尾 **📂 按钮**：在系统文件管理器中打开该工作区的快照目录（`open_checkpoint_dir` 命令，mac `open` / Windows `explorer`，**跨平台**；目录名经 `valid_hash_name` 白名单校验防路径穿越，目录不存在如实报错）。只有磁盘上真实存在的行（实时扫描）才渲染——留档历史行没有 hash 不出按钮；
  - 防护中：保留模式横幅 `🔒 以下快照已锁定保留（只读）` + 实时行照常显示且可点开；删除模式（扫描为空）= 🔒 锁横幅 + **防护前留档行**（`~/.zcode/speed-panel-ckpt-history.json`，状态「已上传 ✓」整体压暗，旧版本未留档时退回今日名单）；平时列表空 = "暂无快照记录"灰字——列表区任何状态不静默空白；
  - 今日快照状态行（`ckpt-info`）locked 时今日量行标注「均为防护开启前的记录」（防护生效的宣告在上方防护统计行，不重复 🔒；防护开启后不可能再有"上传中"脉冲）；
  - 复制/导出报告在防护中（删除模式）追加「防护前原上传记录」节（取证仍完整）。
- **先留档再清空（删除模式）**：apply 删除 checkpoints 前，把当时的上传记录行（每工作区最近一次快照，复用 netio 的解析与行构建，口径与实时列表一致）合并存入 ckpt-history.json——同工作区新行覆盖旧行、按时刻倒序、上限 500 行（`merge_history` 纯函数，测试守护）；重复开启/解除再开启不丢历史。保留模式不写留档（记录未销毁）。
- **防护原理与监控边界 hover**：卡片标题悬停展示机制说明（先落盘再上传 → 锁目录 = 断链路）+ 如实声明监控边界（锁状态每拍探测 / 已知机制下 0 新快照为阻断证据 / 整机流量兜底但 mac 无按进程归属，换直传机制只能靠流量异常发现）——不承诺 100% 拦截。
- **术语**：用户可见文案统一「快照 / 加密快照」，不用内部黑话「工件」（2026-09-18 用户反馈"不要自己取名字"）。
- **命令**：`snapshot_guard_status` / `snapshot_guard_apply(keepFiles)` / `snapshot_guard_release` / `open_checkpoint_dir(hash)`（均注册在 generate_handler）；状态另随 metrics payload 的 `guard` 字段每拍附带。apply(keep=true) = 整树锁（快照保留）；apply(keep=false) = 留档 → 清空 → 重建 → 锁根目录；release = **整树解锁**（mac 递归 nouchg / win 移除拒绝 ACE 含子树继承副本，兼容保留模式的整树锁）+ 清空计数，**文件一律不动**。**确认弹窗在前端**（`#guard-confirm`）——双模式按钮：「保留快照并锁定」（主按钮）/「删除快照并锁定」（红色 danger 样式）；删除模式的五点知情同意缺一不可：损失检查点回滚 / 对话不受影响 / 原始上传记录随之消失 / 自动备份仅清单（快照文件等明细删后不可恢复）/ 可逆（key-rules #16）。
- **平台**：macOS / Windows 双平台支持（win 为 NTFS 拒绝 ACE，FAT32/exFAT 无 ACL 时 icacls 如实报错）；其他平台卡片仍显示但按钮禁用、标题右侧标注"文件锁仅支持 macOS / Windows"（沿用连接明细的如实降级先例）。**📂 打开目录是独立命令，两平台均可**（记录列表本身跨平台）。**Windows 与 mac 的锁强度差异**：拒绝 ACE 只拒创建/写入（WD/AD），故意不含删除（D/DC）——实测拒 D 连纯读取都会被以 DELETE 权限打开文件的工具（git-bash 的 POSIX unlink 模拟、部分编辑器/沙箱层）阻断；因此 Windows 下 ZCode 上传成功后的例行清理仍可移走旧的 pending 快照（不产生新泄露），mac 的 uchg 则连删都挡（key-rules #16）。
- **守护测试**：`state_summary_parse_and_aggregate`（failureCount 求和 / 快照体积累计 / 损坏容错）、`guard_status_serializes_locked_fields`（状态字段 camelCase 序列化契约 + guard.json 往返）、`accrue_rounds_counts_real_delta_only`、`merge_history_replaces_same_workspace_keeps_rest`、`valid_hash_name_rejects_traversal`（路径穿越拒绝）、`parse_whoami_sid_finds_sid_field`（SID 解析）、`windows_icacls_lock_roundtrip`（win 真实 icacls 全生命周期：锁后创建/改写被拒且读取照常、解锁全恢复）。

## 仪表与曲线（gauges.ts）

- **当前速度表**：最小量程 60 t/s（`minScale` 可按表覆盖）；卡片左上角有 **⟳ 重新校准按钮**（口径见"实时速度"节），与右上角"上轮"角标呼应；**平均速度表**：最小 10 t/s；**今日总量表**：最小量程 1 亿 token，超峰值后自动放大（1亿→2亿→5亿→…），回落缓慢收缩。
- **角标小表**（`BadgeGauge`，绝对定位卡片顶角、不占主表布局，56×56）：**当前速度卡右上角** = "上轮" `last_call_tps`（最近一次已完成调用速度）；**今日平均卡左上角** = "最高" 近 7 天最高单调用速度；**今日平均卡右上角** = "历史" 近 7 天平均速度。**"近 7 天"窗口**：按本地自然日统计、含今日，共 7 日（`metrics.rs` `HIST_WINDOW_DAYS`）——每日一桶累计 Σeff/Σgen/峰值，每日零点滑动过期（最早的整日桶整体丢弃，峰值随出窗日消失、平均按剩余桶重算；窗口起点走日历日回退 `hist_window_cutoff`，DST 安全）；实现为按日分桶而非纯累加器，就是为了过期时能把旧调用从 Σ 与峰值里退出去。均为落盘口径，读数与弧线按同一 `SPEED_TIERS` 分档变色，量程最小 60 t/s（与当前速度表同量程，半环=30 t/s 即绿黄分界）；无数据时为灰色 0，不做脉冲/估算态（落盘值没有"统计中"）。悬停有口径说明（`title`）。尺寸受卡片顶角留白约束：窗口最小宽度 720px 时主表弧线已贴近顶角，放大需同步放宽卡片内边距。同类小环亦用于迷你仪表悬浮窗（46px，见"悬浮窗与桌宠"）。
- **当前速度表分档配色**（`gauges.ts` 顶部 `SPEED_TIERS`，主表、迷你仪表与角标小表共用）：背景轨道恒灰，整条进度弧随当前速度所在档**整体**换色——六档全覆盖（部分极速模型远超 100 t/s，故上限开放）：0–40 绿 `#34d399` / 40–80 黄绿 `#a3e635` / 80–160 黄 `#fbbf24` / 160–240 橙 `#fb923c` / 240–320 红 `#f87171` / 320+ 品红 `#e879f9`，大数字同步变色；待机（0）数字保持默认白，估算态仍为琥珀 ≈，不走分档。
- 量程跟随峰值平滑变化（峰值上涨立即放大、回落指数收敛），估算时指针/弧线变琥珀色并加 ≈；**启动期（is_starting）数字显示 "…" 并以青色短弧呼吸脉冲**（已连接、等待首字节），不走分档色。
- **输出速度曲线（四档时间范围）**：下拉可选 15 分钟（默认）/ 1 小时 / 6 小时 / 24 小时（localStorage `chartRange.v1` 记住）。统一 90 桶：10s / 40s / 4min / 16min 一档，横轴刻度 5min / 10min / 1h / 4h。15 分钟档走 metrics payload 的今日 spark（内存聚合，实时尾桶由后端混入）；更长档位走 `chart_stats` 命令直接只读查询 usage 库现算（前端 5s 拉取 + 实时速度混入最新桶，与 15 分钟档尾桶口径一致）。前端按 `nowMs % bucketMs` 相位连续左移；横轴标注**真实墙钟时刻** + 右缘当前时刻，可与数据直接对表。卡片头部拨杆可与**模型速度趋势**视图互斥切换（见下节）。

## 模型速度趋势（metrics.rs `model_stats` + src/model_stats.ts）

- **入口与布局**：曲线卡头部右侧的**分段拨杆**（`#chart-view-toggle`，"整体曲线 / 模型详情"文字分居左右、淡青色块滑到激活一侧——配色与下拉选中态同款，不用实色高亮）在两视图间**互斥切换**；模型视图内嵌在曲线卡内（图例 chips + 按模型折线 + 统计条），不再使用弹窗。视图选择存 localStorage（`chartView.v1`，默认整体曲线），跨重启记住。激活期间每 5s 拉取、每 1s 按墙钟相位平移重画；收起为悬浮窗时 main 整体隐藏，拉取自动跳过（回到完整面板自动恢复）。
- **统计范围与整体曲线共用同一下拉**（15 分钟 / 1 小时 / 6 小时 / 24 小时，`chartRange.v1` 记住）：切换拨杆只换序列、**不改变统计时间范围**；范围档变化时模型视图立即重拉（`refresh()`）。前端把当前范围与网格间隔经 `getRange()` 传入（单一事实源在 main.ts 的 `CHART_RANGES`）。
- **桶粒度**：与整体曲线完全同规格——统一 90 桶，`bucket_ms = 窗口 ÷ 90`（15min→10s、1h→40s、6h→4min、24h→16min）；桶序号 = `now ÷ bucket_ms − completed_at ÷ bucket_ms`（div_euclid，**绝对墙钟槽对齐**，与 chart_stats / 今日 spark 同口径：桶边界钉在真实时刻的整倍数上，同一槽内两次查询桶内容完全一致——趋势随时间只平移不变形）；0 = 最新桶、89 = 最旧，越界（含未来跨槽）丢弃；无调用的桶 tps = 0。
- **每模型曲线**：桶 tps = Σ(output+reasoning) ÷ Σ纯生成秒；gen_ms 口径与全局一致（`completed_at − first_token_at`，first_token 缺失或非正退 `duration_ms`，下限 50ms）。y 轴 0~`niceCeil(峰值×1.25)`（与整体曲线同口径量化，数据微变不致整条曲线纵向缩放；3 条横网格线 + 刻度）；x 轴与 `gauges.drawSpark` **完全同一映射**——右缘 = 下一墙钟桶边界，网格线取整分时刻（间隔同整体曲线：5min / 10min / 1h / 4h）+ 右缘「现在 HH:MM」，可与数据直接对表；两视图横轴逐像素对齐，切换拨杆时刻度不动。折线配色按序取 8 色调色板循环，图例中模型名超 18 字符截断加 …（完整名放 title）。
- **每模型统计条**：模型名 + `均速（全窗口 Σeff ÷ Σgen_s）· 峰值（各桶 tps 最大值）· 调用次数 · token（=Σeff，附占比 = 该模型 eff ÷ 全部模型 eff）`；series 按 total_tokens 降序排列。
- **图例 chips 多选**：图例每模型一个自绘 chip（色点 + 名字，深色按钮风格），点击切换该模型显隐——折线与底部统计行同步过滤，chip 配色取模型在完整列表中的原始序号（隐藏再显示颜色不变）；**至少保留一个**：全取消时自动回到全选。图例行尾有「全选」「仅 Top3」（按 total_tokens 前三，模型不足 3 个时等价全选）两个小文字按钮。选中集合持久化在 localStorage（`modelStats.visible.v1`，存可见模型名数组；查不到 / 模型已全部不存在时回退全选；5s 轮询刷新期间选中集合在内存保持，新出现的模型默认可见，不被旧存档静默隐藏）。
- **数据源与零存储**：直接只读查询 `~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表（`status='completed' AND completed_at >= now − 窗口`），Rust 现算聚合、内存缓存最近一次结果；**不给该库建索引/写入，也不新增任何本地存储**。前端仅在拨杆切到模型视图期间每 5s `invoke("model_stats", { windowMin })` 拉取（windowMin = 当前范围档），切走即 clearInterval。浏览器预览（无 Tauri）由 `mock.mockModelStats` 按同口径现算模拟数据。
- **测试**：`aggregate_model_stats` 为纯函数（`metrics.rs`），两模型两桶聚合、窗口 clamp 与 gen_ms 退化、**墙钟对齐守护**（同一绝对槽内两次 now 桶内容逐字段一致——回归守护：曾按 `(now − completed)` 相对偏移分桶，桶边界随查询时刻漂移导致曲线每拍微变形）各有单测守护。

## 模块显隐与排序（顶栏 ⚙ 设置，main.ts）

完整面板 main 内的四个模块可在设置弹窗里整块开关与排序（顶栏 ⚙ 齿轮按钮，复用通用弹窗头样式 `.model-modal-head`/`.model-modal-title`；齿轮为内联 SVG，同 ⟳ 按钮的理由——符号字形在字体里居中不可靠）：

| 模块（data-module） | 内容 | 默认 |
|---|---|---|
| gauges 仪表盘 | 当前速度 / 今日平均 / 今日总量三块仪表；**并发任务卡跟仪表盘走**（出现在其下方，不单列开关/排序） | 显示 |
| net 网速监控 | 整机上/下行速度 · ZCode 连接归属 · 今日累计 | 显示 |
| chart 输出速度曲线 | 整体速度曲线（四档时间范围）/ 模型速度趋势，拨杆互斥切换 | 显示 |
| guard 快照防护与上传记录 | 防护开关 · 今日快照上传 · 上传记录列表 | **隐藏**（快照相关内容显式开启才出现） |

- **交互**：每行 = 自绘勾选框（`.pet-chk` 结构同款，勾选态走本组件的 `.on` 类；禁原生 checkbox——与 key-rules #8 原生 select 同理，浅色弹层不可读）+ 模块名/描述 + ↑↓ 排序按钮（首/末行相应方向禁用）。**隐藏行同样参与排序**——重新勾选时回到原位。另有「恢复默认」一键还原。变更即时生效（弹窗不关闭，遮罩半透明可直接看背后变化）。Esc / 点遮罩空白 / ✕ 关闭；悬浮窗模式下弹窗隐藏（`body.float-mode` 兜底）。
- **持久化**：localStorage `modules.v1` = `{order, hidden}`（order 为**含隐藏模块**的全局顺序）。读取时清洗：JSON 损坏回默认、未知 id 剔除、缺失模块按默认序补到末尾、去重——手改或旧版本升级不丢模块。
- **实现口径**：每模块包一层 `.module-wrap[data-module]`，`display: contents` 不参与布局——卡片仍是 `main` 的直接 flex 项，卡片间距（gap 14px）与曲线卡的 `flex:1` 拉伸口径不变；`applyModules()` 按配置把 wrapper 依次 `insertBefore` footer（footer 恒在最后）并设 `hidden`（文件顶部的全局 `[hidden]!important` 兜底）。**全部模块隐藏时显示占位提示**「所有模块都已隐藏 · 点顶栏 ⚙ 重新开启」（不静默空白，同 key-rules #16 精神）。网速/快照卡自身还带数据可用性的 hidden（`renderNet`/`renderSnapshot`），与模块开关相互独立、取交集显示。
- 模块开关只影响完整面板；悬浮窗/桌宠/胶囊与状态栏不受影响。

## 自动启动（autostart.rs）

设置弹窗（顶栏 ⚙）的第二个分区，与「显示模块」并列，三态单选：**off**（关闭，默认）/ **boot**（登录后常驻启动，按 `~/.zcode/speed-panel-mode.txt` 里上次的形态显示）/ **follow**（登录后静默待命，检测到 ZCode 即亮出面板）。**注册表 / plist 即唯一事实源**——不写任何本地配置文件，界面每次打开回读真实状态；手动改注册表或删 plist 都会如实落回 off（`AutostartMode::parse` 未知值一律 Off），避免两处状态漂移。

| 平台 | 落点 | 内容 |
|---|---|---|
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，值名 `zcode-speed-panel` | REG_SZ = `"<exe 全路径>" [--zcode-follow]`（带引号，免安装版挪到含空格路径后不被截断；免管理员；`winreg` 是仅 Windows 目标的依赖） |
| macOS | `~/Library/LaunchAgents/com.zcode.speedpanel.autostart.plist` | 手写 XML（无 plist 依赖）：`ProgramArguments` = exe +（follow 时）`--zcode-follow`，`RunAtLoad=true`；off = 直接删除该文件 |

- **follow 模式**：自启动命令行带 `--zcode-follow`，`main.rs` setup 见到该参数**跳过 `window.show()`**（只留托盘常驻、不亮窗），另起检测线程——先歇 3s 避开登录高峰，之后每 2s 调 `liveio::platform::any_zcode_process()`，命中即 `show_main` 并退出线程（亮出后不再自动隐藏）。面板**手动退出后不会自动复活**（重新登录或手动打开恢复）——单实例架构的自然边界，设置界面已写明。
- **进程判定口径**（`any_zcode_process()`，liveio.rs platform 子模块）：Windows 用 Toolhelp32 枚举按**进程名 `zcode.exe`** 匹配——**故意不读命令行**，桌面壳与 CLI 子进程同名，名字命中即算，比 `discover_cli_pids` 更快也更宽；macOS 按 `KERN_PROCARGS2` 的 argv[0] 以 `/ZCode` 结尾（桌面端）或参数区含 `zcode-cli`（CLI，与 `discover_cli_pids` 同口径）。
- **与单实例的关系**：`--zcode-follow` 只在开机无实例时生效——带参启动撞上已有实例会被 `tauri-plugin-single-instance` 拦下并走唤起回调直接 show 旧窗口，所以手动二次启动不会误判为静默待命。
- **前端**：三行复用 `settings-row` + `.pet-chk` 自绘勾选（单选语义）；写入失败行内报错并回滚选中态；弹窗加 `max-height` + 滚动防小屏溢出；浏览器预览（无 Tauri）只展示不可写。
- **测试**：`parse_roundtrip`（三态 + 未知值回 Off）与 Windows `registry_enable_disable_cycle` 端到端（真实读写 HKCU，测完恢复原值、不留副作用）。
- **边界**：macOS 分支（plist 写入与 `any_zcode_process` 的 KERN_PROCARGS2 判定）**只过 `cargo check`、未实机验证**——LaunchAgent 直接执行 `.app` 内的二进制而非走 `open -a`，应用身份与 Dock 两态是否与手动启动一致需实机确认；Windows 侧注册表三态与 follow 唤醒已实机验证。

## 悬浮窗与桌宠

- 三形态：**桌宠**（默认精灵区 200×200，窗口为 200×256——顶部 56px 气泡预留带 `PET_BUBBLE_RESERVE`；**多任务（≥2 进程，连续 3 拍防抖）期间自动向上加高 96px**（`PET_TASK_EXTRA`，位置同步上移保持精灵底边不动，约 2s 收回），默认尺寸下可容纳 实时+6 任务+上轮 共 8 行；改桌宠窗口尺寸口径须同步 `apply_mode`/`set_float_size`/`apply_pet_size` 与 `pet.ts` 排版；默认宠物鲸鱼女仆 maid-deepseek-whale）/ **迷你仪表**（148×118，主环弧底距窗口下边 8px 与右上角小环 top:8px 对称、无底部空隙；主环量程与完整面板当前速度表一致：最小 60 t/s）/ **速度胶囊**（172×72）。完整面板右上角**自绘下拉**切换（深色弹层，点击选项/外部、Esc、窗口失焦均关闭；原生 `<select>` 因 WebView2 弹层跟随系统浅色主题不可读而弃用，见 key-rules #8），选择持久化。
- **上轮均速（落盘口径）在各形态的呈现**：完整面板 = 当前速度卡右上角角标小环（56px）；**迷你仪表** = 主环（116px 宽画布）右上角的小环（46px，top:8px；窗口 148×118 非正方形——主环画布收窄 + 速度数字字号缩小（`MiniGauge` 字号系数 0.46）给小环让位，主环弧底在 `MiniGauge.draw` 里按距画布底 8px 锚定、与小环 top:8px 对称，改窗口尺寸须同步 `main.rs` 的 `FLOAT_GAUGE_SIZE`、gauges.ts 锚定值与 docs/README）；**速度胶囊** = 实时速度下方第二行"上轮均速 xx.x t/s"（数值按分档着色，无数据显示 `--`；两行各自居中，整组在胶囊内水平居中）；**桌宠** = 见下条。hover 监听挂在整块悬浮窗而非画布上，避免移到 🔄/⤢ 按钮时误判离开。
- **桌宠气泡显隐**：`visT` 可见度 0~1 平滑过渡（约 0.15s）。**待机（无生成任务）时整块气泡不显示**（含底板与尾巴，桌宠独享画面）；生成中/估算中/启动等待（`running`/`est`）显示单行实时速度；**鼠标悬停强制显示并展开全部行**——所以待机时移上去仍能查上一轮均速。气泡行 = **实时速度 +（≥2 任务时每进程一行：会话尾 6 位/进程 pid + 分档着色速度，非流式"待机"，最多 6 行，放不下按可用高度丢弃）+ 上轮均速**；多任务（≥2 进程）时同样强制显示。**常显上轮均速**（**桌宠右键菜单勾选项**与**完整面板顶栏同名开关**两处同一状态——顶栏开关仅悬浮窗样式选了桌宠时出现、悬浮窗模式自动隐藏，悬浮窗下走右键菜单勾选；存储键 `petLastAlways.v1`）：勾选后气泡恒展开、无需悬停——生成开始时随实时速度一起淡入即两行（实时速度 + 上轮均速）；**只影响展开行数，不改显隐：待机时气泡照常隐藏**（悬停仍可查上一轮均速）；取消勾选回到上述默认行为。
- **桌宠不缩小**：桌宠窗口为"底部正方形精灵区 + 顶部 56px 气泡预留带"（`main.rs` 的 `PET_BUBBLE_RESERVE`，滚轮缩放只改正方形边长、预留带恒定），气泡底边锚定在精灵头顶附近（约 9% 精灵高处）、随行数**向上生长**——精灵恒按正方形区（画布短边）缩放，行数变化不再压缩精灵。多任务时窗口额外向上加高 96px（`update_pet_task_extra` 3 拍防抖 → `apply_pet_size` 按高度差上移窗口，任务回落同样防抖收回）。展开进度 `hoverT` 做指数平滑（约 0.15s），气泡宽高/标签列一起过渡；各行动气泡高度内裁剪并淡入，不会先画到框外。浏览器预览（窗口非"边长+预留"比例）退回短边正方形排版并按可用高度丢任务行，预留不够时气泡顶到画布顶。
- 桌宠：精灵动画——**待机轮播**机制保留（`idleAnims` 整行播完随机换一个），但当前两包均只登记站立行 0——待机恒站立；鲸鱼行 9/10（说话/害羞）仅在 `anims` 登记备用、不参与轮播；**生成中/启动等待按实时速度六档换动作**（`speedTierIndex` 映射：0–40→行 7 running、40–80→行 8 review（一、二档互换：最慢档小跑、次低速迈步）、80–240→行 4 jumping（三、四档同为跳跃）、240+→行 1/2 running_right/left 左右来回跑、每跑完一遍换向）；**动画不打断**：任何变化（待机↔跑步切换、跑步组档位变化）都等当前动画整行播完才在行尾切换（帧数声明=素材实际非空帧数；鲸鱼女仆行 8 用 `play: [0,1,2,3,0]` 显式序列——第 5 帧(0 基列 4)为坏帧不播、第 6 帧(列 5)图形偏大弃用，末位以列 0 代替，循环收尾都停在首帧）；估算回退保持待机轮播；**滚轮上下缩放** 100~480 逻辑像素（`set_float_size` 持久化）；双击或 🔄 按钮换宠物（存储键 `petPack.v2`）。
- 悬浮窗/桌宠**右键菜单**：恢复窗体 / 退出程序（`quit_app` 命令）；桌宠样式下菜单首项为**"常显上轮均速"勾选项**（自绘小方框，仅 `body.float-mode.style-pet` 时显示；点击切换并即时生效，菜单不收起，点菜单外关闭；与完整面板顶栏同名开关同一状态）。勾选框排版须用双 ID 选择器压过 `#float-menu button` 的 `all: unset`（单 ID 特异性打不过"ID+元素"，flex 失效会让小方框退化成行内零尺寸——已踩坑），见 style.css。
- **位置独立记忆**：完整面板与悬浮窗各自记住位置（`~/.zcode/speed-panel-mode.txt` JSON 的 `full_pos`/`float_pos`/`pet_size`，物理像素）；收起时悬浮窗回自己上次位置（无记忆则锚定当前窗体中心），展开时窗体回自己老位置，均 `clamp_to_screen` 防止跑出屏幕。拖动期间位置落盘节流 2s，关闭/退出立即落盘。
- 整块可拖动（`startDragging`；按钮/下拉框不参与）；**仪表/胶囊双击即恢复完整面板**（按钮上的双击不触发；桌宠不参与——双击已用于换宠物，其恢复走 ⤢ / 右键菜单 / 托盘）。

## 窗口与托盘

- **无边框窗口**（`decorations:false`，透明窗口在 mac 走 `macos-private-api` + `macOSPrivateApi`）+ 自绘顶栏（应用图标 `app-icon.png`）：`#app-header` 带原生 `data-tauri-drag-region`，空白处拖动由内核处理，子元素经 enableDrag 冒泡拖动（二者互斥，按钮/输入/`.dropdown` 不参与）；双击顶栏安全最大化/还原。
- 顶栏控制平台原生化：
  - **macOS**：控制按钮移至顶栏最左侧，为原生交通灯圆点（左起依次为：红 `#ff5f56` 收起为悬浮窗、黄 `#ffbd2e` 最小化、绿 `#27c93f` **原生全屏**，2026-09-19 按 mac 语言惯例调整——原生全屏在所在屏进出、无副屏跳屏问题；"铺满当前屏"仍走双击顶栏的安全最大化），平时半透明纯色圆点，鼠标悬停控制区时显现微小符号（`✕`、`—`、`▢`）；
  - **Windows**：保持顶栏最右侧自绘 `— 最小化`、`▢ 最大化`、`✕ 收起为悬浮窗` 风格不变。
  - 完全退出走托盘菜单或悬浮窗右键菜单。
- **多屏安全最大化（`toggle_maximize_safe`）**：macOS 无边框窗口调用系统 `toggleMaximize()` 会触发系统 `zoom:` 回退到主屏跳屏。后端通过 `toggle_maximize_safe` 计算窗口中心点所在显示器（`monitor_from_point`）铺满（避让顶部菜单栏 28pt），并记忆还原物理矩形；再次调用或双击顶栏安全还原至原副屏位置和尺寸；折叠为悬浮窗时清理暂存。
- 托盘：左键单击显示/隐藏；右键菜单**顶部为实时状态行**（disabled 不可点，poller 每拍按快照更新：生成中 `x.x t/s` / 估算中 `≈x.x t/s` / 待机；文本变化才写入，托盘 tooltip 同步为 `ZCode 速度仪表盘 · 状态`），其后是菜单项（显示面板 / 隐藏到托盘 / 悬浮窗切换 / 退出）。重复启动唤起已有窗口（single-instance 插件）。
- 窗口标题实时同步当前速度（任务栏/Alt+Tab 可见）。
- **mac 差异**：
  - **Dock 两态（动态激活策略，2026-09-19）**：完整面板 = **Regular**（`apply_mode` 切换）——亮出 Dock 图标、进 Cmd+Tab，且只有 Regular 应用身份的窗口绿键才给**原生 Space 全屏**（Accessory 恒为辅助全屏：铺满但菜单栏不隐藏，实测定论）；收起悬浮窗/桌宠 = **Accessory**——Dock 图标自动隐藏、回菜单栏常驻，应用不退出。
  - 自定义应用菜单：`Cmd+Q` 被拦截为"隐藏为悬浮窗"（菜单中**不含任何系统退出项**，保证退出只走托盘与悬浮窗右键）；附"编辑" submenu（cut/copy/paste/select_all）保住 WebView 的 Cmd+C/V/X/A。
  - 退出兜底：`RunEvent::ExitRequested { code: None }` 一律 `prevent_exit` + 保存 + 折叠为悬浮窗（真退出 `app.exit(0)` 时 code=Some 放行，`RunEvent::Exit` 再保存一次）。**真退出只有托盘菜单"退出"与悬浮窗右键"退出程序"两条路**。
  - **引导提示**：前端右上角显示"应用常驻菜单栏 ↗ 点菜单栏图标可显示面板 / 退出"（深色半透明、顶部小箭头指向菜单栏），6 秒自动淡出、点击立即关闭；**仅完整面板模式显示**（悬浮窗/桌宠窗口过小会被裁剪，CSS 按 `body.float-mode` 门控）。触发时机两条：① 启动——setup 阶段早于 WKWebView 加载、emit 发即被弃，改为前端初始化完成后 `invoke("tray_hint_once")` 领取一次性标志（AppState 的 `tray_hint_pending`，mac 初始 true、领取即清零，非 mac 恒 false）；② 托盘"显示面板"/左键 toggle 唤起隐藏窗口（`show_main`，页面已就绪，直接 emit `tray-hint`）。Windows 两条路径都不触发，前端永不显示。

## 日志系统（main.rs DebugLog）

- `~/.zcode/speed-panel-debug.jsonl`：JSONL 追加写，8MB 轮转为 `.jsonl.1`；轮转旧文件超 7 天在启动时自动删除。
- 事件四类：`tick` / `call` / `cal`（格式与排查方法见 [key-rules.md](key-rules.md) #6）与 `cal_reset`（手动/漂移自动重校准：`reason`=manual/auto、系数前后 `bpt_old`/`bpt_new`，auto 另附触发时的轮均值 `round_avg` 与 5 轮基线 `base_avg`）。另有 `net` 事件（netio.rs，见"网速监控"节）：`ev`=ckpt_accepted / ckpt_upload_start / ckpt_upload_end，附工作区 `ws`、工件大小 `mb` 与是否回补 `backfill`。
- `tick` 另含**多任务排查三件套**：`infl`（进行中会话数）、`pids`（每台被跟踪进程的探测窗清洗速率，KB/s）、`attr`（进行中会话尾 4 位 → 归属 pid 列表）。多任务显示异常时先对账这三项：`pids` 里哪台在写、`attr` 是否把两会话挤到同一 pid、`infl` 与门控是否一致（2026-09-18 排查时只有 npids 单字段，无法回答"哪台进程在写"）。`tick` 另含**网络监控五件套**：`net_up`/`net_dn`（整机上传/下载速率，KB/s）、`cli_conn`/`app_conn`（会话组/桌面端组连接数）、`ckpt_up`（快照上传进行中）。

## 持久化文件

| 文件 | 内容 |
|---|---|
| `~/.zcode/speed-panel-mode.txt` | JSON：mode/style/full_pos/float_pos/pet_size（旧格式纯文本兼容） |
| `~/.zcode/speed-panel-cal.json` | 系数样本队列（updated_ms + samples，超 14 天过期回先验；见"实时速度"节） |
| `~/.zcode/speed-panel-net.json` | 网络当日累计（day/up/down/ckpt/ckpt_count + 当日已接受工件名单 uploads，跨天清零；见"网速监控"节） |
| `~/.zcode/speed-panel-guard.json` | 快照防护状态（lockedSinceMs/callsBaseline/blockedRounds/callsSeen 增量基准，未防护时不落多余键；见"快照防护"节） |
| `~/.zcode/speed-panel-ckpt-history.json` | 防护前的原上传记录留档（apply 清空前写入，同工作区新行覆盖，上限 500 行；防护期间列表/报告回看） |
| `~/.zcode/speed-panel-debug.jsonl` | 调试日志（8MB 轮转 + 7 天清理） |

自动启动状态**不落本地文件**——Windows 注册表 `HKCU\…\Run` / macOS LaunchAgent plist 即事实源（见「自动启动」节）。

## 应用内更新（updater.rs）

- **版本号**：来源 `tauri.conf.json` 的 `version`（与 `Cargo.toml`/`package.json` 三处同值，发版一起 bump），运行时读 `app.package_info().version`。完整面板状态栏最右显示（如 `v0.2.1`；悬浮窗/桌宠形态与浏览器 mock 模式下隐藏），**点击即手动检查更新**——检查中前缀 ↻ 旋转，结果以右下角轻提示反馈（"已是最新版本 vX.Y.Z" / "检查更新失败：网络异常"）。
- **检查时机**：① 启动后 8s（避开启动期 SQLite/IO 高峰）后台静默查一次；② 常驻期间每小时醒一次，距上次成功检查 ≥24h 才发请求（每天一次）；③ 手动点击版本号。数据源：GitHub API `repos/Masterchiefm/zcode-speed-panel/releases/latest`（必须带 User-Agent；latest 端点天然排除 draft/prerelease；403 限流同网络失败处理）。
- **静默原则**：自动路径上任何失败（断网、限流、解析失败、无本平台安装包、tag 解析不了）一律视为"无更新"，不弹窗不出提示——宁可漏报，不打扰。唯一如实反馈的场景：用户点了"立即更新"后下载/启动安装失败（静默会让按钮看起来失灵）。
- **版本比较**：`parse_version` 取 tag 的 `v?主.次.修` 三段数值（`-rc.1`/`+build` 后缀忽略，本项目不发预发布），严格大于当前版本才算更新；任一侧解析失败不算（防怪 tag 诱导更新）。
- **安装包匹配（CI 命名耦合，key-rules #12）**：win-x64 → `*_x64-setup.exe`（NSIS 安装版；免安装 portable 版不参与自动安装）；mac-x64 → `*_x64.dmg`；mac-aarch64 → `*_aarch64.dmg`。改 build.yml 产物名/加架构须同一提交同步 `updater::pick_asset` 及其测试。
- **流程**：检查到新版本 → emit `update`(available) 弹右下角更新卡片（新旧版本对比 + Release 说明摘要 + "更新内容 ↗"链接经 `open_url` 命令走系统浏览器打开，仅放行 https + **"手动下载 ↗"按钮**——应用内安装之外的自助路径，打开本版本 Release 页自行下载，无版本 URL 时回退 releases 列表页）→ 立即**后台静默预下载**到系统临时目录 `zcode-speed-panel-update/<资产名>`（进度按 250ms 节流 emit downloading 事件）→ 就绪 emit ready，卡片按钮变"立即安装"。点"立即更新"时若尚未下载则标记待装，就绪后自动安装，无需二次点击。
- **安装**：Windows 运行 NSIS 安装包、600ms 后保存状态并退出应用（安装器接管，等一下再退避免撞上进程锁）；macOS `open` 打开 dmg 由用户拖入 Applications（应用不退出，旧版本跑到用户重启）。安装是**用户点击触发**而非全自动无人值守——安装器窗口未经同意弹出与"不打扰"原则冲突，且未签名包静默替换有风险；若将来要完全自动，Windows 侧启动安装包时加 `/S` 参数（NSIS 静默安装）即可，下载/就绪链路不用动。
- **免打扰细节**：卡片 ✕ 关闭后该版本不再自动弹（前端 localStorage 记忆，版本号旁留青色小圆点提醒；手动检查会重新打开卡片）；悬浮窗模式下卡片/提示不显示（回到完整面板仍可见）；用户没在等安装时预下载失败完全静默（点"立即更新"会重新拉起下载）。

## CI 与发布

- `.github/workflows/build.yml`：**不随普通推送自动触发**；`v*` 标签 → 自动创建 GitHub Release，Assets 附 Windows 安装版（`_x64-setup.exe`）、免安装版（`_x64-portable.exe`，主程序 exe 直接改名）与 macOS 双架构 dmg（`_x64.dmg` = Intel 10.15+、`_aarch64.dmg` = Apple Silicon 11+，独立包不做 universal，未签名公证见 README 绕过指引）；`workflow_dispatch` → Actions 页手动触发，产物在本次运行的 Artifacts（`windows` 含两个 exe；`macos-x86_64-apple-darwin` / `macos-aarch64-apple-darwin` 各含一个 dmg）。
- **发版流程（应用内更新依赖，key-rules #12）**：版本号三处同步 bump——`src-tauri/tauri.conf.json`（权威：运行时读取用于显示与比较）、`src-tauri/Cargo.toml`、`package.json`——再打 `v*` 标签。更新检查按 Release 资产名后缀匹配安装包（见"应用内更新"节），build.yml 的产物命名不可随意改。
- **build-macos job**：`runs-on: macos-15`，matrix 双目标（`x86_64-apple-darwin` + `MACOSX_DEPLOYMENT_TARGET=10.15`、`aarch64-apple-darwin` + `11.0`），tauri-action `--bundles dmg`；最低系统版本双重注入——环境变量决定二进制 `LC_BUILD_VERSION`，`--config '{"bundle":{"macOS":{"minimumSystemVersion":"…"}}}'` 决定 Info.plist 的 `LSMinimumSystemVersion`（plist 只认 config，缺省恒 10.13，不读环境变量）。产物名 `zcode-speed-panel_版本_x64.dmg` / `_aarch64.dmg` 由 tauri 默认命名，恰好满足双独立包要求。
- 本地正式版：`npm run tauri build` → Windows `src-tauri/target/release/bundle/nsis/*.exe`；macOS `bundle/dmg/*.dmg`（交叉构建 aarch64 见 README 开发章节）。

## 已知边界情况

- 管道静默调用（实测约半数）实时读数走 ≈ 回退，属预期行为而非 bug（此类调用启动期先显示 20s "…" 提示再切换）。
- 多任务并发（多窗口/子代理并行）时实时读数为**进行中会话归属进程的聚合总吞吐**（任一会话尚无归属记录时触发全进程求和兜底）；完整面板同时显示分任务明细（见"实时速度"节）。同进程内并行的多个子代理在字节层不可拆分，显示为该进程合计。
- 冷启动后系数需 1~2 个达标调用收敛，此前读数可能有偏差。
- 网络监控的口径边界（详见"网速监控"节）：**会话流量是 token×系数的量级估算**（± 数倍，带 ≈ 标注）；**快照工件是面板观测期的真实下界**（面板未运行期间的接受仅当日首次启动回补一次）；**整机数字混合其他应用流量**，走本地代理时还含隧道加密开销；连接归属仅 Windows。checkpoints 目录被 ACL 封锁时显示"不可读"，监控不到新上传（封锁本身就是用户侧防护）。
- 硬崩溃（CLI 进程被杀、assistant 行无人补写 `completed`）最多残留 10 分钟门控（兜底上限）；已归属会话的进程退出会被进程守卫立即判停，未归属的新会话只能等兜底。
- mac 的 CleanParams（burst 禁用/无静态底噪/系数先验 700/延迟落盘宽限 15s/离群拒绝 3 倍）中，先验与宽限已按 2026-09-17 的 6 条 cal 事件真值对账修正（归因正确时 pred/true 完全一致 62.1=62.1），尚未做 Windows 侧同等长度的对账回归；读数异常时先跑 `python scripts/live_vs_true.py` 对账、看 cal 事件 `attr_pid`/`top_pid` 归因再调参（key-rules #10）。
- 应用内更新能力**随版本生效**：只有装了含 updater.rs 版本的用户才会收到后续更新提示，存量旧版本需手动升级一次铺底；dev 实例（`npm run tauri dev`）同样做真实检查与下载——版本等于最新 Release tag 时显示"已是最新"，属预期（热重启每次都会触发一次启动检查，量级远低于 API 限流）。
- 自动启动的 macOS 分支**未实机验证**（只过 `cargo check`）：LaunchAgent 直接跑 `.app` 内二进制而非 `open -a`，应用身份/Dock 两态需实机确认；follow 模式下面板手动退出后不会自动复活，需重新登录或手动打开（见"自动启动"节）。
