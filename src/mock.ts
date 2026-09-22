// 浏览器预览模式：模拟 ZCode 的 model-io 调用流，便于无 Tauri 环境下预览 UI

import type { GuardStatus } from "./guard";
import type { ModelStatsPayload } from "./model_stats";

/** 分任务实时明细（多任务并发时才有多个）：一个 CLI 进程 = 一行 */
export interface TaskStat {
  pid: number;
  /** 归属的进行中会话 id（空 = 尚未归属的流式进程） */
  session: string;
  /** 该进程承载的进行中会话数（≥2 = 同进程多任务，速度为合计） */
  nSessions: number;
  tps: number;
  streaming: boolean;
}

/** 快照上传记录行（与后端 CkptStat 同形） */
export interface CkptStat {
  workspace: string;
  bytes: number;
  recordedMs: number;
  accepted: boolean;
  uploading: boolean;
  /** checkpoints 下的工作区子目录名（点 📂 打开该目录）；留档旧行无此字段 */
  hash?: string;
}

/** ZCode 连接明细行（与后端 ConnStat 同形）：两组均为 ZCode 自身进程 */
export interface ConnStat {
  remote: string;
  pid: number;
  /** 进程类型标签：CLI 会话进程 / 主进程 / 渲染进程 / GPU 进程 / 工具进程 / 崩溃报告进程 */
  proc: string;
}

export interface Snapshot {
  currentTps: number;
  avgTps: number;
  totalTokens: number;
  outputTokens: number;
  inputTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
  callsToday: number;
  sessionsToday: number;
  isLive: boolean;
  isEstimating: boolean;
  /** 调用已开始但首字节未到（TTFT）：显示"统计中…"提示而非估算值 */
  isStarting: boolean;
  /** 实测流式已开始但 30s 滑窗未填满（显示"统计中"） */
  ramping: boolean;
  /** 近 10 分钟已完成调用的真实速度（落盘口径） */
  windowTps: number;
  /** 最近一次已完成调用的真实速度（落盘口径），当前速度卡右上角小表用 */
  lastCallTps: number;
  /** 近 7 天最高单调用速度（窗口与准入口径见 metrics.rs；mock 给个合理峰值） */
  histMaxTps: number;
  /** 近 7 天平均速度（窗口内调用 Σeff ÷ Σgen，与今日平均同口径） */
  histAvgTps: number;
  liveSource: string;
  lastActivityMs: number;
  nowMs: number;
  rolloutDir: string;
  spark: number[];
  /** 并发任务分进程明细（≥2 个时前端显示任务列表） */
  tasks: TaskStat[];
  // ---- 网络流量监控（netio.rs；浏览器预览为模拟值） ----
  netAvailable: boolean;
  netUpBps: number;
  netDownBps: number;
  netUpToday: number;
  netDownToday: number;
  netSessUpToday: number;
  netSessDownToday: number;
  netCkptToday: number;
  netCkptTodayCount: number;
  /** 当日已接受工件名单（时间/工作区/大小） */
  netCkptTodayList: CkptStat[];
  netCkptUploading: boolean;
  netCkptStatus: string;
  /** 快照上传记录（每工作区最近一次工件实况） */
  netCkptList: CkptStat[];
  netConnsAvailable: boolean;
  netCliConns: number;
  netAppConns: number;
  /** 连接明细（每条含远端 + 归属 pid + 进程类型标签） */
  netCliConnList: ConnStat[];
  netAppConnList: ConnStat[];
  /** 快照防护状态（snapshot_guard.rs；mock/浏览器预览无此字段 → 按未防护渲染） */
  guard?: GuardStatus;
}

interface MockCall {
  completed: number;
  duration: number;
  output: number;
  input: number;
  cache: number;
  session: string;
  /** 管道静默调用：整段无增量字节，走 ≈ 估算显示 */
  silent: boolean;
  /** 并发任务期间第二个进程的固定速度（0 = 单任务；按调用固定，不逐拍重抽） */
  second: number;
  /** 模型名（模型详情视图按此分组；与真实库的 model_id 同角色） */
  model: string;
}

const MIN_DUR = 50;
const WINDOW = 10 * 60 * 1000;
const BUCKETS = 90;
const BUCKET = 10_000;

const rnd = (a: number, b: number) => a + Math.random() * (b - a);

/** 模拟模型池：主模型高频，两个次模型低频（模型详情视图的多条折线演示） */
const MODEL_POOL = ["claude-sonnet-4-5", "claude-sonnet-4-5", "glm-4.6", "deepseek-v3.2"];

let calls: MockCall[] = [];
let sessionNo = 1;
// 网络监控模拟状态：当日累计单调累加；偶发一段"快照上传"演示警示行
let netUpToday = rnd(2e8, 6e8);
let netDownToday = rnd(1e9, 4e9);
let mockCkptUploading = false;
let ckptNextToggle = Date.now() + rnd(15_000, 40_000);

function newCall(now: number): MockCall {
  if (Math.random() < 0.18) sessionNo++;
  const duration = Math.exp(rnd(Math.log(12000), Math.log(180000)));
  const tps = rnd(18, 70);
  const output = Math.max(60, Math.round((duration / 1000) * tps));
  // usage 库语义:cache_read 是 input 的子集,命中率常态 90%+
  const input = Math.round(rnd(15000, 60000));
  return {
    completed: now + duration,
    duration,
    output,
    input,
    cache: Math.round(input * rnd(0.8, 0.99)),
    session: `mock-sess-${sessionNo}`,
    silent: Math.random() < 0.22,
    second: Math.random() < 0.3 ? rnd(15, 90) : 0,
    model: MODEL_POOL[Math.floor(Math.random() * MODEL_POOL.length)],
  };
}

function seedHistory(now: number) {
  let t = now - 3 * 3600 * 1000;
  while (t < now) {
    if (Math.random() < 0.72) {
      const burst = Math.round(rnd(3, 14));
      for (let i = 0; i < burst && t < now; i++) {
        const c = newCall(t);
        if (c.completed > now) break;
        calls.push(c);
        t = c.completed + rnd(300, 2500);
      }
      t += rnd(20000, 240000);
    } else {
      t += rnd(60000, 300000);
    }
  }
  sessionNo = Math.max(sessionNo, 6);
}

function snapshot(now: number, pending: MockCall | null): Snapshot {
  let out = 0,
    input = 0,
    cache = 0,
    dur = 0,
    wOut = 0,
    wDur = 0,
    last = 0,
    lastTps = 0;
  const sessions = new Set<string>();
  const buckets = new Array<number>(BUCKETS).fill(0);
  const bucketDur = new Array<number>(BUCKETS).fill(0);
  const nowSlot = Math.floor(now / BUCKET);
  for (const c of calls) {
    const d = Math.max(MIN_DUR, c.duration);
    out += c.output;
    input += c.input;
    cache += c.cache;
    dur += d;
    if (c.completed >= last) {
      last = c.completed;
      lastTps = c.output / (d / 1000); // 与后端一致：取完成时刻最晚一条的 eff ÷ 纯生成时长
    }
    sessions.add(c.session);
    if (c.completed >= now - WINDOW) {
      wOut += c.output;
      wDur += d;
    }
    const slot = nowSlot - Math.floor(c.completed / BUCKET);
    if (slot >= 0 && slot < BUCKETS) {
      buckets[BUCKETS - 1 - slot] += c.output;
      bucketDur[BUCKETS - 1 - slot] += d;
    }
  }
  // 门控模型与后端一致：以进行中的调用（pending）为准。
  // 模拟 TTFT ~2.5s：启动期显示"统计中…"；约 1/5 的调用为管道静默（整段 ≈ 估算）
  const pendingStart = pending ? pending.completed - pending.duration : 0;
  const ageSec = pending ? (now - pendingStart) / 1000 : Infinity;
  const isStarting = !!pending && ageSec < 2.5 && !pending.silent;
  const isLive = !!pending && ageSec >= 2.5 && !pending.silent;
  const isEstimating = !!pending && pending.silent && wDur > 0;
  const currentTps = isStarting
    ? 0
    : isLive || isEstimating
      ? wDur > 0
        ? wOut / (wDur / 1000)
        : 0
      : 0;
  // 模拟并发任务：部分调用期间另有第二个 CLI 进程在流式——当前速度为聚合
  // 总吞吐，分任务列表显示两行（预览多任务 UI；第二任务速度按调用固定）
  const ownTps = currentTps;
  const secondTps = pending && pending.second > 0 && isLive ? pending.second : 0;
  const currentAgg = currentTps + secondTps;
  const spark = buckets.map((o, i) => (bucketDur[i] > 0 ? o / (bucketDur[i] / 1000) : 0));
  if (isEstimating && spark[BUCKETS - 1] <= 0) {
    spark[BUCKETS - 1] = currentTps;
  }
  const tasks: TaskStat[] = [];
  if (pending && (isLive || isStarting)) {
    tasks.push({
      pid: 4000 + sessionNo,
      session: pending.session,
      nSessions: 1,
      tps: ownTps,
      streaming: isLive,
    });
    if (secondTps > 0) {
      tasks.push({
        pid: 7000 + sessionNo,
        session: `mock-sess-${sessionNo + 1}`,
        nSessions: 1,
        tps: secondTps,
        streaming: true,
      });
    }
  }
  return {
    currentTps: currentAgg,
    avgTps: dur > 0 ? out / (dur / 1000) : 0,
    totalTokens: out + input,
    outputTokens: out,
    inputTokens: input,
    cacheCreationTokens: 0,
    cacheReadTokens: cache,
    callsToday: calls.length,
    sessionsToday: sessions.size,
    isLive,
    isEstimating,
    isStarting,
    ramping: isLive && ageSec < 30,
    windowTps: wDur > 0 ? wOut / (wDur / 1000) : 0,
    lastCallTps: lastTps,
    // 近 7 天统计 mock：峰值为今日峰值 × 1.2、7 天平均略低于今日平均（多日稀释）
    histMaxTps: Math.max(...spark, lastTps) * 1.2 || 312,
    histAvgTps: dur > 0 ? (out / (dur / 1000)) * 0.92 : 0,
    liveSource: isStarting || isLive ? "io" : isEstimating ? "window" : "idle",
    lastActivityMs: last,
    nowMs: now,
    rolloutDir: "（浏览器预览 · 模拟数据）",
    spark,
    tasks,
    // 网络监控模拟：速度随调用活动起伏，当日累计单调累加
    netAvailable: true,
    netUpBps: isLive ? rnd(20_000, 90_000) : rnd(0, 3_000),
    netDownBps: isLive ? rnd(80_000, 400_000) : rnd(0, 8_000),
    netUpToday: netUpToday,
    netDownToday: netDownToday,
    // 与后端同口径：上传按未缓存提示（input−cache_read）×5，下载按输出 ×400
    netSessUpToday: Math.max(0, input - cache) * 5,
    netSessDownToday: out * 400,
    // 与实机同款：今日 3 个工件（1GB 大件 + 两个 KB 级小件）
    netCkptToday: 1024.0 * 1048576 + 990 + 1013,
    netCkptTodayCount: 3,
    netCkptTodayList: [
      { workspace: "GenePad-free", bytes: 1024.0 * 1048576, recordedMs: now - 7 * 3600_000, accepted: true, uploading: false },
      { workspace: "default", bytes: 990, recordedMs: now - 16 * 3600_000, accepted: true, uploading: false },
      { workspace: "zcode-speed-panel", bytes: 1013, recordedMs: now - 5 * 3600_000, accepted: true, uploading: false },
    ],
    netCkptUploading: mockCkptUploading,
    netCkptStatus: "ok",
    // 快照上传记录模拟：上传中 > 待传 > 已接受（含跨天记录演示月日显示），
    // 共 9 行演示"固定显示 5 行、其余滚动"
    netCkptList: [
      { workspace: "GenePad", bytes: 549.2 * 1048576, recordedMs: now - 3600_000, accepted: !mockCkptUploading, uploading: mockCkptUploading },
      { workspace: "GenePad-free", bytes: 1024.0 * 1048576, recordedMs: now - 7 * 3600_000, accepted: true, uploading: false },
      { workspace: "zcode-speed-panel", bytes: 990, recordedMs: now - 4 * 3600_000, accepted: true, uploading: false },
      { workspace: "Gene_Editor-master", bytes: 522.5 * 1048576, recordedMs: now - 13 * 86400_000, accepted: true, uploading: false },
      { workspace: "notes-sync", bytes: 18.4 * 1048576, recordedMs: now - 2 * 86400_000, accepted: true, uploading: false },
      { workspace: "dotfiles", bytes: 2048, recordedMs: now - 3 * 86400_000, accepted: true, uploading: false },
      { workspace: "blog-hugo", bytes: 96.7 * 1048576, recordedMs: now - 4 * 86400_000, accepted: true, uploading: false },
      { workspace: "ml-bench", bytes: 733.0 * 1048576, recordedMs: now - 6 * 86400_000, accepted: true, uploading: false },
      { workspace: "scrape-tools", bytes: 4400, recordedMs: now - 8 * 86400_000, accepted: true, uploading: false },
    ],
    netConnsAvailable: true,
    netCliConns: isLive ? 2 : 1,
    netAppConns: mockCkptUploading ? 3 : 1,
    // 连接明细模拟：两组都是 ZCode 自身进程（CLI / Electron 壳），按进程标注
    netCliConnList: [
      { remote: "61.170.79.24:443", pid: 41092, proc: "CLI 会话进程" },
      { remote: "61.170.79.31:443", pid: 41092, proc: "CLI 会话进程" },
    ],
    netAppConnList: mockCkptUploading
      ? [
          { remote: "61.151.230.245:443", pid: 18104, proc: "主进程" },
          { remote: "oss-cn-hangzhou.aliyuncs.com:443", pid: 18104, proc: "主进程" },
          { remote: "oss-cn-hangzhou.aliyuncs.com:443", pid: 18220, proc: "工具进程" },
        ]
      : [{ remote: "61.151.230.245:443", pid: 18104, proc: "主进程" }],
  };
}

/** 模型详情视图的模拟数据：把调用流按模型 × 绝对墙钟槽聚合（与后端
 *  aggregate_model_stats 同口径：slot = now÷bucketMs − completed÷bucketMs，
 *  90 桶、四档窗口与曲线共用、越界丢弃、桶 tps = Σeff ÷ Σgen_s），供无 Tauri 预览 */
export function mockModelStats(windowMin: number): ModelStatsPayload {
  const win = [15, 60, 360, 1440].reduce((a, b) => (Math.abs(b - windowMin) < Math.abs(a - windowMin) ? b : a));
  const now = Date.now();
  const bucketMs = (win * 60_000) / 90;
  const cutoff = now - win * 60_000;
  interface Acc {
    eff: number;
    gen: number;
    calls: number;
  }
  const per = new Map<string, { slots: Acc[]; total: Acc }>();
  for (const c of calls) {
    if (c.completed < cutoff || c.completed > now) continue;
    const gen = Math.max(MIN_DUR, c.duration);
    const slot = Math.floor(now / bucketMs) - Math.floor(c.completed / bucketMs);
    if (slot < 0 || slot >= 90) continue;
    let e = per.get(c.model);
    if (!e) {
      e = { slots: Array.from({ length: 90 }, () => ({ eff: 0, gen: 0, calls: 0 })), total: { eff: 0, gen: 0, calls: 0 } };
      per.set(c.model, e);
    }
    const b = e.slots[slot];
    b.eff += c.output;
    b.gen += gen;
    b.calls += 1;
    e.total.eff += c.output;
    e.total.gen += gen;
    e.total.calls += 1;
  }
  const grandEff = [...per.values()].reduce((t, e) => t + e.total.eff, 0);
  const tpsOf = (b: Acc) => (b.gen > 0 ? b.eff / (b.gen / 1000) : 0);
  const series = [...per.entries()]
    .sort((a, b) => b[1].total.eff - a[1].total.eff || a[0].localeCompare(b[0]))
    .map(([model, e]) => ({
      model,
      buckets: e.slots.map((b) => ({ tps: tpsOf(b), calls: b.calls, tokens: b.eff })),
      totalCalls: e.total.calls,
      totalTokens: e.total.eff,
      avgTps: e.total.gen > 0 ? e.total.eff / (e.total.gen / 1000) : 0,
      peakTps: Math.max(0, ...e.slots.map(tpsOf)),
      share: grandEff > 0 ? e.total.eff / grandEff : 0,
    }));
  return { windowMin: win, bucketMs, nowMs: now, series };
}

export function startMock(onData: (s: Snapshot) => void) {
  const now = Date.now();
  seedHistory(now);
  let pending: MockCall | null = null;
  let nextStart = now + rnd(1000, 4000);

  const tick = () => {
    const t = Date.now();
    if (pending && t >= pending.completed) {
      calls.push(pending);
      // 只保留最近 30 分钟
      const cutoff = t - 30 * 60 * 1000;
      calls = calls.filter((c) => c.completed >= cutoff);
      pending = null;
      nextStart = t + rnd(200, 2500);
    }
    if (!pending && t >= nextStart) {
      pending = newCall(t);
    }
    // 网络监控模拟：累计按模拟速度推进；快照上传段偶发启停
    netUpToday += rnd(500, 120_000) * 0.4;
    netDownToday += rnd(2_000, 500_000) * 0.4;
    if (t >= ckptNextToggle) {
      mockCkptUploading = !mockCkptUploading;
      ckptNextToggle = t + (mockCkptUploading ? rnd(20_000, 50_000) : rnd(40_000, 120_000));
    }
    onData(snapshot(t, pending));
  };

  tick();
  setInterval(tick, 400);
}
