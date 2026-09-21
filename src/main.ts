import "./style.css";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ArcGauge, BadgeGauge, MiniGauge, SPEED_TIERS, drawSpark, fmtBps, fmtBytes, fmtClock, fmtDayClock, fmtTokens, fmtTps, speedColor } from "./gauges";
import { PetWidget } from "./pet";
import { startMock, type CkptStat, type ConnStat, type Snapshot } from "./mock";
import { initModelStats } from "./model_stats";
import { initGuard, renderGuard, type GuardStatus } from "./guard";

interface SnapshotPayload {
  snapshot: Snapshot;
  rolloutDir: string;
  mode: string;
  floatStyle: string;
  /** 快照防护状态（后端 snapshot_guard.rs；mock 模式无此字段 → 卡片隐藏） */
  guard?: GuardStatus;
}

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

const hasTauri = typeof (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ !== "undefined";

const isMac = navigator.userAgent.includes("Mac");
if (isMac) {
  document.body.classList.add("platform-mac");
}

async function tauriInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T | undefined> {
  if (!hasTauri) return undefined;
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

const gCurrent = new ArcGauge($("g-current"), {
  label: "当前输出速度",
  unit: "token / s",
  color: "#22d3ee",
  color2: "#0ea5e9",
  kind: "speed",
  minScale: 60, // 最小量程 60 t/s，常见速度落在弧形中段更好读
  tiers: SPEED_TIERS, // 六档（0–40/40–80/80–160/160–240/240–320/320+），随当前速度换色
});

const gAvg = new ArcGauge($("g-avg"), {
  label: "今日平均速度",
  unit: "token / s",
  color: "#a78bfa",
  color2: "#8b5cf6",
  kind: "speed",
});

const gTotal = new ArcGauge($("g-total"), {
  label: "今日总 Token",
  unit: "今日累计",
  color: "#34d399",
  color2: "#10b981",
  kind: "tokens",
});

// 当前速度卡右上角小表：最近一轮已完成调用的速度（落盘口径，非实时）
const gLast = new BadgeGauge($("g-last"), { tiers: SPEED_TIERS });
// 当前速度卡右下角小表：历史最高单调用速度（准入口径见 tooltip 与 metrics.rs）
const gPeak = new BadgeGauge($("g-peak"), { tiers: SPEED_TIERS, label: "最高" });
// 今日平均卡右上角小表：历史平均速度（全部调用 Σeff ÷ Σgen，与今日平均同口径）
const gHistAvg = new BadgeGauge($("g-histavg"), { tiers: SPEED_TIERS, label: "历史" });

const miniGauge = new MiniGauge($("mini-gauge"), { tiers: SPEED_TIERS });
// 仪表悬浮窗右上角的上轮小环（与完整面板角标同款，只是尺寸更小）
const miniLast = new BadgeGauge($("mini-last"), { tiers: SPEED_TIERS });
// 存储键升级到 v2：让老用户也拿到一次新默认（鲸鱼女仆），之后的选择照常记住
const PET_PACK_KEY = "petPack.v2";
let currentPetPack = localStorage.getItem(PET_PACK_KEY) ?? "maid-deepseek-whale";
const petWidget = new PetWidget($<HTMLCanvasElement>("pet-canvas"), currentPetPack, () => {
  currentPetPack = petWidget.packId;
  localStorage.setItem(PET_PACK_KEY, currentPetPack);
});
petWidget.start();

// ---- 桌宠滚轮缩放：上下滚动调整悬浮窗大小（后端记忆，重启后保持） ----
const PET_BASE_SIZE = 200;
const PET_SIZE_MIN = 100;
const PET_SIZE_MAX = 480;
let petSize = Math.min(PET_SIZE_MAX, Math.max(PET_SIZE_MIN, Number(localStorage.getItem("petSize.v1")) || PET_BASE_SIZE));
$("float-pet").addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    const next = petSize * (e.deltaY < 0 ? 1.08 : 1 / 1.08);
    const clamped = Math.min(PET_SIZE_MAX, Math.max(PET_SIZE_MIN, next));
    if (Math.round(clamped) === Math.round(petSize)) return;
    petSize = clamped;
    localStorage.setItem("petSize.v1", String(Math.round(clamped)));
    if (hasTauri) {
      tauriInvoke("set_float_size", { size: Math.round(clamped) }).catch(() => {});
    }
  },
  { passive: false }
);

// ---- 悬浮窗/桌宠右键菜单：恢复窗体 / 退出 ----
const floatMenu = $("float-menu");
const showFloatMenu = (x: number, y: number) => {
  floatMenu.style.display = "flex";
  const mw = floatMenu.offsetWidth || 110;
  const mh = floatMenu.offsetHeight || 60;
  floatMenu.style.left = `${Math.max(0, Math.min(x, window.innerWidth - mw - 2))}px`;
  floatMenu.style.top = `${Math.max(0, Math.min(y, window.innerHeight - mh - 2))}px`;
};
const hideFloatMenu = () => {
  floatMenu.style.display = "none";
};
for (const id of ["float-pet", "float-gauge", "float-pill"]) {
  $(id).addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showFloatMenu(e.clientX, e.clientY);
  });
}
window.addEventListener("mousedown", (e) => {
  if (!floatMenu.contains(e.target as Node)) hideFloatMenu();
  if (!styleDropdown.contains(e.target as Node)) setStyleDropdownOpen(false);
});
window.addEventListener("blur", () => {
  hideFloatMenu();
  setStyleDropdownOpen(false);
});
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") setStyleDropdownOpen(false);
});
$("float-menu-restore").addEventListener("click", () => {
  hideFloatMenu();
  requestMode("full");
});
$("float-menu-quit").addEventListener("click", () => {
  hideFloatMenu();
  tauriInvoke("quit_app");
});

// ---- 桌宠"常显上轮均速"：桌宠右键菜单勾选项 + 完整面板顶栏开关（同一状态）----
// 勾选后气泡恒两行（生成中随实时速度一起展开显示，无需悬停）；显隐仍随生成状态，
// 待机不显示。菜单点击后不收起，让勾选状态可见，点菜单外任意处照常关闭。
// 顶栏开关仅桌宠样式时由 CSS 显示
const PET_LAST_KEY = "petLastAlways.v1";
let petLastAlways = localStorage.getItem(PET_LAST_KEY) === "1";
const petLastControls = [$("float-menu-pet-last"), $("pet-last-toggle")];
const applyPetLast = () => {
  for (const el of petLastControls) el.classList.toggle("pet-last-on", petLastAlways);
  petWidget.setAlwaysLast(petLastAlways);
};
applyPetLast();
for (const el of petLastControls) {
  el.addEventListener("click", () => {
    petLastAlways = !petLastAlways;
    localStorage.setItem(PET_LAST_KEY, petLastAlways ? "1" : "0");
    applyPetLast();
  });
}

const sparkCanvas = $<HTMLCanvasElement>("spark");
const liveDot = $("live-dot");
const liveText = $("live-text");
const updatedAt = $("updated-at");
const subCurrent = $("sub-current");
const subAvg = $("sub-avg");
const subTotal = $("sub-total");
const stDir = $("st-dir");
const stCalls = $("st-calls");
const stSessions = $("st-sessions");
const stLast = $("st-last");
const chartMax = $("chart-max");
const taskCard = $("task-card");
const taskList = $("task-list");
const netCard = $("net-card");
const netScope = $("net-scope");
const netUpBpsEl = $("net-up-bps");
const netDownBpsEl = $("net-down-bps");
const netConnsEl = $("net-conns");
const netConnCli = $("net-conn-cli");
const netConnApp = $("net-conn-app");
const netCliConnsEl = $("net-cli-conns");
const netAppConnsEl = $("net-app-conns");
const netSessUpEl = $("net-sess-up");
const netSessDownEl = $("net-sess-down");
const netUpTodayEl = $("net-up-today");
const netDownTodayEl = $("net-down-today");
// 快照防护与上传记录卡（网络监控卡下方）：防护控制区 + 今日快照上传 + 记录列表
const guardCard = $("guard-card");
const guardCtl = $("guard-ctl");
const guardNote = $("guard-note");
const ckptInfo = $("ckpt-info");
const ckptText = $("ckpt-text");
const ckptNames = $("ckpt-names");
const ckptListHead = $("ckpt-list-head");
const ckptList = $("ckpt-list");
const floatTps = $("float-tps");
const floatDot = $("float-dot");
const floatLast = $("float-last");

let lastSpark: number[] = [];
let lastNowMs = 0;
let sparkColor = "#22d3ee";
// ---- 曲线时间范围（15m/1h/6h/24h，默认 15 分钟，localStorage 记住选择）----
// 15 分钟档走 metrics payload 的今日 spark（后端把实时速度混入尾桶，零额外
// 查询）；更长档位走 chart_stats 命令（usage 库现算 90 桶，5s 拉取一次），
// 前端把实时速度混入最新桶——两档尾桶口径一致，切换无跳变
type ChartRange = 15 | 60 | 360 | 1440;
const CHART_RANGES: { value: ChartRange; label: string; bucketLabel: string; gridMs: number }[] = [
  { value: 15, label: "15 分钟", bucketLabel: "10 秒一档", gridMs: 5 * 60_000 },
  { value: 60, label: "1 小时", bucketLabel: "40 秒一档", gridMs: 10 * 60_000 },
  { value: 360, label: "6 小时", bucketLabel: "4 分钟一档", gridMs: 60 * 60_000 },
  { value: 1440, label: "24 小时", bucketLabel: "16 分钟一档", gridMs: 4 * 3_600_000 },
];
const CHART_RANGE_KEY = "chartRange.v1";
const storedChartRange = Number(localStorage.getItem(CHART_RANGE_KEY));
let chartRange: ChartRange = CHART_RANGES.some((r) => r.value === storedChartRange)
  ? (storedChartRange as ChartRange)
  : 15;
let chartCache: { buckets: number[]; bucketMs: number; nowMs: number } | null = null;
let chartTimer = 0;
// 最新一拍的实时状态（长档位尾桶混入用）
let liveTpsNow = 0;
let liveActive = false;

function chartRangeCfg(): (typeof CHART_RANGES)[number] {
  return CHART_RANGES.find((r) => r.value === chartRange) ?? CHART_RANGES[0];
}

/** 长档位数据拉取：失败静默保留旧缓存（浏览器预览无 Tauri 同样静默） */
async function refreshChartStats() {
  const p = await tauriInvoke<{
    windowMin: number;
    bucketMs: number;
    nowMs: number;
    buckets: number[];
  }>("chart_stats", { windowMin: chartRange });
  if (p && p.windowMin === chartRange) {
    chartCache = { buckets: p.buckets, bucketMs: p.bucketMs, nowMs: p.nowMs };
    redrawSpark();
  }
}

function redrawSpark() {
  if (chartRange === 15) {
    if (lastSpark.length) drawSpark(sparkCanvas, lastSpark, sparkColor, lastNowMs);
    return;
  }
  if (chartCache && chartCache.buckets.length >= 2) {
    const values = chartCache.buckets.slice();
    // 实时尾桶混入（与 15 分钟档后端行为一致）：生成/估算中把最新桶临时填成
    // 当前速度，下一轮 5s 拉取被真实落盘数据替换
    if (liveActive && liveTpsNow > 0) values[values.length - 1] = liveTpsNow;
    drawSpark(sparkCanvas, values, sparkColor, lastNowMs, {
      bucketMs: chartCache.bucketMs,
      gridMs: chartRangeCfg().gridMs,
    });
    return;
  }
  // 浏览器预览（无 Tauri）：长档位暂无数据源，沿用 15 分钟 mock 数据画样式
  if (!hasTauri && lastSpark.length) {
    drawSpark(sparkCanvas, lastSpark, sparkColor, lastNowMs, { gridMs: chartRangeCfg().gridMs });
  }
}

// 任务卡隐藏迟滞：任务数在 1↔2 边界抖动（子代理起止、流式阈值边缘）时，
// 连续 3 拍（~2s）不足 2 行才隐藏，避免下方曲线卡整块上下跳
let taskHideStreak = 3;

function statusClass(s: Snapshot): string {
  if (s.isLive || s.isStarting) return "dot live";
  if (s.isEstimating) return "dot est";
  return "dot idle";
}

/** 网速监控卡：整机实测速度 + 会话上传估算拆分。
 *  整机 = 接口计数器真实值；会话 = token×系数估算（带 ≈）。
 *  快照相关的一切（防护 / 今日快照上传 / 上传记录）在快照防护卡
 *  （renderSnapshot），本卡不含快照内容。接口不可用（stub 平台）时整卡隐藏 */
function renderNet(s: Snapshot) {
  if (!s.netAvailable) {
    netCard.hidden = true;
    return;
  }
  netCard.hidden = false;
  netUpBpsEl.textContent = fmtBps(s.netUpBps);
  netDownBpsEl.textContent = fmtBps(s.netDownBps);
  netSessUpEl.textContent = fmtBytes(s.netSessUpToday);
  netSessDownEl.textContent = fmtBytes(s.netSessDownToday);
  netUpTodayEl.textContent = fmtBytes(s.netUpToday);
  netDownTodayEl.textContent = fmtBytes(s.netDownToday);

  // 连接归属（仅 Windows）：每条连接的远端 + 归属进程（类型 + pid）挂 tooltip。
  // 两组都是 ZCode 自身进程：会话 = CLI（对话 API 流量），
  // 桌面端 = Electron 壳（快照上传/遥测等非对话流量），不含其他应用
  netConnsEl.style.display = s.netConnsAvailable ? "" : "none";
  if (s.netConnsAvailable) {
    netCliConnsEl.textContent = String(s.netCliConns);
    netAppConnsEl.textContent = String(s.netAppConns);
    const connLines = (list: ConnStat[]) => list.map((r) => `${r.remote} · ${r.proc || "?"}(${r.pid})`);
    netConnCli.title = s.netCliConnList.length
      ? `ZCode 会话进程（CLI，对话 API 流量）的连接：\n${connLines(s.netCliConnList).join("\n")}`
      : "ZCode 会话进程当前无外连";
    netConnApp.title = s.netAppConnList.length
      ? `ZCode 桌面端进程（Electron 主/渲染/GPU/工具——快照上传、遥测等非对话流量）的连接：\n${connLines(s.netAppConnList).join("\n")}`
      : "ZCode 桌面端进程当前无外连";
  }
  netScope.textContent = s.netConnsAvailable ? "整机 = 本机全部应用流量（非仅 ZCode）" : "整机 = 本机全部应用流量";
}

/** 快照防护与上传记录卡：今日快照上传状态行 + 工作区名单 + 上传记录列表。
 *  防护控制区（徽标/按钮/统计）由 guard.ts 的 renderGuard 渲染——后端无
 *  guard 字段（浏览器预览/mock）时整块隐藏，本卡只看记录。
 *  有防护状态或任何快照数据可显示时亮卡（与网络卡是否可用相互独立） */
function renderSnapshot(s: Snapshot) {
  const guardOk = !!s.guard;
  const ckptOk =
    s.netCkptStatus === "ok" || s.netCkptStatus === "blocked" || (s.netCkptList?.length ?? 0) > 0;
  guardCard.hidden = !(guardOk || ckptOk);
  guardCtl.hidden = !guardOk;
  guardNote.hidden = !guardOk;

  // 今日快照上传状态行：上传中（脉冲）> blocked（ACL 封锁）> 今日有量 > 隐藏。
  // 防护开启时目录已清空，不可能有"上传中"；今日量是防护开启前的真实历史，
  // 标注"防护前"（防护生效的宣告在上方 guard-stats，这里不重复 🔒）。
  // 今日有量时状态行下方逐行列出工作区名单（不去重折叠，上游 v0.4.1 交互），
  // 悬停看逐条明细——防护中同样列名单（都是防护开启前发生的真实上传）
  const renderTodayNames = (todayList: CkptStat[]) => {
    ckptNames.textContent = "";
    ckptNames.hidden = todayList.length === 0;
    // 按工作区聚合今日快照（件数 >1 时附件数与字节小计），最新越近排越前；
    // 回补扫描顺序不保证按时间，取组内最大 recordedMs 当"最新"
    const byWs = new Map<string, CkptStat[]>();
    for (const r of todayList) {
      const k = r.workspace || "?";
      const arr = byWs.get(k);
      if (arr) arr.push(r);
      else byWs.set(k, [r]);
    }
    const lastMs = (rows: CkptStat[]) => Math.max(...rows.map((r) => r.recordedMs));
    const groups = [...byWs.entries()].sort((a, b) => lastMs(b[1]) - lastMs(a[1]));
    for (const [ws, rows] of groups) {
      const line = document.createElement("div");
      line.className = "ckpt-name";
      const bytes = rows.reduce((t, r) => t + r.bytes, 0);
      line.textContent = `${ws} · ${rows.length > 1 ? `${rows.length} 个 · ` : ""}${fmtBytes(bytes)}`;
      line.title = rows
        .map((r) => `${fmtDayClock(r.recordedMs)} · ${fmtBytes(r.bytes)}`)
        .join("\n");
      ckptNames.append(line);
    }
  };
  if (s.netCkptUploading) {
    ckptInfo.hidden = false;
    ckptInfo.classList.add("uploading");
    ckptText.textContent = "⬆ 快照上传进行中——工作区内容正整包加密上传";
    ckptNames.hidden = true;
  } else if (s.netCkptStatus === "blocked") {
    ckptInfo.hidden = false;
    ckptInfo.classList.remove("uploading");
    ckptText.textContent = "checkpoints 目录不可读（可能已被 ACL 封锁，监控不到新上传）";
    ckptNames.hidden = true;
  } else if (s.netCkptStatus === "missing" || s.netCkptToday === 0) {
    ckptInfo.hidden = true;
    ckptInfo.classList.remove("uploading");
    ckptNames.hidden = true;
  } else {
    ckptInfo.hidden = false;
    ckptInfo.classList.remove("uploading");
    ckptText.textContent = s.guard?.locked
      ? `今日快照上传 ${fmtBytes(s.netCkptToday)}（${s.netCkptTodayCount} 个）· 均为防护开启前的记录`
      : `今日快照上传 ${fmtBytes(s.netCkptToday)}（${s.netCkptTodayCount} 个）`;
    const todayList: CkptStat[] = s.netCkptTodayList ?? [];
    renderTodayNames(todayList);
    ckptInfo.title = todayList.length
      ? `今日已成功上传的加密快照（${todayList.length} 个）：\n${todayList
          .map((r) => `${fmtDayClock(r.recordedMs)} · ${r.workspace || "?"} · ${fmtBytes(r.bytes)}`)
          .join("\n")}`
      : "";
  }

  // 快照上传记录：每工作区最近一次快照（时间 / 工作区 / 加密后大小 / 状态），
  // 上传中 > 待传 > 已接受排序（后端排好）。固定显示 5 行，其余列表内滚动
  // 看完；文字可选中复制，另有 复制/导出 按钮（见 ckpt-tools）。
  // 行尾 📂 = 在系统文件管理器中打开该快照目录（mac Finder / Win 资源管理器，
  // 跨平台；只有磁盘上真实存在的行才有——留档历史行没有）。
  // 列表为空时不能静默消失（防护清空目录后曾变大片空白）：
  // 防护中给锁横幅（保留/删除两态）+ 留档历史；平时给"暂无记录"占位
  const ckptRows: CkptStat[] = s.netCkptList ?? [];
  ckptList.textContent = "";
  ckptListHead.style.display = "";
  const appendRow = (r: CkptStat, cls: string, stText: string) => {
    const row = document.createElement("div");
    row.className = cls;
    const time = document.createElement("span");
    time.className = "ckpt-time";
    time.textContent = fmtDayClock(r.recordedMs);
    const ws = document.createElement("span");
    ws.className = "ckpt-ws";
    ws.textContent = r.workspace || "?";
    const size = document.createElement("span");
    size.className = "ckpt-size";
    size.textContent = fmtBytes(r.bytes);
    const st = document.createElement("span");
    st.className = "ckpt-st";
    st.textContent = stText;
    row.append(time, ws, size, st);
    if (r.hash) {
      const open = document.createElement("button");
      open.className = "ckpt-open";
      open.type = "button";
      open.textContent = "📂";
      open.title = "在文件管理器中打开该工作区的快照目录（~/.zcode/v2/checkpoints）";
      open.addEventListener("click", () => {
        tauriInvoke("open_checkpoint_dir", { hash: r.hash }).catch((err: unknown) => {
          open.textContent = "⚠️";
          open.title = `打开失败：${err}`;
          window.setTimeout(() => {
            open.textContent = "📂";
          }, 2500);
        });
      });
      row.append(open);
    }
    ckptList.append(row);
  };
  if (ckptRows.length > 0) {
    for (const r of ckptRows) {
      appendRow(r, r.uploading ? "ckpt-row uploading" : r.accepted ? "ckpt-row" : "ckpt-row pending",
        r.uploading ? "上传中 ⬆" : r.accepted ? "已接受 ✓" : "待传");
    }
    if (s.guard?.locked) {
      // 保留模式：快照还在（递归锁，只读可扫），行照常显示且可点开——
      // 横幅说明状态即可，不挡内容
      const banner = document.createElement("div");
      banner.className = "ckpt-empty locked";
      banner.textContent = "🔒 以下快照已锁定保留（只读）· ZCode 无法写入新快照";
      ckptList.prepend(banner);
    }
  } else if (s.guard?.locked) {
    const banner = document.createElement("div");
    banner.className = "ckpt-empty locked";
    banner.textContent = "🔒 快照目录已清空并锁定 · 以下为防护前的原上传记录";
    ckptList.append(banner);
    // 防护前留档（apply 清空前保存）；旧版本未留档时退回今日已上传名单
    const history: CkptStat[] = s.guard.history?.length ? s.guard.history : s.netCkptTodayList ?? [];
    for (const r of history) {
      appendRow(r, "ckpt-row history", r.uploading ? "待传" : "已上传 ✓");
    }
  } else {
    const empty = document.createElement("div");
    empty.className = "ckpt-empty";
    empty.textContent = "暂无快照记录（ZCode 未生成过工作区快照）";
    ckptList.append(empty);
  }
  lastCkptReport = buildCkptReport(s);
}

/** 快照上传记录的纯文本报告（复制/导出共用）：表头 + 逐行 + 当日汇总 +
 *  ZCode 连接实况——取证时可整体留存 */
let lastCkptReport = "";
function buildCkptReport(s: Snapshot): string {
  const lines: string[] = [];
  const now = new Date();
  const p = (x: number) => x.toString().padStart(2, "0");
  lines.push(`ZCode 快照上传记录 · 导出于 ${now.getFullYear()}-${p(now.getMonth() + 1)}-${p(now.getDate())} ${p(now.getHours())}:${p(now.getMinutes())}`);
  lines.push("口径：每工作区最近一次快照（~/.zcode/v2/checkpoints/*/state.json）；大小为加密压缩后字节；状态 = 上传中/待传/已接受");
  lines.push("");
  lines.push("时间          工作区                       大小         状态");
  lines.push("------------  ---------------------------  -----------  --------");
  for (const r of s.netCkptList ?? []) {
    const st = r.uploading ? "上传中" : r.accepted ? "已接受" : "待传";
    lines.push(
      `${fmtDayClock(r.recordedMs).padEnd(12)}  ${(r.workspace || "?").padEnd(27).slice(0, 27)}  ${fmtBytes(r.bytes).padEnd(11)}  ${st}`,
    );
  }
  // 防护前留档（apply 清空前保存）——防护中磁盘扫描为空，取证报告仍要
  // 能看到完整的原上传记录
  if (s.guard?.locked && s.guard.history?.length) {
    lines.push("");
    lines.push(`防护前原上传记录（清空快照前留档，共 ${s.guard.history.length} 条）：`);
    for (const r of s.guard.history) {
      lines.push(
        `${fmtDayClock(r.recordedMs).padEnd(12)}  ${(r.workspace || "?").padEnd(27).slice(0, 27)}  ${fmtBytes(r.bytes).padEnd(11)}  防护前`,
      );
    }
  }
  lines.push("");
  lines.push(`今日快照上传：${fmtBytes(s.netCkptToday)}（${s.netCkptTodayCount} 个）`);
  for (const r of s.netCkptTodayList ?? []) {
    lines.push(`  ${fmtDayClock(r.recordedMs)}  ${(r.workspace || "?").padEnd(27).slice(0, 27)}  ${fmtBytes(r.bytes)}`);
  }
  lines.push(`今日整机上传：${fmtBytes(s.netUpToday)} / 下载：${fmtBytes(s.netDownToday)}（全部应用）`);
  lines.push(`会话流量估算：上传 ≈${fmtBytes(s.netSessUpToday)} / 下载 ≈${fmtBytes(s.netSessDownToday)}`);
  if (s.netConnsAvailable) {
    const conn = (list: ConnStat[]) =>
      list.map((r) => `  ${r.remote} · ${r.proc || "?"}(${r.pid})`).join("\n") || "  （无）";
    lines.push("");
    lines.push("ZCode 会话进程（CLI）连接：");
    lines.push(conn(s.netCliConnList ?? []));
    lines.push("ZCode 桌面端进程（Electron 壳，非会话流量）连接：");
    lines.push(conn(s.netAppConnList ?? []));
  }
  return lines.join("\n");
}

/** 复制到剪贴板：优先 navigator.clipboard，WebView 拒绝时退回
 *  execCommand（临时 textarea；body 是 user-select:none，需临时可选） */
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      ta.style.userSelect = "text";
      document.body.append(ta);
      ta.select();
      const ok = document.execCommand("copy");
      ta.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

// 复制 / 导出按钮（快照防护与上传记录卡）：按钮闪 ✓ 反馈，导出路径走右下角轻提示
const ckptCopyBtn = $<HTMLButtonElement>("ckpt-copy");
const ckptExportBtn = $<HTMLButtonElement>("ckpt-export");
let netToolTimer = 0;
const flashNetBtn = (btn: HTMLButtonElement, okText: string) => {
  const orig = btn.textContent;
  btn.textContent = okText;
  window.clearTimeout(netToolTimer);
  netToolTimer = window.setTimeout(() => {
    btn.textContent = orig;
  }, 1500);
};
ckptCopyBtn.addEventListener("click", async () => {
  if (!lastCkptReport) return;
  const ok = await copyText(lastCkptReport);
  flashNetBtn(ckptCopyBtn, ok ? "已复制 ✓" : "失败");
});
ckptExportBtn.addEventListener("click", () => {
  if (!lastCkptReport) return;
  const now = new Date();
  const p = (x: number) => x.toString().padStart(2, "0");
  const name = `zcode快照上传记录-${now.getFullYear()}${p(now.getMonth() + 1)}${p(now.getDate())}-${p(now.getHours())}${p(now.getMinutes())}.txt`;
  if (!hasTauri) {
    // 浏览器预览模式：无后端命令，退化为浏览器下载
    const blob = new Blob([lastCkptReport], { type: "text/plain;charset=utf-8" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = name;
    a.click();
    URL.revokeObjectURL(a.href);
    flashNetBtn(ckptExportBtn, "已下载 ✓");
    return;
  }
  tauriInvoke<string>("export_text_file", { fileName: name, text: lastCkptReport })
    .then((path) => {
      if (path) {
        flashNetBtn(ckptExportBtn, "已导出 ✓");
        toast(`已导出到 ${path}`);
      }
    })
    .catch((err) => {
      flashNetBtn(ckptExportBtn, "失败");
      toast(`导出失败：${err}`);
      console.warn("导出失败:", err);
    });
});

/** 缓存命中率 = cache_read ÷ input（usage 库的 input 本身就是全部提示 token，
 *  缓存命中的部分已含其中，分母再加 cache_read 会重复计数；cache_creation 全库
 *  恒为 0，防御性保留在分母以兼容将来单列它的 provider） */
const cacheHitRate = (s: Snapshot): string => {
  const prompt = s.inputTokens + s.cacheCreationTokens;
  if (prompt <= 0) return "0%";
  return ((s.cacheReadTokens / prompt) * 100).toFixed(1) + "%";
};

function onSnapshot(s: Snapshot) {
  gCurrent.setTarget(s.currentTps, s.isEstimating, s.isStarting);
  gAvg.setTarget(s.avgTps);
  gLast.setTarget(s.lastCallTps);
  gPeak.setTarget(s.histMaxTps);
  gHistAvg.setTarget(s.histAvgTps);
  gTotal.setTarget(s.totalTokens);
  miniGauge.setTarget(s.currentTps, s.isEstimating, s.isStarting);
  miniLast.setTarget(s.lastCallTps);
  renderNet(s);
  renderSnapshot(s);

  // 并发任务明细：≥2 个任务时显示（单任务时隐藏，不占版面）。
  // 一个 CLI 进程 = 一行，行值合计 = 当前速度表（文件增长按字节占比分摊）；
  // 同一进程承载多个会话（同一 ZCode 窗口新开任务会复用 app-server 进程，
  // 字节层不可拆分）计为一行合计，按 n_sessions 计入任务总数
  const tasks = s.tasks ?? [];
  const taskCount = tasks.reduce((n, t) => n + Math.max(1, t.nSessions || 0), 0);
  if (taskCount >= 2) {
    taskHideStreak = 0;
    taskCard.hidden = false;
    taskList.textContent = "";
    for (const t of tasks) {
      const row = document.createElement("div");
      row.className = "task-row";
      const dot = document.createElement("span");
      dot.className = t.streaming ? "dot live" : "dot idle";
      const label = document.createElement("span");
      label.className = "task-sess";
      label.textContent =
        t.nSessions >= 2
          ? `${t.nSessions} 会话（同进程合计） · 进程 ${t.pid}`
          : t.session
            ? `会话 …${t.session.slice(-6)} · 进程 ${t.pid}`
            : `未归属进程 ${t.pid}`;
      const tps = document.createElement("span");
      tps.className = "task-tps";
      tps.textContent = t.streaming ? `${fmtTps(t.tps)} t/s` : "待机";
      if (t.streaming) tps.style.color = speedColor(t.tps, SPEED_TIERS);
      row.append(dot, label, tps);
      taskList.append(row);
    }
  } else if (taskHideStreak < 3) {
    taskHideStreak++;
    if (taskHideStreak >= 3) taskCard.hidden = true;
  }

  subCurrent.textContent = s.isStarting
    ? "生成已启动 · 等待模型输出（统计中…）"
    : s.liveSource === "io"
      ? s.ramping
        ? "实时实测 · 统计中…（30s 滑窗建立中）"
        : `实时实测 · 进程流式输出（30s 滑窗实测${taskCount >= 2 ? ` · ${taskCount} 任务聚合` : ""}）`
      : s.isEstimating
        ? "生成中 · 此段无增量字节，按近期真实速度估算 ≈"
        : "待机 · 已无生成任务";
  subAvg.textContent = `Σ输出 ÷ Σ生成时长 · 今日 ${s.callsToday} 次调用`;
  subTotal.textContent = `输出 ${fmtTokens(s.outputTokens)} · 输入 ${fmtTokens(s.inputTokens)} · 缓存命中率 ${cacheHitRate(s)}`;
  document.body.classList.toggle("live", s.isLive || s.isStarting);
  document.body.classList.toggle("est", s.isEstimating);
  const petState: "idle" | "running" | "estimating" | "starting" = s.isStarting
    ? "starting"
    : s.liveSource === "io"
      ? "running"
      : "idle";
  petWidget.setLive(s.currentTps, petState);
  // 多任务分进程明细：桌宠气泡 ≥2 任务时展开分任务行（与完整面板任务卡同口径；
  // 单任务/回退/启动期传空，气泡只显示聚合值）
  petWidget.setTasks(
    taskCount >= 2
      ? tasks.map((t) => ({
          label:
            t.nSessions >= 2
              ? `${t.nSessions}会话·${t.pid}`
              : t.session
                ? `…${t.session.slice(-6)}`
                : `进程 ${t.pid}`,
          tps: t.tps,
          streaming: t.streaming,
        }))
      : []
  );
  liveDot.className = statusClass(s);
  liveText.textContent = s.isLive || s.isStarting ? "生成中" : s.isEstimating ? "估算中" : "待机";
  updatedAt.textContent = `更新于 ${fmtClock(s.nowMs)}`;
  floatDot.className = statusClass(s);
  floatTps.textContent = s.isStarting
    ? "…"
    : (s.isEstimating && s.liveSource !== "io" ? "≈" : "") + fmtTps(s.currentTps);
  petWidget.setLast(s.lastCallTps);
  // 胶囊第二行：上轮均速（落盘口径），按速度分档着色，无数据时显示 --
  floatLast.textContent = s.lastCallTps > 0 ? fmtTps(s.lastCallTps) : "--";
  floatLast.style.color = speedColor(s.lastCallTps, SPEED_TIERS);

  // 窗口标题同步实时速度，任务栏/Alt+Tab 可直接看到
  const title = `${s.isLive || s.isStarting ? "▶" : s.isEstimating ? "≈" : "⏸"} ${s.isStarting ? "…" : fmtTps(s.currentTps)} t/s · ${s.callsToday} 次 · ZCode 速度仪表盘`;
  document.title = title;
  try {
    getCurrentWindow().setTitle(title).catch(() => {});
  } catch {
    // 浏览器预览模式无 Tauri API
  }

  stDir.textContent = `监控 ${s.rolloutDir}`;
  stCalls.textContent = `今日调用 ${s.callsToday} 次`;
  stSessions.textContent = `${s.sessionsToday} 个会话`;
  stLast.textContent = `最近活动 ${fmtClock(s.lastActivityMs)}`;

  lastSpark = s.spark;
  lastNowMs = s.nowMs;
  liveTpsNow = s.currentTps;
  liveActive = s.isLive || s.isEstimating;
  sparkColor = s.isLive ? "#22d3ee" : s.isEstimating ? "#fbbf24" : "#64748b";
  // 峰值标签按当前展示的档位取数（15m = payload spark；长档位 = 5s 缓存 + 实时）
  const shown = chartRange === 15 ? s.spark : (chartCache?.buckets ?? []);
  const peak = Math.max(10, ...shown, s.currentTps);
  chartMax.textContent = `峰值 ${fmtTps(peak)} t/s`;
  redrawSpark();
}

window.addEventListener("resize", redrawSpark);

// ---- 曲线时间范围下拉（自绘 dropdown，与悬浮窗样式下拉同款交互）----
const chartDropdown = $("chart-window");
const chartRangeOptions = Array.from(
  $<HTMLElement>("chart-window-list").querySelectorAll<HTMLButtonElement>("button[data-value]"),
);

function applyChartRangeUi() {
  const cfg = chartRangeCfg();
  $("chart-window-label").textContent = cfg.label;
  $("chart-title").textContent = `近 ${cfg.label}输出速度（${cfg.bucketLabel} · token/s，横轴为真实时刻）`;
  for (const opt of chartRangeOptions) {
    opt.classList.toggle("selected", Number(opt.dataset.value) === chartRange);
  }
}

function setChartDropdownOpen(open: boolean) {
  chartDropdown.classList.toggle("open", open);
  $<HTMLButtonElement>("chart-window-btn").setAttribute("aria-expanded", String(open));
}

function selectChartRange(r: ChartRange) {
  if (r === chartRange) {
    setChartDropdownOpen(false);
    return;
  }
  setChartDropdownOpen(false);
  chartRange = r;
  localStorage.setItem(CHART_RANGE_KEY, String(r));
  if (r === 15) chartCache = null;
  applyChartRangeUi();
  window.clearInterval(chartTimer);
  chartTimer = 0;
  if (r !== 15) {
    void refreshChartStats();
    chartTimer = window.setInterval(() => void refreshChartStats(), 5000);
  }
  redrawSpark();
}

$<HTMLButtonElement>("chart-window-btn").addEventListener("click", () =>
  setChartDropdownOpen(!chartDropdown.classList.contains("open")),
);
for (const opt of chartRangeOptions) {
  opt.addEventListener("click", () => selectChartRange(Number(opt.dataset.value) as ChartRange));
}
window.addEventListener("mousedown", (e) => {
  if (chartDropdown.classList.contains("open") && !chartDropdown.contains(e.target as Node)) {
    setChartDropdownOpen(false);
  }
});
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && chartDropdown.classList.contains("open")) setChartDropdownOpen(false);
});
// 启动恢复上次选择的档位（长档位立即拉取一次并起 5s 定时器）
applyChartRangeUi();
if (chartRange !== 15) {
  void refreshChartStats();
  chartTimer = window.setInterval(() => void refreshChartStats(), 5000);
}

// ---- 模式与悬浮窗样式（自绘下拉，替代原生 select：WebView2 弹层在浅色系统主题下看不清） ----
function applyModeUi(mode: string) {
  document.body.classList.toggle("float-mode", mode === "float");
}

const STYLE_LABELS: Record<string, string> = {
  pet: "桌宠",
  gauge: "仪表悬浮窗",
  pill: "胶囊悬浮窗",
};

let currentStyle = localStorage.getItem("floatStyle") ?? "gauge";
const styleDropdown = $("float-style");
const styleOptions = Array.from(
  $<HTMLElement>("float-style-list").querySelectorAll<HTMLButtonElement>("button[data-value]"),
);

function applyStyleUi(style: string) {
  currentStyle = style;
  document.body.classList.toggle("style-pet", style === "pet");
  document.body.classList.toggle("style-gauge", style === "gauge");
  document.body.classList.toggle("style-pill", style === "pill");
  $("float-style-label").textContent = STYLE_LABELS[style] ?? STYLE_LABELS.gauge;
  for (const opt of styleOptions) {
    opt.classList.toggle("selected", opt.dataset.value === style);
  }
}

function setStyleDropdownOpen(open: boolean) {
  styleDropdown.classList.toggle("open", open);
  $<HTMLButtonElement>("float-style-btn").setAttribute("aria-expanded", String(open));
}

function selectFloatStyle(style: string) {
  setStyleDropdownOpen(false);
  localStorage.setItem("floatStyle", style);
  applyStyleUi(style);
  if (document.body.classList.contains("float-mode")) {
    tauriInvoke("set_float_style", { style }).catch(() => {});
  }
}

function requestMode(mode: "full" | "float") {
  if (!hasTauri) {
    applyModeUi(mode);
    return;
  }
  tauriInvoke("set_mode", { mode, style: currentStyle }).catch(() => {});
}

$("btn-float").addEventListener("click", () => requestMode("float"));
$("float-gauge-expand").addEventListener("click", () => requestMode("full"));
$("float-pill-expand").addEventListener("click", () => requestMode("full"));
$("float-pet-expand").addEventListener("click", () => requestMode("full"));
$("float-pet-cycle").addEventListener("click", () => {
  currentPetPack = petWidget.cyclePack();
  localStorage.setItem(PET_PACK_KEY, currentPetPack);
});

// ---- 模块显隐与排序（顶栏 ⚙ 设置弹窗）----
// 模块 = 完整面板 main 里可整块开关/排序的卡片组；并发任务卡跟着仪表盘走（不单列）。
// 默认显示 仪表盘 / 网速监控 / 近 15 分钟输出速度，快照防护与上传记录默认隐藏
// （快照相关内容显式开启才出现）。配置存 localStorage，跨重启保持。
type ModuleId = "gauges" | "net" | "guard" | "chart";
const MODULE_DEFS: { id: ModuleId; name: string; desc: string }[] = [
  { id: "gauges", name: "仪表盘", desc: "当前速度 / 今日平均 / 今日总量（多任务时含并发任务明细）" },
  { id: "net", name: "网速监控", desc: "整机上传/下载速度 · ZCode 连接归属 · 今日累计" },
  { id: "guard", name: "快照防护与上传记录", desc: "防护开关 · 今日快照上传 · 上传记录列表" },
  { id: "chart", name: "输出速度曲线", desc: "近 15 分钟 / 1 小时 / 6 小时 / 24 小时速度曲线 · 模型详情入口" },
];
const MODULES_KEY = "modules.v1";
const MODULES_DEFAULT_ORDER: ModuleId[] = ["gauges", "net", "chart", "guard"];
const MODULES_DEFAULT_HIDDEN: ModuleId[] = ["guard"];

interface ModulesConfig {
  /** 全部模块的全局顺序（含隐藏的——重新勾选时回到原位，排序对隐藏行同样有效） */
  order: ModuleId[];
  /** 隐藏的模块 id */
  hidden: ModuleId[];
}

/** 读 localStorage 并兜底清洗：JSON 损坏回默认；未知 id 剔除、缺失的按默认序补到末尾、
 *  去重——手改/旧版本配置不致丢模块或抛错 */
function loadModulesConfig(): ModulesConfig {
  const fallback = (): ModulesConfig => ({
    order: [...MODULES_DEFAULT_ORDER],
    hidden: [...MODULES_DEFAULT_HIDDEN],
  });
  try {
    const raw = localStorage.getItem(MODULES_KEY);
    if (!raw) return fallback();
    const parsed = JSON.parse(raw) as Partial<{ order: unknown; hidden: unknown }>;
    const known = new Set<string>(MODULES_DEFAULT_ORDER);
    const clean = (v: unknown): ModuleId[] => [
      ...new Set(Array.isArray(v) ? v.filter((x): x is ModuleId => typeof x === "string" && known.has(x)) : []),
    ];
    const order = clean(parsed.order);
    for (const id of MODULES_DEFAULT_ORDER) if (!order.includes(id)) order.push(id);
    return { order, hidden: clean(parsed.hidden) };
  } catch {
    return fallback();
  }
}

const moduleWraps = new Map<ModuleId, HTMLElement>(
  MODULE_DEFS.map((m) => [m.id, document.querySelector<HTMLElement>(`.module-wrap[data-module="${m.id}"]`)!]),
);
const mainEl = document.querySelector("main")!;
const footerEl = $("statusbar");
const modulesEmpty = $("modules-empty");
let modulesCfg = loadModulesConfig();

/** 按配置重排/隐藏模块：wrapper 为 display:contents，卡片仍是 main 的 flex 项，
 *  顺序 = order 数组过滤隐藏项；footer 恒在最后。全部隐藏时给占位提示（顶栏 ⚙
 *  始终可再打开，但空白页不解释会像坏了）。网速/快照卡自身还带数据可用性的
 *  hidden 逻辑（renderNet/renderSnapshot），与模块开关相互独立、取交集显示 */
function applyModules() {
  for (const id of modulesCfg.order) {
    const wrap = moduleWraps.get(id);
    if (!wrap) continue;
    mainEl.insertBefore(wrap, footerEl);
    wrap.hidden = modulesCfg.hidden.includes(id);
  }
  modulesEmpty.hidden = modulesCfg.order.some((id) => !modulesCfg.hidden.includes(id));
  mainEl.insertBefore(modulesEmpty, footerEl);
}

const saveModulesConfig = () => {
  localStorage.setItem(MODULES_KEY, JSON.stringify(modulesCfg));
};
applyModules();

// ---- 设置弹窗：自绘勾选（.pet-chk 同款，禁原生 checkbox）+ ↑↓ 排序，即时生效 ----
const settingsModal = $("settings-modal");
const settingsList = $("settings-modules");
const setSettingsOpen = (open: boolean) => {
  settingsModal.style.display = open ? "flex" : "none";
};

function renderSettingsRows() {
  settingsList.textContent = "";
  modulesCfg.order.forEach((id, idx) => {
    const def = MODULE_DEFS.find((m) => m.id === id)!;
    const shown = !modulesCfg.hidden.includes(id);
    const row = document.createElement("div");
    row.className = shown ? "settings-row" : "settings-row off";

    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = `ghost-btn settings-toggle${shown ? " on" : ""}`;
    toggle.title = shown ? "隐藏该模块" : "显示该模块";
    const chk = document.createElement("span");
    chk.className = "pet-chk";
    chk.setAttribute("aria-hidden", "true");
    toggle.append(chk, document.createTextNode("显示"));
    toggle.addEventListener("click", () => {
      modulesCfg.hidden = shown
        ? [...modulesCfg.hidden, id]
        : modulesCfg.hidden.filter((x) => x !== id);
      saveModulesConfig();
      applyModules();
      renderSettingsRows();
    });

    const info = document.createElement("div");
    info.className = "settings-info";
    const name = document.createElement("span");
    name.className = "settings-name";
    name.textContent = def.name;
    const desc = document.createElement("span");
    desc.className = "settings-desc";
    desc.textContent = def.desc;
    info.append(name, desc);

    const move = (dir: -1 | 1) => {
      const j = idx + dir;
      if (j < 0 || j >= modulesCfg.order.length) return;
      [modulesCfg.order[idx], modulesCfg.order[j]] = [modulesCfg.order[j], modulesCfg.order[idx]];
      saveModulesConfig();
      applyModules();
      renderSettingsRows();
    };
    const up = document.createElement("button");
    up.type = "button";
    up.className = "ghost-btn settings-move";
    up.textContent = "↑";
    up.title = "上移";
    up.disabled = idx === 0;
    up.addEventListener("click", () => move(-1));
    const down = document.createElement("button");
    down.type = "button";
    down.className = "ghost-btn settings-move";
    down.textContent = "↓";
    down.title = "下移";
    down.disabled = idx === modulesCfg.order.length - 1;
    down.addEventListener("click", () => move(1));
    const orderBtns = document.createElement("div");
    orderBtns.className = "settings-order";
    orderBtns.append(up, down);

    row.append(toggle, info, orderBtns);
    settingsList.append(row);
  });
}

$("btn-settings").addEventListener("click", () => {
  renderSettingsRows();
  void refreshAutostart();
  setSettingsOpen(true);
});
$("settings-close").addEventListener("click", () => setSettingsOpen(false));
$("settings-reset").addEventListener("click", () => {
  modulesCfg = {
    order: [...MODULES_DEFAULT_ORDER],
    hidden: [...MODULES_DEFAULT_HIDDEN],
  };
  saveModulesConfig();
  applyModules();
  renderSettingsRows();
});
// 点遮罩空白处 / Esc 关闭（与模型详情、防护确认弹窗同一习惯）
settingsModal.addEventListener("mousedown", (e) => {
  if (e.target === settingsModal) setSettingsOpen(false);
});
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && settingsModal.style.display === "flex") setSettingsOpen(false);
});

// ---- 自动启动（设置弹窗「自动启动」区）：三态 off / boot / follow ----
// 与模块显隐不同，事实源在后端（Windows 注册表 / mac LaunchAgent，见
// autostart.rs）：打开弹窗时回读真实状态、点击即写并按写后回读刷新选中态。
// 不存 localStorage——注册表/plist 是唯一事实源，避免两处状态漂移。
// 浏览器预览（无 Tauri）仅展示不可写
type AutostartMode = "off" | "boot" | "follow";
const AUTOSTART_DEFS: { id: AutostartMode; name: string; desc: string }[] = [
  { id: "off", name: "关闭", desc: "不自动启动，需要时手动打开" },
  { id: "boot", name: "开机自动启动", desc: "登录后常驻启动，按上次退出时的形态（完整面板 / 悬浮窗）显示" },
  { id: "follow", name: "跟随 ZCode 启动", desc: "登录后静默待命（仅托盘图标、不显示窗口），检测到 ZCode 正在运行时自动亮出面板" },
];
const autostartList = $("settings-autostart");
/** null = 读取中/预览模式（三行都不显示选中） */
let autostartCurrent: AutostartMode | null = null;
const autostartError = document.createElement("div");
autostartError.className = "autostart-error";

function renderAutostartRows() {
  autostartList.textContent = "";
  for (const def of AUTOSTART_DEFS) {
    const on = autostartCurrent === def.id;
    const row = document.createElement("div");
    row.className = on ? "settings-row" : "settings-row off";

    // 单选语义：选中行复用模块行的自绘勾选样式（.pet-chk），未选中呈 off 态
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = `ghost-btn settings-toggle${on ? " on" : ""}`;
    toggle.title = on ? "当前模式" : "切换到该模式";
    const chk = document.createElement("span");
    chk.className = "pet-chk";
    chk.setAttribute("aria-hidden", "true");
    toggle.append(chk, document.createTextNode(on ? "已选" : "选择"));
    toggle.addEventListener("click", () => void applyAutostart(def.id));

    const info = document.createElement("div");
    info.className = "settings-info";
    const name = document.createElement("span");
    name.className = "settings-name";
    name.textContent = def.name;
    const desc = document.createElement("span");
    desc.className = "settings-desc";
    desc.textContent = def.desc;
    info.append(name, desc);

    row.append(toggle, info);
    autostartList.append(row);
  }
  autostartList.append(autostartError);
}

/** 打开弹窗时从后端回读真实状态（注册表/plist 即事实源，不信任上次内存值） */
const refreshAutostart = async () => {
  const mode = await tauriInvoke<string>("autostart_get");
  autostartCurrent = mode === "boot" || mode === "follow" ? mode : "off";
  autostartError.textContent = "";
  renderAutostartRows();
};

const applyAutostart = async (mode: AutostartMode) => {
  if (!hasTauri || mode === autostartCurrent) return;
  const prev = autostartCurrent;
  autostartCurrent = mode;
  autostartError.textContent = "";
  renderAutostartRows();
  try {
    // 后端写完回读生效值（写失败抛错，前端回滚选中态并展示原因）
    const applied = await tauriInvoke<string>("autostart_set", { mode });
    autostartCurrent = applied === "boot" || applied === "follow" ? applied : "off";
  } catch (e) {
    autostartCurrent = prev;
    autostartError.textContent = `设置失败：${e}`;
  }
  renderAutostartRows();
};
renderAutostartRows();

// ---- 重新校准（当前速度卡左上角 ⟳）：丢弃字节→token 系数样本回到先验 ----
const btnRecal = $<HTMLButtonElement>("btn-recal");
if (!hasTauri) btnRecal.style.display = "none"; // 浏览器预览无真实校准
let recalTimer = 0;
const flashRecal = () => {
  btnRecal.classList.add("done");
  window.clearTimeout(recalTimer);
  recalTimer = window.setTimeout(() => btnRecal.classList.remove("done"), 1500);
};
btnRecal.addEventListener("click", () => {
  tauriInvoke("recalibrate").catch((err) => console.warn("recalibrate 失败:", err));
});

// ---- 应用内更新：footer 右下角版本号（点击=手动检查）；后端启动+每日静默检查，
//      发现新版本自动预下载并弹此卡片；无更新/网络异常静默，不打扰 ----
interface UpdateEvent {
  state: "available" | "downloading" | "ready" | "launching" | "error";
  currentVersion: string;
  newVersion: string;
  releaseUrl: string;
  notes: string;
  downloadedBytes: number;
  totalBytes: number;
  message: string;
}
type CheckOutcome =
  | { kind: "upToDate"; current: string }
  | { kind: "available"; current: string; newVersion: string }
  | { kind: "failed"; message: string };

const DISMISS_KEY = "updateDismissed.v1";
const stVersion = $("st-version");
const updateCard = $("update-card");
const updateVersion = $("update-version");
const updateCurrent = $("update-current");
const updateNotes = $("update-notes");
const updateLink = $("update-link");
const updateProgress = $("update-progress");
const updateBarFill = $("update-bar-fill");
const updateProgressText = $("update-progress-text");
const updateInstall = $<HTMLButtonElement>("update-install");
const updateStatus = $("update-status");
const updateToast = $("update-toast");
let currentVersion = "";
let updateDismissed = localStorage.getItem(DISMISS_KEY) ?? "";
let toastTimer = 0;
let checkingUpdate = false;

if (!hasTauri) {
  stVersion.style.display = "none"; // 浏览器预览无后端，隐藏入口
} else {
  tauriInvoke<string>("app_version").then((v) => {
    if (v) {
      currentVersion = v;
      stVersion.textContent = `v${v}`;
    }
  });
}

const toast = (msg: string) => {
  updateToast.textContent = msg;
  updateToast.classList.add("show");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => updateToast.classList.remove("show"), 2600);
};

const setUpdateProgress = (done: number, total: number) => {
  const pct = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0;
  updateBarFill.style.width = `${pct}%`;
  updateProgressText.textContent =
    total > 0 ? `${pct}% · ${(done / 1048576).toFixed(1)}/${(total / 1048576).toFixed(1)} MB` : "下载中…";
};

/** 弹卡片（用户没关过这个版本的提示）；关过则只给版本号挂小圆点 */
const maybeOpenCard = (e: UpdateEvent) => {
  if (updateDismissed && updateDismissed === e.newVersion) {
    stVersion.classList.add("has-update");
  } else {
    updateCard.classList.add("show");
  }
};

function applyUpdateEvent(e: UpdateEvent) {
  if (e.currentVersion) currentVersion = e.currentVersion;
  if (e.newVersion) {
    updateVersion.textContent = `v${e.newVersion}`;
    updateCurrent.textContent = currentVersion ? `v${currentVersion}` : "";
    updateNotes.textContent = e.notes;
    updateLink.dataset.url = e.releaseUrl;
  }
  updateStatus.classList.toggle("error", e.state === "error");
  updateStatus.textContent = e.message;
  switch (e.state) {
    case "available":
      updateProgress.style.display = "none";
      updateInstall.disabled = false;
      updateInstall.textContent = "⤓ 立即更新";
      maybeOpenCard(e);
      break;
    case "downloading":
      updateProgress.style.display = "";
      setUpdateProgress(e.downloadedBytes, e.totalBytes);
      updateInstall.disabled = true;
      updateInstall.textContent = "⤓ 下载中…";
      maybeOpenCard(e);
      break;
    case "ready":
      updateProgress.style.display = "none";
      updateInstall.disabled = false;
      updateInstall.textContent = "⤓ 立即安装";
      maybeOpenCard(e);
      break;
    case "launching":
      updateInstall.disabled = true;
      updateInstall.textContent = "正在安装…";
      updateCard.classList.add("show");
      break;
    case "error":
      updateInstall.disabled = false;
      updateInstall.textContent = "重试";
      updateCard.classList.add("show");
      break;
  }
}

async function manualCheck() {
  if (!hasTauri || checkingUpdate) return;
  checkingUpdate = true;
  // 先清"已关闭提示"再检查：手动检查视为重新关注，"available" 事件（可能先于
  // invoke 返回到达）到达时能正常弹卡片
  updateDismissed = "";
  localStorage.removeItem(DISMISS_KEY);
  stVersion.classList.remove("has-update");
  stVersion.classList.add("checking");
  try {
    const r = await tauriInvoke<CheckOutcome>("check_update");
    if (r?.kind === "upToDate") toast(`已是最新版本 v${r.current}`);
    else if (r?.kind === "failed") toast("检查更新失败：网络异常，请稍后重试");
    // available → 卡片由 "update" 事件渲染
  } finally {
    stVersion.classList.remove("checking");
    checkingUpdate = false;
  }
}

stVersion.addEventListener("click", () => manualCheck());
updateInstall.addEventListener("click", () => {
  updateInstall.disabled = true;
  updateInstall.textContent = "准备中…";
  tauriInvoke("install_update").catch((err) => {
    updateStatus.classList.add("error");
    updateStatus.textContent = String(err);
    updateInstall.disabled = false;
    updateInstall.textContent = "重试";
  });
});
$("update-close").addEventListener("click", () => {
  updateCard.classList.remove("show");
  const v = updateVersion.textContent?.replace(/^v/, "") ?? "";
  if (v) {
    updateDismissed = v;
    localStorage.setItem(DISMISS_KEY, v);
  }
});
updateLink.addEventListener("click", (e) => {
  e.preventDefault();
  const url = updateLink.dataset.url;
  if (url) tauriInvoke("open_url", { url }).catch(() => {});
});
// 手动下载：应用内安装之外的自助路径（安装失败/不想自动装时直达 Release 页）
const RELEASES_URL = "https://github.com/Masterchiefm/zcode-speed-panel/releases";
$("update-manual").addEventListener("click", () => {
  const url = updateLink.dataset.url || RELEASES_URL;
  tauriInvoke("open_url", { url }).catch(() => {});
});

$("float-style-btn").addEventListener("click", () => {
  setStyleDropdownOpen(!styleDropdown.classList.contains("open"));
});
for (const opt of styleOptions) {
  opt.addEventListener("click", () => selectFloatStyle(opt.dataset.value!));
}

// ---- mac 引导提示：主窗口从隐藏→显示时后端发 "tray-hint"（Windows 不发，前端永不显示）----
const trayHint = $("tray-hint");
let trayHintTimer = 0;
const showTrayHint = () => {
  trayHint.classList.add("show");
  window.clearTimeout(trayHintTimer);
  trayHintTimer = window.setTimeout(() => trayHint.classList.remove("show"), 6000);
};
trayHint.addEventListener("click", () => {
  window.clearTimeout(trayHintTimer);
  trayHint.classList.remove("show");
});

// 顶栏/悬浮窗拖动：mousedown 调 startDragging（按钮、下拉框除外）。
// 目标自身带 data-tauri-drag-region 时由 Tauri 内核直接处理（跳过，避免双重拖动）
function enableDrag(el: HTMLElement) {
  el.addEventListener("mousedown", (e) => {
    const target = e.target as HTMLElement;
    if (target.closest("button, select, input, .dropdown")) return;
    if (target.hasAttribute("data-tauri-drag-region")) return;
    e.preventDefault();
    import("@tauri-apps/api/window")
      .then(({ getCurrentWindow: g }) => g().startDragging().catch(() => {}))
      .catch(() => {});
  });
}
enableDrag($("app-header"));
enableDrag($("float-gauge"));
enableDrag($("float-pill"));
enableDrag($("float-pet"));

// 悬浮窗双击 = 恢复完整面板。桌宠不参与：双击已用于换宠物（pet.ts），
// 其恢复走 ⤢ 按钮 / 右键菜单 / 托盘。按钮上的双击不触发（click 已处理）
for (const id of ["float-gauge", "float-pill"]) {
  $(id).addEventListener("dblclick", (e) => {
    if ((e.target as HTMLElement).closest("button, select, input, .dropdown")) return;
    requestMode("full");
  });
}

// ---- 自绘标题栏：拖动移动、双击最大化，— / ▢ / ✕ 窗口控制 ----
const currentWindow = () => import("@tauri-apps/api/window").then((m) => m.getCurrentWindow());
$("app-header").addEventListener("dblclick", (e) => {
  if ((e.target as HTMLElement).closest("button, select, input, .dropdown")) return;
  if (hasTauri) tauriInvoke("toggle_maximize_safe").catch(() => {});
});
if (hasTauri) {
  $("wc-min").addEventListener("click", () => {
    currentWindow().then((w) => w.minimize()).catch(() => {});
  });
  $("wc-max").addEventListener("click", () => {
    // mac 用原生 Overlay 标题栏（真交通灯，绿点=原生全屏），此按钮已隐藏；
    // Windows ▢ = 安全最大化（与双击顶栏同款）
    tauriInvoke("toggle_maximize_safe").catch(() => {});
  });
  $("wc-close").addEventListener("click", () => requestMode("float"));
} else {
  // 浏览器预览无窗口控制
  ($("win-controls") as HTMLElement).style.display = "none";
}

applyStyleUi(localStorage.getItem("floatStyle") ?? "gauge");

// ---- 模型速度趋势详情弹窗（图表卡片"模型详情"入口；复用同一个 tauriInvoke） ----
initModelStats(tauriInvoke);

// ---- 快照防护卡片（网络监控卡下方；状态随 metrics payload 的 guard 字段推送） ----
initGuard(tauriInvoke);

if (hasTauri) {
  (async () => {
    const { listen } = await import("@tauri-apps/api/event");
    await listen<SnapshotPayload>("metrics", (e) => {
      onSnapshot({ ...e.payload.snapshot, rolloutDir: e.payload.rolloutDir, guard: e.payload.guard });
      renderGuard(e.payload.guard ?? null);
    });
    await listen<string>("mode", (e) => applyModeUi(e.payload));
    await listen("tray-hint", () => showTrayHint());
    // 重新校准完成（手动或漂移自动触发）：按钮闪 ✓ 反馈
    await listen("recalibrated", flashRecal);
    await listen<string>("float-style", (e) => {
      localStorage.setItem("floatStyle", e.payload);
      applyStyleUi(e.payload);
    });
    await listen<UpdateEvent>("update", (e) => applyUpdateEvent(e.payload));
    const p = await tauriInvoke<SnapshotPayload>("snapshot");
    if (p) {
      applyModeUi(p.mode);
      if (p.floatStyle) {
        localStorage.setItem("floatStyle", p.floatStyle);
        applyStyleUi(p.floatStyle);
      }
      onSnapshot({ ...p.snapshot, rolloutDir: p.rolloutDir, guard: p.guard });
      renderGuard(p.guard ?? null);
    }
    // mac 启动引导（一次性）：页面就绪后主动领取，避免 setup 内 emit 早于加载被丢弃
    if (await tauriInvoke<boolean>("tray_hint_once")) showTrayHint();
  })().catch((err) => {
    document.title = `初始化失败 · ZCode 速度仪表盘`;
    subCurrent.textContent = `Tauri 初始化失败：${err}`;
  });
} else {
  startMock(onSnapshot);
}
