// 快照防护与上传记录卡（完整面板，网络监控卡下方）的防护控制区：状态徽标 +
// 已积累工件统计 + 开启/解除按钮（自绘确认弹窗，明示损失「检查点回滚 /
// 时间线」——用户要求的知情同意，key-rules #16）。卡片里的今日快照上传与
// 记录列表由 main.ts 的 renderSnapshot 渲染（快照相关的一切都在这张卡）。
// 后端 snapshot_guard.rs 用目录写入锁（macOS chflags 不可变标志 /
// Windows ACL 拒绝创建/写入）阻断 ZCode 工作区快照落盘上传：不碰网络、
// 不影响模型对话/补全/工具调用；状态随 metrics payload 的 guard 字段每拍
// 推送（src/main.ts 调 renderGuard；无该字段时控制区隐藏，只看记录）。
import { fmtBytes } from "./gauges";
import type { CkptStat } from "./mock";

/** metrics payload 附带的防护状态（src-tauri/src/snapshot_guard.rs，camelCase） */
export interface GuardStatus {
  supported: boolean;
  locked: boolean;
  lockedSinceMs: number | null;
  blockedRounds: number;
  artifactCount: number;
  artifactBytes: number;
  workspaceCount: number;
  failureCount: number;
  /** 防护前的原上传记录留档（apply 清空前保存；防护期间完整回看） */
  history: CkptStat[];
}

type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T | undefined>;

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

/** initGuard 后填充的元素引用（renderGuard 在 metrics 事件里高频调用） */
interface GuardEls {
  card: HTMLElement;
  scope: HTMLElement;
  badge: HTMLElement;
  applyBtn: HTMLButtonElement;
  releaseBtn: HTMLButtonElement;
  msg: HTMLElement;
  stats: HTMLElement;
  rounds: HTMLElement;
}

let els: GuardEls | null = null;
let latest: GuardStatus | null = null;

/** 渲染防护状态（main.ts 的 metrics 监听逐拍调用；null = 无后端/mock，卡片保持隐藏） */
export function renderGuard(g: GuardStatus | null): void {
  if (!g || !els) return;
  latest = g;
  const e = els;
  e.card.hidden = false;
  e.badge.textContent = g.locked ? "🔒 已防护" : "🔓 未防护";
  e.badge.classList.toggle("on", g.locked);
  // 按钮互斥显示（开启 ↔ 解除）；平台不支持则禁用并如实标注
  e.applyBtn.hidden = g.locked;
  e.releaseBtn.hidden = !g.locked;
  e.applyBtn.disabled = !g.supported;
  e.releaseBtn.disabled = !g.supported;
  if (!g.supported) {
    e.scope.textContent = "文件锁仅支持 macOS / Windows";
  }
  // 状态三态（术语用平实词，不用内部黑话）：
  // 未防护 = 实时扫描统计；防护中·快照已保留 = 递归锁、文件只读留原地；
  // 防护中·快照已删除 = 空目录锁定，仅剩留档清单可回看
  e.stats.textContent = g.locked
    ? g.artifactCount > 0
      ? `防护生效中（快照已保留）：${g.artifactCount} 个加密快照锁定在本地只读（共 ${fmtBytes(g.artifactBytes)}）· ZCode 写不进新快照；记录仍可看、可点 📂 打开`
      : `防护生效中（快照已删除）：目录已清空并锁定，ZCode 写不进新快照；防护前的上传记录在下方列表完整保留（${g.history.length} 条）`
    : g.artifactCount > 0
      ? `本地已积累加密快照 ${g.artifactCount} 个 · 共 ${fmtBytes(g.artifactBytes)} · ` +
        `覆盖 ${g.workspaceCount} 个项目 · ZCode 记录上传失败 ${g.failureCount} 次`
      : `本地未发现 ZCode 快照（checkpoints 目录为空，可能从未生成或已被清理）`;
  // 防护后追加行：开启以来的对话轮次（锁定期间目录不可写，新快照恒为 0）
  if (g.locked) {
    e.rounds.hidden = false;
    e.rounds.textContent = `防护开启后 ${g.blockedRounds} 轮对话 · 新快照落盘 0 个`;
  } else {
    e.rounds.hidden = true;
  }
}

/** 绑定卡片与确认弹窗交互；确认后的执行结果由 renderGuard 即时刷新 */
export function initGuard(invoke: InvokeFn): void {
  els = {
    card: $("guard-card"),
    scope: $("guard-scope"),
    badge: $("guard-badge"),
    applyBtn: $<HTMLButtonElement>("guard-apply"),
    releaseBtn: $<HTMLButtonElement>("guard-release"),
    msg: $("guard-msg"),
    stats: $("guard-stats"),
    rounds: $("guard-rounds"),
  };
  const modal = $("guard-confirm");
  const box = $("guard-confirm-box");
  const title = $("guard-confirm-title");
  const text = $("guard-confirm-text");
  const okBtn = $<HTMLButtonElement>("guard-confirm-ok");
  const keepBtn = $<HTMLButtonElement>("guard-confirm-keep");

  /** 弹窗当前待执行的动作（null = 关闭态） */
  let pendingAction: "apply" | "release" | null = null;
  let busy = false;
  let msgTimer = 0;

  const flashMsg = (s: string, err = false) => {
    els!.msg.textContent = s;
    els!.msg.classList.toggle("error", err);
    window.clearTimeout(msgTimer);
    msgTimer = window.setTimeout(() => {
      els!.msg.textContent = "";
      els!.msg.classList.remove("error");
    }, 4000);
  };

  /** **加粗** 标记转 <b>（文案固定来自下方字面量，无注入面） */
  const appendRich = (p: HTMLParagraphElement, raw: string) => {
    raw.split("**").forEach((seg, i) => {
      const el = document.createElement(i % 2 ? "b" : "span");
      el.textContent = seg;
      p.append(el);
    });
  };

  const closeConfirm = () => {
    pendingAction = null;
    modal.style.display = "none";
  };

  /** 执行防护开启/解除；apply 带 keepFiles（保留模式递归锁，删除模式清空后锁） */
  const runAction = (cmd: string, args: Record<string, unknown>, okMsg: string, btn: HTMLButtonElement) => {
    if (busy) return;
    busy = true;
    btn.disabled = true;
    invoke<GuardStatus>(cmd, args)
      .then((st) => {
        if (st) renderGuard(st);
        flashMsg(okMsg);
        closeConfirm();
      })
      .catch((err) => flashMsg(`执行失败：${err}`, true))
      .finally(() => {
        busy = false;
        btn.disabled = false;
      });
  };

  const openConfirm = (action: "apply" | "release") => {
    if (!latest) return;
    pendingAction = action;
    text.replaceChildren();
    if (action === "apply") {
      // 双模式确认：保留（推荐，快照只读留原地）与删除（必须明示原始记录
      // 消失 + 仅备份清单——key-rules #16 知情同意）。共同要点两条在前，
      // 模式差异各自标明
      title.textContent = "开启快照防护？";
      keepBtn.hidden = false;
      okBtn.hidden = false;
      okBtn.textContent = "删除快照并锁定";
      okBtn.classList.add("danger-btn");
      const lines = [
        // 共同
        "开启后将损失 **「检查点回滚 / 时间线」功能**——无法再回滚到历史检查点",
        "模型对话、代码补全、工具调用**不受任何影响**",
        // 保留模式
        `**「保留并锁定」**：本地已积累的 ${latest.artifactCount} 个加密快照（共 ${fmtBytes(latest.artifactBytes)}）**原地保留（只读）**，上传记录仍完整可看、可点 📂 打开快照目录`,
        // 删除模式（用户逐条要求的知情同意）
        `**「删除并锁定」**：删除全部快照——**原始上传记录会随之消失**；删除前自动备份记录清单（时间 / 工作区 / 加密后大小 / 状态），防护期间可在快照上传记录列表回看，**只备份清单**，快照文件等明细删除后无法恢复`,
        // 共同
        "随时可解除防护（保留的快照原地恢复，空目录由 ZCode 自动重建）",
      ];
      for (const raw of lines) {
        const p = document.createElement("p");
        appendRich(p, raw);
        text.append(p);
      }
    } else {
      title.textContent = "解除快照防护？";
      keepBtn.hidden = true;
      okBtn.hidden = false;
      okBtn.textContent = "确认解除";
      okBtn.classList.remove("danger-btn");
      const p = document.createElement("p");
      p.textContent = "解除后 ZCode 将恢复快照捕获与上传（再次开启可随时阻断；保留的快照原地恢复可写）。";
      text.append(p);
    }
    modal.style.display = "flex";
  };

  els.applyBtn.addEventListener("click", () => openConfirm("apply"));
  els.releaseBtn.addEventListener("click", () => openConfirm("release"));
  keepBtn.addEventListener("click", () => {
    if (pendingAction !== "apply") return;
    runAction("snapshot_guard_apply", { keepFiles: true }, "已开启防护（快照已保留锁定）✓", keepBtn);
  });
  okBtn.addEventListener("click", () => {
    const action = pendingAction;
    if (!action) return;
    if (action === "apply") {
      runAction("snapshot_guard_apply", { keepFiles: false }, "已开启防护（快照已删除）✓", okBtn);
    } else {
      runAction("snapshot_guard_release", {}, "已解除防护 ✓", okBtn);
    }
  });
  $("guard-confirm-cancel").addEventListener("click", closeConfirm);
  $("guard-confirm-close").addEventListener("click", closeConfirm);
  // 点弹窗内容之外关闭（与设置弹窗同款交互）
  window.addEventListener("mousedown", (e) => {
    if (modal.style.display !== "flex") return;
    if (box.contains(e.target as Node)) return;
    closeConfirm();
  });
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && modal.style.display === "flex") closeConfirm();
  });
}
