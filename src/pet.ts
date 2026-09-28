// 桌宠宠物包（Codex Pet 格式，来自 dsh-desk 项目，MIT）
// 精灵图：1536 宽 × 8 列，每帧 192×cellHeight；行序对应动画

import { SPEED_TIERS, fmtTps, speedColor, speedTierIndex } from "./gauges";

const FONT = `"Segoe UI", "Microsoft YaHei", sans-serif`;

/** 气泡分任务行数上限（再多放不下时丢弃，聚合值仍在第一行） */
const MAX_TASK_ROWS = 6;

/** 单个并发任务行（分进程实测，来自 snapshot.tasks，main.ts 负责拼标签） */
export interface PetTask {
  label: string;
  tps: number;
  streaming: boolean;
}

/** 头顶气泡排版（随悬停进度在单行/两行之间插值） */
interface BubbleLayout {
  rows: Array<{ label: string; value: string; color: string }>;
  /** 悬停进度 0~1：文字与尺寸的插值系数 */
  t: number;
  fs: number;
  labelFs: number;
  lineH: number;
  gapY: number;
  padX: number;
  padY: number;
  colGap: number;
  /** 已乘悬停进度：过标签列随之滑入，避免数值被挤出框 */
  labelW: number;
  boxW: number;
  boxH: number;
  /** 气泡底边锚点（精灵头顶附近，画布坐标）：单行/两行切换时底边不动、向上生长 */
  bottom: number;
  /** 气泡可见度（待机为 0）：整块淡入淡出 */
  vis: number;
}

/** 单个动画：素材行 row；play = 显式播放列序（0 基列号，可重复/跳过坏帧），
 *  缺省按 0..frames-1 逐帧播完 */
export interface AnimDef {
  row: number;
  frames: number;
  play?: number[];
}

export interface PetPack {
  id: string;
  displayName: string;
  sheet: string;
  sheetW: number;
  cellW: number;
  cellH: number;
  rows: number;
  /** 行序 → 动画名与帧数 */
  anims: Record<string, AnimDef>;
  /** 待机组动画名：待机时每整行播完随机换一个播 */
  idleAnims: string[];
  frameMs: number;
}

export const PET_PACKS: PetPack[] = [
  {
    id: "yuexinmiao",
    displayName: "月薪喵",
    sheet: "pets/yuexinmiao/spritesheet.webp",
    sheetW: 1536,
    cellW: 192,
    cellH: 208,
    rows: 9,
    anims: {
      idle: { row: 0, frames: 6 },
      running_right: { row: 1, frames: 8 },
      running_left: { row: 2, frames: 8 },
      waving: { row: 3, frames: 4 },
      jumping: { row: 4, frames: 5 },
      failed: { row: 5, frames: 8 },
      waiting_permission: { row: 6, frames: 6 },
      running: { row: 7, frames: 6 },
      review: { row: 8, frames: 6 },
    },
    idleAnims: ["idle"],
    frameMs: 160,
  },
  {
    id: "maid-deepseek-whale",
    displayName: "鲸鱼女仆",
    sheet: "pets/maid-deepseek-whale/spritesheet.webp",
    sheetW: 1536,
    cellW: 192,
    cellH: 208,
    rows: 11,
    anims: {
      idle: { row: 0, frames: 7 },
      running_right: { row: 1, frames: 8 },
      running_left: { row: 2, frames: 8 },
      waving: { row: 3, frames: 4 },
      jumping: { row: 4, frames: 5 },
      failed: { row: 5, frames: 8 },
      waiting_permission: { row: 6, frames: 6 },
      running: { row: 7, frames: 6 },
      review: { row: 8, frames: 6, play: [0, 1, 2, 3, 0] }, // 列4坏帧不播；列5图形偏大弃用，末位以列0代替
      idle_talk: { row: 9, frames: 8 },
      idle_shy: { row: 10, frames: 8 },
    },
    idleAnims: ["idle"], // 行9/10 说话/害羞仅登记备用，暂不参与轮播
    frameMs: 160,
  },
];

export function packById(id: string): PetPack {
  return PET_PACKS.find((p) => p.id === id) ?? PET_PACKS[0];
}

/** 动画的播放帧列：显式 play 序列优先，否则 0..frames-1 */
function animCols(anim: AnimDef): number[] {
  if (anim.play?.length) return anim.play;
  return Array.from({ length: anim.frames }, (_, c) => c);
}

/** 桌宠画布：精灵动画 + 状态切换 + 头顶速度气泡 */
export class PetWidget {
  private canvas: HTMLCanvasElement;
  private pack: PetPack;
  private img: HTMLImageElement | null = null;
  private anim = "idle";
  /** 播放序号：当前动画帧列（animCols，坏帧已剔除）中的位置 */
  private seq = 0;
  private lastFrameAt = 0;
  /** 数值速度（选档用；this.tps 是气泡显示字符串） */
  private tpsNum = 0;
  /** 5/6 档高速跑的方向：true=向右(行1)/false=向左(行2)，每跑完一遍换向 */
  private fastDir = true;
  private tps = "";
  /** 上轮均速（最近一次已完成调用，落盘口径）；0 = 今日尚无已完成调用 */
  private lastTps = 0;
  /** 并发任务明细（≥2 个时气泡展开分任务行，与完整面板任务卡同口径） */
  private tasks: PetTask[] = [];
  /** 常显上轮均速（右键菜单勾选项）：气泡恒两行（生成时直接展开，无需悬停）；显隐仍随生成状态，待机不显示 */
  private alwaysLast = false;
  /** 鼠标悬停：气泡由单行实时速度变两行（实时速度 / 上轮均速） */
  private hover = false;
  /** 悬停进度 0~1（平滑过渡，同时驱动气泡尺寸与精灵让位） */
  private hoverT = 0;
  /** 气泡可见度 0~1：待机时不显示（悬停除外），淡入淡出同时驱动精灵让位 */
  private visT = 0;
  private lastDrawAt = 0;
  private running = false;
  private est = false;
  private raf = 0;
  private expandBtn: HTMLElement | null = null;
  private cycleBtn: HTMLElement | null = null;

  constructor(canvas: HTMLCanvasElement, packId: string) {
    this.canvas = canvas;
    this.pack = packById(packId);
    this.load();
    // 悬停监听挂在整块悬浮窗上（按钮是画布兄弟节点，挂画布会在移到按钮上时误判离开）
    const hoverTarget = canvas.parentElement ?? canvas;
    hoverTarget.addEventListener("mouseenter", () => {
      this.hover = true;
    });
    hoverTarget.addEventListener("mouseleave", () => {
      this.hover = false;
    });
  }

  private load() {
    this.img = null;
    // 换包后按当前状态重新起手（动画键集随包不同，如鲸鱼专属的说话/害羞行）
    this.anim = this.running ? this.runAnim() : this.pickIdle();
    this.seq = 0;
    const img = new Image();
    img.src = this.pack.sheet;
    img.onload = () => {
      this.img = img;
    };
  }

  setLive(tps: number, state: "idle" | "running" | "estimating" | "starting") {
    // 启动等待（首字节未到）显示 "…"，与表盘的统计中提示一致
    this.tps = state === "starting" ? "…" : state === "estimating" ? "≈" + tps.toFixed(1) : tps.toFixed(1);
    this.tpsNum = tps;
    this.est = state === "estimating";
    // 只有实测到流式输出（或刚启动等待中）才进入跑步组动画；估算回退时保持待机轮播。
    // 状态/档位变化一律不立即切：当前动画必整行播完，切换只在行尾生效（见 draw）
    this.running = state === "running" || state === "starting";
  }

  /** 待机随机轮播：从包的待机组里均匀随机挑一个（鲸鱼 3 选 1 含说话/害羞，月薪喵仅站立），
   *  每整行播完重新挑，可能连续抽到同一个 */
  private pickIdle(): string {
    const keys = this.pack.idleAnims;
    return keys[Math.floor(Math.random() * keys.length)];
  }

  /** 速度档位 → 跑步组动画：1 档行 7(running) / 2 档行 8(review) / 3、4 档行 4(jumping)，
   *  5、6 档用行 1、2(running_right/left) 左右来回跑 */
  private runAnim(): string {
    switch (speedTierIndex(this.tpsNum)) {
      case 0:
        return "running";
      case 1:
        return "review";
      case 2:
      case 3:
        return "jumping";
      default:
        return this.fastDir ? "running_right" : "running_left";
    }
  }

  /** 跑步组行尾切换：仍处 5/6 档且动画未变时换向跑，否则取当前档位的动画（档位变化在此生效） */
  private nextRunAnim(): string {
    const next = this.runAnim();
    if (next === this.anim && (next === "running_right" || next === "running_left")) {
      this.fastDir = !this.fastDir;
      return this.fastDir ? "running_right" : "running_left";
    }
    return next;
  }

  /** 上轮均速（最近一次已完成调用速度，落盘口径）：悬停气泡第二行用 */
  setLast(tps: number) {
    this.lastTps = isFinite(tps) && tps > 0 ? tps : 0;
  }

  /** 并发任务明细（main.ts 在 ≥2 任务时传入，否则传空数组） */
  setTasks(tasks: PetTask[]) {
    this.tasks = tasks;
  }

  /** 常显上轮均速（右键菜单勾选项，持久化由调用方处理） */
  setAlwaysLast(on: boolean) {
    this.alwaysLast = on;
  }

  /** 切换到下一只宠物（自动保存由调用方处理） */
  cyclePack(): string {
    const i = PET_PACKS.findIndex((p) => p.id === this.pack.id);
    this.pack = packById(PET_PACKS[(i + 1) % PET_PACKS.length].id);
    this.load();
    return this.pack.id;
  }

  start() {
    const loop = (now: number) => {
      this.draw(now);
      this.raf = requestAnimationFrame(loop);
    };
    this.raf = requestAnimationFrame(loop);
  }

  stop() {
    cancelAnimationFrame(this.raf);
  }

  private draw(now: number) {
    const canvas = this.canvas;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    if (w < 8 || h < 8) return;
    const dpr = window.devicePixelRatio || 1;
    const pw = Math.round(w * dpr);
    const ph = Math.round(h * dpr);
    if (canvas.width !== pw || canvas.height !== ph) {
      canvas.width = pw;
      canvas.height = ph;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    // 展开进度（平滑过渡）：0 = 单行实时速度，1 = 全行（实时 / 分任务 / 上轮均速）
    const dt = Math.min(0.1, Math.max(0, (now - this.lastDrawAt) / 1000));
    this.lastDrawAt = now;
    const multi = this.tasks.length >= 2;
    const want = this.hover || this.alwaysLast || multi ? 1 : 0;
    this.hoverT += (want - this.hoverT) * (1 - Math.exp(-dt * 12));
    if (Math.abs(want - this.hoverT) < 0.002) this.hoverT = want;
    // 气泡可见度：待机（无生成任务）时不显示气泡（悬停可查上轮）；生成/估算/启动
    // 等待时显示，悬停与多任务（≥2 进程）时强制显示。勾选"常显上轮均速"只控制
    // 展开行数（恒两行），不改变显隐——生成开始时气泡随实时速度一起淡入
    const wantVis = this.hover || this.running || this.est || multi ? 1 : 0;
    this.visT += (wantVis - this.visT) * (1 - Math.exp(-dt * 12));
    if (Math.abs(wantVis - this.visT) < 0.002) this.visT = wantVis;

    const anim = this.pack.anims[this.anim] ?? this.pack.anims.idle;
    let cols = animCols(anim);
    if (now - this.lastFrameAt >= this.pack.frameMs) {
      this.lastFrameAt = now;
      this.seq++;
      if (this.seq >= cols.length) {
        // 整行播完一轮才能切下一个动画：待机每轮随机换一个；
        // 待机↔跑步切换与跑步组档位变化同样只在此行尾生效
        this.seq = 0;
        this.anim = this.running ? this.nextRunAnim() : this.pickIdle();
        cols = animCols(this.pack.anims[this.anim] ?? this.pack.anims.idle);
      }
    }

    // 精灵几何（纯包常量，与图片是否加载无关）：窗口 = 底部正方形精灵区 + 顶部
    // 气泡预留带（main.rs 的 PET_BUBBLE_RESERVE），精灵恒按正方形区缩放、不随
    // 气泡行数缩小；浏览器预览窗口无此比例时退回短边正方形
    const sq = Math.min(w, h);
    const availH = Math.max(this.pack.cellH * 0.15, sq * 0.92);
    const scale = Math.min((w * 0.94) / this.pack.cellW, availH / this.pack.cellH);
    const dw = this.pack.cellW * scale;
    const dh = this.pack.cellH * scale;
    const dx = (w - dw) / 2;
    // 气泡底边锚在精灵头顶附近（约 9% 精灵高处）：单行/两行切换底边不动、向上生长
    const bubble = this.layoutBubble(ctx, w, h - 2 - dh * 0.91);

    const img = this.img;
    if (img) {
      const sx = cols[this.seq] * this.pack.cellW;
      const sy = anim.row * this.pack.cellH;
      // 底部贴边居中：精灵图单元自带透明边距，放大并紧贴下缘，避免脚下留大片空白
      ctx.drawImage(img, sx, sy, this.pack.cellW, this.pack.cellH, dx, h - dh - 2, dw, dh);

      // 按钮贴到精灵脚部右侧（跟随实际绘制宽度）
      if (!this.expandBtn) this.expandBtn = document.getElementById("float-pet-expand");
      if (!this.cycleBtn) this.cycleBtn = document.getElementById("float-pet-cycle");
      const rightGap = Math.max(4, w - (dx + dw) + 2);
      if (this.expandBtn) this.expandBtn.style.right = `${rightGap}px`;
      if (this.cycleBtn) this.cycleBtn.style.right = `${rightGap + 26}px`;
    }

    // 待机且未悬停：整块不画（连尾巴也不留）
    if (bubble.vis > 0.01) this.paintBubble(ctx, w, bubble);
  }

  /** 气泡排版：行 = 实时速度 +（多任务时每进程一行）+ 上轮均速，随展开进度
   *  在"单行实时速度"与"全行带标签"之间插值。底边锚点固定（精灵头顶附近），
   *  行数增多向上生长；高度放不下时丢任务行（聚合值仍在第一行），宽度放不下
   *  时缩字号（桌宠窗口可缩到 100px，浏览器预览无多任务加高同样适配） */
  private layoutBubble(ctx: CanvasRenderingContext2D, w: number, bottom: number): BubbleLayout {
    const padX = 11;
    // 内边距/行高取到与旧版单行气泡等高（12 + 15×1.2 ≈ 原 26px），
    // 否则单行状态也会平白多压住精灵一点
    const padY = 5;
    const colGap = 8;
    const gapY = 3;
    const t = this.hoverT;
    const liveColor = this.est ? "#fbbf24" : this.running ? "#22d3ee" : "#8b93a7";
    const rows: Array<{ label: string; value: string; color: string }> = [
      { label: "实时速度", value: `${this.tps} t/s`, color: liveColor },
    ];
    for (const task of this.tasks.slice(0, MAX_TASK_ROWS)) {
      rows.push({
        label: task.label,
        value: task.streaming ? `${fmtTps(task.tps)} t/s` : "待机",
        color: task.streaming ? speedColor(task.tps, SPEED_TIERS) : "#8b93a7",
      });
    }
    rows.push({
      label: "上轮均速",
      value: this.lastTps > 0 ? `${fmtTps(this.lastTps)} t/s` : "--",
      color: speedColor(this.lastTps, SPEED_TIERS),
    });

    let fs = Math.max(11, Math.min(15, w * 0.075));
    let labelFs = Math.max(8, fs * 0.78);
    let lineH = fs * 1.2;
    // 高度适配：气泡底边锚定、向上生长，可用高度 = 底边锚点 − 顶边距；
    // 放不下时从后往前丢任务行（末行是上轮均速，保底实时/上轮两行信息）
    const boxHFor = (n: number) => padY * 2 + lineH * n + gapY * (n - 1);
    const availH = Math.max(padY * 2 + lineH, bottom - 2);
    while (rows.length > 2 && boxHFor(rows.length) > availH) {
      rows.splice(rows.length - 2, 1);
    }

    let labelW = 0;
    let valueW = 0;
    let valueW0 = 0;
    const maxBoxW = Math.max(40, w - 6);
    // 过渡中按展开宽度排（否则字会先溢出再收缩）
    const expanded = t > 0.01 || this.hover || this.alwaysLast || this.tasks.length >= 2;
    for (;;) {
      ctx.font = `600 ${fs}px ${FONT}`;
      valueW0 = ctx.measureText(rows[0].value).width;
      valueW = expanded ? Math.max(...rows.map((r) => ctx.measureText(r.value).width)) : valueW0;
      ctx.font = `500 ${labelFs}px ${FONT}`;
      labelW = expanded ? Math.max(...rows.map((r) => ctx.measureText(r.label).width)) : 0;
      if (padX * 2 + labelW + colGap + valueW <= maxBoxW || fs <= 8) break;
      fs -= 1;
      labelFs = Math.max(8, fs * 0.78);
      lineH = fs * 1.2;
    }

    // 收拢宽 = 单行实时速度所需；展开宽 = 标签列 + 最宽值
    const boxW1 = padX * 2 + valueW0;
    const boxW2 = padX * 2 + labelW + colGap + valueW;
    const boxH1 = padY * 2 + lineH;
    const boxH2 = boxHFor(rows.length);
    const vis = this.visT;
    return {
      rows,
      t,
      vis,
      fs,
      labelFs,
      lineH,
      gapY,
      padX,
      padY,
      colGap,
      labelW: labelW * t,
      boxW: boxW1 + (boxW2 - boxW1) * t,
      boxH: boxH1 + (boxH2 - boxH1) * t,
      bottom,
    };
  }

  private paintBubble(ctx: CanvasRenderingContext2D, w: number, b: BubbleLayout) {
    const bx = w / 2 - b.boxW / 2;
    // 底边锚定不动、向上生长；预留带不够时（浏览器预览等）退回顶到画布顶
    const by = Math.max(2, b.bottom - b.boxH);
    const bg = "rgba(13,20,36,0.88)";

    // 底板与尾巴随可见度淡入淡出（待机时整块消失）
    ctx.globalAlpha = b.vis;
    ctx.fillStyle = bg;
    ctx.strokeStyle = this.est
      ? "rgba(251,191,36,0.75)"
      : this.running
        ? "rgba(34,211,238,0.75)"
        : "rgba(255,255,255,0.22)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.roundRect(bx, by, b.boxW, b.boxH, Math.min(13, b.boxH / 2));
    ctx.fill();
    ctx.stroke();
    // 气泡小尾巴
    ctx.beginPath();
    ctx.moveTo(w / 2 - 5, by + b.boxH - 1);
    ctx.lineTo(w / 2 + 5, by + b.boxH - 1);
    ctx.lineTo(w / 2, by + b.boxH + 7);
    ctx.closePath();
    ctx.fillStyle = bg;
    ctx.fill();

    // 按气泡当前高度裁剪：过渡时第二行随气泡长高而露出，不会先画到框外
    ctx.save();
    ctx.beginPath();
    ctx.rect(bx, by, b.boxW, b.boxH);
    ctx.clip();
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    b.rows.forEach((r, i) => {
      if (i > 0 && b.t <= 0.01) return;
      const cy = by + b.padY + b.lineH * (i + 0.5) + b.gapY * i;
      let x = bx + b.padX;
      if (r.label && b.t > 0.01) {
        ctx.globalAlpha = b.t * b.vis;
        ctx.font = `500 ${b.labelFs}px ${FONT}`;
        ctx.fillStyle = "#8b93a7";
        ctx.fillText(r.label, x, cy);
        x += b.labelW + b.colGap;
      }
      ctx.globalAlpha = (i > 0 ? b.t : 1) * b.vis;
      ctx.font = `600 ${b.fs}px ${FONT}`;
      ctx.fillStyle = r.color;
      ctx.fillText(r.value, x, cy);
    });
    ctx.restore();
    ctx.globalAlpha = 1;
  }
}
