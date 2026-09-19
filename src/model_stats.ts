// 模型速度趋势详情弹窗：按模型分类的速度折线（统一 60 桶）与窗口统计。
// 数据来自后端 model_stats 命令（只读查询 usage 库 model_usage 表现算聚合，
// 零本地存储）；弹窗打开期间每 5s 拉取一次，关闭即停。
// 图例为可点击 chips：切换该模型显隐（折线与底部统计行同步过滤，至少保留
// 一个——全取消自动回全选），行尾附「全选 / 仅 Top3」；选中集合存
// localStorage（modelStats.visible.v1），5s 轮询刷新不闪。
import { fmtClock, fmtTokens, fmtTps } from "./gauges";

const FONT = `"Segoe UI", "Microsoft YaHei", sans-serif`;

/** 折线/图例配色（按 series 顺序循环） */
const PALETTE = ["#22d3ee", "#a78bfa", "#34d399", "#fbbf24", "#f87171", "#60a5fa", "#f472b6", "#4ade80"];

/** 统计窗口选项（分钟），与后端 clamp 档位一致 */
const WINDOW_OPTIONS = [
  { value: 10, label: "最近 10 分钟" },
  { value: 60, label: "最近 1 小时" },
  { value: 360, label: "最近 6 小时" },
];

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

interface ModelBucket {
  tps: number;
  calls: number;
  tokens: number;
}

interface ModelSeries {
  model: string;
  buckets: ModelBucket[];
  totalCalls: number;
  totalTokens: number;
  avgTps: number;
  peakTps: number;
  share: number;
}

interface ModelStatsPayload {
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

/** 每模型一条 tps 折线：y 轴 0~峰值×1.15 自适应 + 3 条横网格线，x 轴 5 个真实时刻刻度。
 *  只画 visible 里的模型，配色取各自在完整列表中的原始序号（隐藏再显示颜色不变） */
function drawModelChart(canvas: HTMLCanvasElement, p: ModelStatsPayload, visible: VisibleItem[]) {
  const fit = fitCanvas(canvas);
  if (!fit) return;
  const { ctx, w, h } = fit;
  const padL = 40;
  const padR = 10;
  const padT = 10;
  const padB = 20;
  const iw = w - padL - padR;
  const ih = h - padT - padB;
  const n = visible[0]?.s.buckets.length ?? p.series[0]?.buckets.length ?? 60;
  const spanMs = n * p.bucketMs;
  // 桶 i（0 = 最新）的中心时刻：右缘 ≈ 现在
  const tStart = p.nowMs - spanMs;
  const xAt = (t: number) => padL + (iw * (t - tStart)) / spanMs;
  const yMax = Math.max(10, Math.max(0, ...visible.flatMap((it) => it.s.buckets.map((b) => b.tps))) * 1.15);
  const yAt = (v: number) => padT + ih - (Math.min(v, yMax) / yMax) * ih;

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
    ctx.fillText(fmtTps((yMax * (2 - i)) / 2), padL - 6, y);
  }

  // x 轴 5 个时间刻度（HH:MM），竖向细网格线便于对表
  ctx.textAlign = "center";
  ctx.textBaseline = "top";
  for (let k = 0; k <= 4; k++) {
    const t = tStart + (spanMs * k) / 4;
    const gx = xAt(t);
    ctx.strokeStyle = "rgba(255,255,255,0.04)";
    ctx.beginPath();
    ctx.moveTo(gx, padT);
    ctx.lineTo(gx, padT + ih);
    ctx.stroke();
    ctx.fillText(fmtClock(t).slice(0, 5), gx, h - padB + 4);
  }
  if (visible.length === 0) return;

  ctx.lineWidth = 2;
  ctx.lineJoin = "round";
  for (const { s, idx } of visible) {
    ctx.strokeStyle = PALETTE[idx % PALETTE.length];
    ctx.beginPath();
    s.buckets.forEach((b, i) => {
      const x = xAt(tStart + (n - i - 0.5) * p.bucketMs);
      const y = yAt(b.tps);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.stroke();
  }
}

/** 绑定弹窗全部交互：入口按钮、窗口下拉、5s 轮询、绘制与关闭清理 */
export function initModelStats(invoke: InvokeFn): void {
  const modal = $("model-modal");
  const box = $("model-modal-box");
  const openBtn = $("btn-model-stats");
  const closeBtn = $("model-modal-close");
  const dropdown = $("model-window");
  const dropdownBtn = $<HTMLButtonElement>("model-window-btn");
  const dropdownLabel = $("model-window-label");
  const legend = $("model-legend");
  const canvas = $<HTMLCanvasElement>("model-chart");
  const empty = $("model-empty");
  const summary = $("model-summary");
  const options = Array.from(dropdown.querySelectorAll<HTMLButtonElement>("button[data-value]"));

  let isOpen = false;
  let timer = 0;
  let windowMin = 60; // 默认 1 小时
  let lastPayload: ModelStatsPayload | null = null;
  // 模型显隐选择：Set 里的模型可见。至少保留一个（全取消自动回全选），
  // 持久化到 localStorage；5s 轮询只是重画，选中集合在内存里自然保持不闪
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
    rerender();
  };

  const selectAll = (models: string[]) => {
    models.forEach((m) => visible.add(m));
    saveVisible();
    rerender();
  };

  /** 仅 Top3：按 total_tokens 排序取前三（模型不足 3 个时等价全选） */
  const selectTop3 = (series: ModelSeries[]) => {
    const top3 = [...series].sort((a, b) => b.totalTokens - a.totalTokens).slice(0, 3).map((s) => s.model);
    visible = new Set(top3);
    saveVisible();
    rerender();
  };

  const setDropdownOpen = (open: boolean) => {
    dropdown.classList.toggle("open", open);
    dropdownBtn.setAttribute("aria-expanded", String(open));
  };

  /** 渲染一次 payload：图例 chips、统计行与折线（无数据时显示空状态）。
   *  图例与统计行只列可见模型；chip 配色用原始序号，隐藏再显示颜色不变 */
  const render = (p: ModelStatsPayload) => {
    lastPayload = p;
    const models = p.series.map((s) => s.model);
    // 空窗口不动选择（否则会把存档清空，数据回来时选择丢失）
    if (models.length > 0) reconcileVisible(models);
    const visibleItems: VisibleItem[] = p.series
      .map((s, idx) => ({ s, idx }))
      .filter((it) => visible.has(it.s.model));
    const has = p.series.length > 0;
    empty.style.display = has ? "none" : "flex";
    legend.style.display = has ? "flex" : "none";
    summary.style.display = has ? "flex" : "none";
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
    if (has) {
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
      drawModelChart(canvas, p, visibleItems);
    }
  };

  /** 交互后按最近一次 payload 重画（不重新拉取，选中切换即时生效） */
  const rerender = () => {
    if (lastPayload) render(lastPayload);
  };

  const fetchNow = () => {
    // 收起为悬浮窗时弹窗已被 CSS 隐藏（窗口太小放不下）：停表关闭，不再空转拉取
    if (document.body.classList.contains("float-mode")) {
      close();
      return;
    }
    invoke<ModelStatsPayload>("model_stats", { windowMin })
      .then((p) => {
        if (p && isOpen) render(p);
      })
      .catch((err) => console.warn("[model_stats] invoke failed:", err));
  };

  const open = () => {
    if (isOpen) return;
    isOpen = true;
    modal.style.display = "flex";
    fetchNow();
    timer = window.setInterval(fetchNow, 5000);
  };

  const close = () => {
    if (!isOpen) return;
    isOpen = false;
    window.clearInterval(timer);
    modal.style.display = "none";
    setDropdownOpen(false);
  };

  openBtn.addEventListener("click", () => (isOpen ? close() : open()));
  closeBtn.addEventListener("click", close);
  // 点弹窗内容之外关闭（入口按钮自身除外，由上面的 click 切换开关；
  // 与 main.ts 的 float-menu / 样式下拉 mousedown 监听各自独立，互不影响）
  window.addEventListener("mousedown", (e) => {
    if (!isOpen) return;
    const t = e.target as Node;
    if (box.contains(t) || openBtn.contains(t)) return;
    close();
  });
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && isOpen) close();
  });
  window.addEventListener("resize", () => {
    if (!isOpen || !lastPayload) return;
    const items: VisibleItem[] = lastPayload.series
      .map((s, idx) => ({ s, idx }))
      .filter((it) => visible.has(it.s.model));
    if (items.length) drawModelChart(canvas, lastPayload, items);
  });

  dropdownBtn.addEventListener("click", () => setDropdownOpen(!dropdown.classList.contains("open")));
  for (const opt of options) {
    opt.addEventListener("click", () => {
      setDropdownOpen(false);
      windowMin = Number(opt.dataset.value) || 60;
      dropdownLabel.textContent = WINDOW_OPTIONS.find((w) => w.value === windowMin)?.label ?? `最近 ${windowMin} 分钟`;
      for (const o of options) o.classList.toggle("selected", o === opt);
      if (isOpen) fetchNow(); // 切窗口立即拉一次（定时器继续按新窗口拉取）
    });
  }
}
