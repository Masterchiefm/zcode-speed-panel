// 模型速度趋势视图（曲线卡内，与整体输出速度曲线拨杆互斥切换）：按模型分类的
// 速度折线（统一 90 桶）与窗口统计。数据来自后端 model_stats 命令（只读查询
// usage 库 model_usage 表现算聚合，零本地存储）；视图激活期间每 5s 拉取一次、
// 每 1s 按墙钟相位平移重画，取消激活即停。
// 时间轴与整体曲线（gauges.drawSpark）完全同规格：同一组时间范围档位（由
// main.ts 的范围下拉决定，切视图不改变范围）、同一桶宽与网格间隔，桶按绝对
// 墙钟槽对齐（后端 div_euclid），x 映射锚定"下一桶边界"，网格线取整分时刻，
// 曲线随时间平移不变形、两视图横轴逐像素对齐。
// 图例为可点击 chips：切换该模型显隐（折线与底部统计行同步过滤，至少保留
// 一个——全取消自动回全选），行尾附「全选 / 仅 Top3」；选中集合存
// localStorage（modelStats.visible.v1）。
import { fmtClock, fmtTokens, fmtTps, niceCeil } from "./gauges";

const FONT = `"Segoe UI", "Microsoft YaHei", sans-serif`;

/** 折线/图例配色（按 series 顺序循环） */
const PALETTE = ["#22d3ee", "#a78bfa", "#34d399", "#fbbf24", "#f87171", "#60a5fa", "#f472b6", "#4ade80"];

/** 图例与统计行里模型名的截断长度（超过加 …，完整名放 title） */
const MODEL_NAME_MAX = 18;

/** 模型显隐选择的持久化键（存可见模型名数组；查不到 / 模型已全部不存在时回退全选） */
const VISIBLE_KEY = "modelStats.visible.v1";

const loadVisible = (): Set<string> => {
  try {
    const raw = localStorage.getItem(VISIBLE_KEY);
    const arr = raw ? (JSON.parse(raw) as unknown) : null;
    if (Array.isArray(arr)) {
      return new Set(arr.filter((x): x is string => typeof x === "string"));
    }
  } catch {
    // 存档损坏：忽略，回退全选
  }
  return new Set();
};

export interface ModelBucket {
  tps: number;
  calls: number;
  tokens: number;
}

export interface ModelSeries {
  model: string;
  buckets: ModelBucket[];
  totalCalls: number;
  totalTokens: number;
  avgTps: number;
  peakTps: number;
  share: number;
}

export interface ModelStatsPayload {
  windowMin: number;
  bucketMs: number;
  nowMs: number;
  series: ModelSeries[];
}

type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T | undefined>;

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

const shortModel = (name: string): string => (name.length > MODEL_NAME_MAX ? name.slice(0, MODEL_NAME_MAX) + "…" : name);

/** 与 gauges.drawSpark 同规则的画布按 DPR 适配 */
function fitCanvas(
  canvas: HTMLCanvasElement,
): { ctx: CanvasRenderingContext2D; w: number; h: number } | null {
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (w < 8 || h < 8) return null;
  const dpr = window.devicePixelRatio || 1;
  const pw = Math.round(w * dpr);
  const ph = Math.round(h * dpr);
  if (canvas.width !== pw || canvas.height !== ph) {
    canvas.width = pw;
    canvas.height = ph;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  return { ctx, w, h };
}

/** 可见图例项：series + 在完整 series 列表中的原始序号（决定配色，过滤后保持稳定） */
interface VisibleItem {
  s: ModelSeries;
  idx: number;
}

/** 每模型一条 tps 折线：y 轴 0~niceCeil(峰值×1.25)（与整体曲线同口径量化，
 *  数据微变不致整条曲线纵向缩放）+ 3 条横网格线；x 轴与 drawSpark 完全同映射——
 *  右缘 = 下一桶边界，网格线取 gridMs 整分时刻，数据点画在墙钟桶中心。
 *  只画 visible 里的模型，配色取各自在完整列表中的原始序号（隐藏再显示颜色不变） */
function drawModelChart(
  canvas: HTMLCanvasElement,
  p: ModelStatsPayload,
  visible: VisibleItem[],
  nowMs: number,
  gridMs: number,
) {
  const fit = fitCanvas(canvas);
  if (!fit) return;
  const { ctx, w, h } = fit;
  const padL = 40;
  const padR = 10;
  const padT = 10;
  const padB = 20;
  const iw = w - padL - padR;
  const ih = h - padT - padB;
  const n = visible[0]?.s.buckets.length ?? p.series[0]?.buckets.length ?? 90;
  const bucketMs = p.bucketMs;
  const peak = Math.max(
    10,
    niceCeil(Math.max(0, ...visible.flatMap((it) => it.s.buckets.map((b) => b.tps))) * 1.25),
  );
  const yAt = (v: number) => padT + ih - (Math.min(v, peak) / peak) * ih;

  // 3 条横网格线 + 刻度文字（顶 = 峰值档、中 = 半档、底 = 0）
  ctx.strokeStyle = "rgba(255,255,255,0.06)";
  ctx.fillStyle = "rgba(139,147,167,0.7)";
  ctx.font = `10px ${FONT}`;
  ctx.lineWidth = 1;
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  for (let i = 0; i <= 2; i++) {
    const y = padT + (ih * i) / 2;
    ctx.beginPath();
    ctx.moveTo(padL, y);
    ctx.lineTo(w - padR, y);
    ctx.stroke();
    ctx.fillText(fmtTps((peak * (2 - i)) / 2), padL - 6, y);
  }
  if (visible.length === 0) return;

  // ---- x 轴真实时刻刻度：与 drawSpark 同一映射（可对表验证）。
  //      最新桶结束时刻 = 下一个墙钟桶边界；右缘即"现在"（差 ≤1 桶），
  //      nowMs 在桶内滑动时整条曲线连续左移，网格线钉在整分不动
  const dx = iw / n;
  const tLastEnd = Math.floor(nowMs / bucketMs) * bucketMs + bucketMs;
  const xAt = (t: number) => padL + iw - ((tLastEnd - t) / bucketMs) * dx;
  ctx.textAlign = "center";
  ctx.textBaseline = "top";
  let t = Math.ceil((tLastEnd - n * bucketMs) / gridMs) * gridMs;
  for (; t <= tLastEnd; t += gridMs) {
    const gx = xAt(t);
    if (gx < padL || gx > padL + iw) continue;
    ctx.strokeStyle = "rgba(255,255,255,0.05)";
    ctx.beginPath();
    ctx.moveTo(gx, padT);
    ctx.lineTo(gx, padT + ih);
    ctx.stroke();
    ctx.fillText(fmtClock(t).slice(0, 5), gx, h - padB + 4);
  }
  // 右缘：当前时刻（靠右对齐避免溢出）
  ctx.textAlign = "right";
  ctx.fillStyle = "rgba(139,147,167,0.9)";
  ctx.fillText(`现在 ${fmtClock(nowMs).slice(0, 5)}`, padL + iw, h - padB + 4);

  ctx.lineWidth = 2;
  ctx.lineJoin = "round";
  for (const { s, idx } of visible) {
    ctx.strokeStyle = PALETTE[idx % PALETTE.length];
    ctx.beginPath();
    s.buckets.forEach((b, i) => {
      // 桶 i（0 = 最新）中心时刻：最新桶右缘 tLastEnd 往回 (i+0.5) 个桶
      const x = xAt(tLastEnd - (i + 0.5) * bucketMs);
      const y = yAt(b.tps);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.stroke();
  }
}

/** 模型详情视图控制器：setActive(true) 起数据轮询与平移重绘，false 全停；
 *  refresh() 在统计范围档位变化时立即重拉（5s 轮询照常继续）。
 *  画布/图例/统计行的 DOM 由本模块自管；显隐切换（CSS body.chart-view-model）
 *  与持久化在 main.ts——视图开关与时间范围都属于曲线卡整体 */
export interface ModelStatsController {
  setActive(active: boolean): void;
  refresh(): void;
}

/** 绑定模型详情视图：5s 数据轮询、1s 相位平移重绘。
 *  getRange 返回整体曲线当前的统计范围与网格间隔（单一事实源在 main.ts 的
 *  CHART_RANGES）——两视图共用同一条时间轴，切换拨杆不改变范围 */
export function initModelStats(
  invoke: InvokeFn,
  getRange: () => { windowMin: number; gridMs: number },
): ModelStatsController {
  const legend = $("model-legend");
  const canvas = $<HTMLCanvasElement>("model-chart");
  const empty = $("model-empty");
  const summary = $("model-summary");

  let active = false;
  let fetchTimer = 0;
  let slideTimer = 0;
  let lastPayload: ModelStatsPayload | null = null;
  // 模型显隐选择：Set 里的模型可见。至少保留一个（全取消自动回全选），
  // 持久化到 localStorage；轮询只是重画，选中集合在内存里自然保持不闪
  let visible = loadVisible();
  /** 本会话已见过的模型：null = 首帧未到；首帧按存档裁剪/回退，之后
   *  新出现的模型（换模型/新窗口）默认可见，不被旧存档静默隐藏 */
  let knownModels: Set<string> | null = null;

  const saveVisible = () => localStorage.setItem(VISIBLE_KEY, JSON.stringify([...visible]));

  /** 按当前 payload 的模型清单整理可见集合：
   *  首帧——存档里已不存在的模型剔除（全部失效回退全选）；
   *  后续帧——新出现的模型默认可见，消失的模型移出（保持存档干净） */
  const reconcileVisible = (models: string[]) => {
    if (knownModels === null) {
      for (const m of [...visible]) {
        if (!models.includes(m)) visible.delete(m);
      }
      if (visible.size === 0) models.forEach((m) => visible.add(m));
      knownModels = new Set(models);
      return;
    }
    for (const m of models) {
      if (!knownModels.has(m)) {
        knownModels.add(m);
        visible.add(m);
      }
    }
    for (const m of [...visible]) {
      if (!models.includes(m)) visible.delete(m);
    }
    if (visible.size === 0) models.forEach((m) => visible.add(m));
  };

  /** 切换一个模型的显隐；全取消时自动回到全选（至少保留一个） */
  const toggleModel = (model: string, models: string[]) => {
    if (visible.has(model)) visible.delete(model);
    else visible.add(model);
    if (visible.size === 0) models.forEach((m) => visible.add(m));
    saveVisible();
    render(lastPayload);
  };

  const selectAll = (models: string[]) => {
    models.forEach((m) => visible.add(m));
    saveVisible();
    render(lastPayload);
  };

  /** 仅 Top3：按 total_tokens 排序取前三（模型不足 3 个时等价全选） */
  const selectTop3 = (series: ModelSeries[]) => {
    const top3 = [...series].sort((a, b) => b.totalTokens - a.totalTokens).slice(0, 3).map((s) => s.model);
    visible = new Set(top3);
    saveVisible();
    render(lastPayload);
  };

  /** 空态：无 payload / 空窗口 / 拉取失败时占位（图例与统计行一并隐藏） */
  const showEmpty = (text: string) => {
    empty.textContent = text;
    empty.style.display = "flex";
    legend.style.display = "none";
    summary.style.display = "none";
  };

  /** 渲染一次 payload：图例 chips、统计行与折线（无数据时显示空状态）。
   *  图例与统计行只列可见模型；chip 配色用原始序号，隐藏再显示颜色不变 */
  const render = (p: ModelStatsPayload | null) => {
    if (!p) return;
    lastPayload = p;
    const models = p.series.map((s) => s.model);
    // 空窗口不动选择（否则会把存档清空，数据回来时选择丢失）
    if (models.length > 0) reconcileVisible(models);
    const visibleItems: VisibleItem[] = p.series
      .map((s, idx) => ({ s, idx }))
      .filter((it) => visible.has(it.s.model));
    const has = p.series.length > 0;
    empty.style.display = has ? "none" : "flex";
    if (!has) empty.textContent = "窗口内暂无调用数据";
    legend.style.display = has ? "flex" : "none";
    summary.style.display = has ? "flex" : "none";
    if (!has) return;
    legend.replaceChildren();
    summary.replaceChildren();
    p.series.forEach((s, i) => {
      const on = visible.has(s.model);
      const color = PALETTE[i % PALETTE.length];
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = on ? "model-chip on" : "model-chip";
      chip.title = `${s.model}（点击${on ? "隐藏" : "显示"}）`;
      chip.setAttribute("aria-pressed", String(on));
      const dot = document.createElement("span");
      dot.className = "model-dot";
      dot.style.background = color;
      const name = document.createElement("span");
      name.className = "model-chip-name";
      name.textContent = shortModel(s.model);
      chip.append(dot, name);
      chip.addEventListener("click", () => toggleModel(s.model, models));
      legend.append(chip);

      if (!on) return;
      const row = document.createElement("span");
      row.className = "model-stat-row";
      const rdot = document.createElement("span");
      rdot.className = "model-dot";
      rdot.style.background = color;
      rdot.title = s.model;
      const text = document.createElement("span");
      text.textContent = `${shortModel(s.model)} · 均速 ${fmtTps(s.avgTps)} t/s · 峰值 ${fmtTps(s.peakTps)} · ${s.totalCalls} 次 · ${fmtTokens(s.totalTokens)} token (${(s.share * 100).toFixed(1)}%)`;
      text.title = s.model;
      row.append(rdot, text);
      summary.append(row);
    });
    // 行尾操作：全选 / 仅 Top3
    const tools = document.createElement("span");
    tools.className = "model-legend-tools";
    const allBtn = document.createElement("button");
    allBtn.type = "button";
    allBtn.textContent = "全选";
    allBtn.title = "显示全部模型";
    allBtn.addEventListener("click", () => selectAll(models));
    const top3Btn = document.createElement("button");
    top3Btn.type = "button";
    top3Btn.textContent = "仅 Top3";
    top3Btn.title = "只显示 token 用量前三的模型";
    top3Btn.addEventListener("click", () => selectTop3(p.series));
    tools.append(allBtn, top3Btn);
    legend.append(tools);
    drawModelChart(canvas, p, visibleItems, Date.now(), getRange().gridMs);
  };

  /** 1s 相位平移重绘：只画布不重建 DOM——数据 5s 才变，期间曲线随墙钟连续左移 */
  const slide = () => {
    if (!active || !lastPayload || document.body.classList.contains("float-mode")) return;
    const items: VisibleItem[] = lastPayload.series
      .map((s, idx) => ({ s, idx }))
      .filter((it) => visible.has(it.s.model));
    if (items.length) drawModelChart(canvas, lastPayload, items, Date.now(), getRange().gridMs);
  };

  const fetchNow = () => {
    if (!active) return;
    // 收起为悬浮窗时 main 整体被 CSS 隐藏：跳过拉取（回到完整面板自动恢复）
    if (document.body.classList.contains("float-mode")) return;
    invoke<ModelStatsPayload>("model_stats", { windowMin: getRange().windowMin })
      .then((p) => {
        if (!active) return;
        if (p) render(p);
        else if (!lastPayload) showEmpty("统计暂不可用");
      })
      .catch((err) => {
        console.warn("[model_stats] invoke failed:", err);
        if (active && !lastPayload) showEmpty("统计读取失败");
      });
  };

  const setActive = (on: boolean) => {
    if (active === on) return;
    active = on;
    window.clearInterval(fetchTimer);
    fetchTimer = 0;
    window.clearInterval(slideTimer);
    slideTimer = 0;
    if (on) {
      if (!lastPayload) showEmpty("读取统计中…");
      fetchNow();
      fetchTimer = window.setInterval(fetchNow, 5000);
      slideTimer = window.setInterval(slide, 1000);
    }
  };

  window.addEventListener("resize", slide);
  return {
    setActive,
    refresh: () => {
      if (active) fetchNow(); // 换档立即重拉（5s 定时器继续按新档拉取）
    },
  };
}
