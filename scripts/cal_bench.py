#!/usr/bin/env python3
"""字节→token 校准系数估计器 基准重放（真实数据）。

数据源：~/.zcode/speed-panel-debug.jsonl（含轮转 .1/.2…，按时间升序合并），
提取 kind=cal 且未跳过的校准样本序列（bpt_sample = 清洗流积分字节 ÷ 真实
output+reasoning tokens），按完成顺序重放各估计器，度量「预测下一个调用的
样本」的相对误差——误差 = |生效系数 − 该调用真实样本| ÷ 真实样本，等价于
实时读数在该调用期间的系数误差。

估计器：
  prior   恒用平台先验（下界基线）
  median5 旧实现：最近 5 样本（含先验占位）普通中位数
  wmed10  上代实现：最近 10 样本的新近加权中位数，两遍法修剪 [0.4, 2.5]，
          半衰期 3 样本，窗口不足 3 个向先验线性收缩
  wmed16  现行实现（liveio::cal_estimate）：wmed10 基础上窗口 10→16，
          并向最新样本做一步对数收缩（AR1，ρ=0.3，比值钳 [0.5, 2]——
          单样本影响 ≤ ×1.23，离群只拉动一拍、突变首样本先手）

用法：
  python scripts/cal_bench.py                  # 分析默认日志（当前+轮转）
  python scripts/cal_bench.py <日志.jsonl>      # 分析指定日志
  python scripts/cal_bench.py --models         # 追加：联库按模型分组（信息性）
  python scripts/cal_bench.py --corr           # 追加：样本自相关（新近加权的依据）

结论基线（2026-09-24 二轮定标，本机 262 个真实样本）：
  prior   med=25.9%
  median5 med=24.3% 均值=37.8%
  wmed10  med=21.3% p75=47.8% 均值=37.1%
  wmed16  med=20.7% p75=41.5% 均值=34.1%（分块验证 ρ 外推稳定：测试块
          p75 51.9→44.3、均值 37.5→33.7、med 持平）
样本 B/token 逐调用天然波动（p25~p75=466~740，log 空间 lag-1 自相关 ≈0.50、
lag-2 ≈ 0.27 ≈ ρ² 纯 AR(1)）决定误差地板 ~20% 中位数——实时读数偏差两三成
属正常波动，不要据此贸然调参。改估计器必须过基准：真实重放本脚本（改动前后
对表，不得回退）+ liveio 测试 `benchmark_*`（稳态 AR(1)/突变收敛/离群有界）。
已重放否决的方案（勿轻率重试）：
  分模型校准（两轮验证：GLM-5.3 与 Flash 分布一致；deepseek-v4-flash 仅
    3 样本 med≈221，量级差由速度而非模型驱动，样本太薄撑不起独立队列）；
  字节 ≈ a×token + c×时长的两参数模型（bpt 与真实速度负相关 r≈-0.3~0.5
    来自渲染分批，利用它需要未知的当调用速度；逐调用预测误差 med 48%~90%）；
  EWMA / 全局收缩 / 更大均窗（半衰期 6）/ eff 或时长加权——无收益或更差；
  AR1「界外样本跳过收缩」——对比「钳制拉动」全面更差（钳制界 [0.5,2]）。
"""
from __future__ import annotations

import glob
import json
import math
import statistics
import sys
from pathlib import Path

DEFAULT_LOG = Path.home() / ".zcode" / "speed-panel-debug.jsonl"

# 与 src-tauri/src/liveio.rs 的常量保持一致
CAL_QUEUE_CAP = 16
CAL_HALF_LIFE = 3.0
CAL_TRIM_LO, CAL_TRIM_HI = 0.4, 2.5
CAL_SHRINK_N = 3
CAL_AR1_RHO = 0.3
CAL_AR1_CLAMP = 2.0
PRIOR = 600.0


def load_events(path: Path) -> list[dict]:
    """日志 + 同名轮转(.1/.2…)按文件序合并（旧在前），返回接受的 cal 事件序列。"""
    files = sorted(
        glob.glob(str(path) + ".*"),
        key=lambda p: int(p.rsplit(".", 1)[-1]) if p.rsplit(".", 1)[-1].isdigit() else 0,
    )
    files.append(str(path))
    evs: list[dict] = []
    for fp in files:
        try:
            with open(fp, encoding="utf-8", errors="ignore") as f:
                for line in f:
                    try:
                        d = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if d.get("kind") == "cal" and not d.get("skipped") and d.get("bpt_sample", 0) > 0:
                        evs.append(d)
        except OSError:
            continue
    return evs


def _wmed(win: list[float], keep) -> float:
    """新近加权中位数（半衰期 3）：与 liveio::weighted_median 同口径。"""
    pairs = [
        (v, 0.5 ** ((len(win) - 1 - i) / CAL_HALF_LIFE))
        for i, v in enumerate(win)
        if keep(v)
    ]
    if not pairs:
        return sorted(win)[len(win) // 2]
    pairs.sort()
    total = sum(w for _, w in pairs)
    acc = 0.0
    for v, w in pairs:
        acc += w
        if acc >= total / 2:
            return v
    return pairs[-1][0]


def cal_estimate(hist: list[float], prior: float = PRIOR) -> float:
    """与 src-tauri/src/liveio.rs::cal_estimate 同口径
    （两遍法修剪 + AR1 新近收缩 + 先验收缩）。"""
    win = hist[-CAL_QUEUE_CAP:]
    if not win:
        return prior
    center = _wmed(win, lambda _v: True)
    if center <= 0:
        return prior
    est = _wmed(win, lambda v: center * CAL_TRIM_LO <= v <= center * CAL_TRIM_HI)
    ratio = min(max(win[-1] / est, 1.0 / CAL_AR1_CLAMP), CAL_AR1_CLAMP)
    est *= ratio**CAL_AR1_RHO
    shrink = min(len(win) / CAL_SHRINK_N, 1.0)
    return est * shrink + prior * (1 - shrink)


def wmed10(hist: list[float], prior: float = PRIOR) -> float:
    """上代实现：10 样本新近加权中位数（无 AR1 收缩），留作对比。"""
    win = hist[-10:]
    if not win:
        return prior
    center = _wmed(win, lambda _v: True)
    if center <= 0:
        return prior
    est = _wmed(win, lambda v: center * CAL_TRIM_LO <= v <= center * CAL_TRIM_HI)
    shrink = min(len(win) / CAL_SHRINK_N, 1.0)
    return est * shrink + prior * (1 - shrink)


def median5(hist: list[float], prior: float = PRIOR) -> float:
    """旧实现：最近 5 个（含先验占位的队列形态）普通中位数。"""
    win = hist[-5:]
    return sorted(win)[len(win) // 2]


def replay(samples: list[float], est) -> list[float]:
    """预测第 i 个样本时只许用 [0, i) 的历史，误差相对真实样本。"""
    errs, hist = [], []
    for s in samples:
        if hist:
            errs.append(abs(est(hist) - s) / s)
        hist.append(s)
    return errs


def table(name: str, errs: list[float]) -> str:
    errs = sorted(errs)
    med = errs[len(errs) // 2]
    p75 = errs[int(len(errs) * 0.75)]
    p90 = errs[int(len(errs) * 0.90)]
    mean = sum(errs) / len(errs)
    return f"  {name:<10} med={med*100:5.1f}%  p75={p75*100:5.1f}%  p90={p90*100:5.1f}%  mean={mean*100:5.1f}%"


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = {a for a in sys.argv[1:] if a.startswith("--")}
    path = Path(args[0]) if args else DEFAULT_LOG
    evs = load_events(path)
    if len(evs) < 20:
        print(f"有效校准样本不足（{len(evs)} 条）: {path}")
        return 1
    samples = [e["bpt_sample"] for e in evs]
    qs = statistics.quantiles(samples, n=4)
    print(f"样本 n={len(samples)}  min={min(samples):.0f}  p25={qs[0]:.0f}  "
          f"med={statistics.median(samples):.0f}  p75={qs[2]:.0f}  max={max(samples):.0f}")
    print("预测下一调用样本的相对误差（= 实时读数的系数误差）:")
    print(table("prior", replay(samples, lambda _h: PRIOR)))
    print(table("median5", replay(samples, median5)))
    print(table("wmed10", replay(samples, wmed10)))
    print(table("wmed16", replay(samples, cal_estimate)))

    if "--corr" in flags:
        logs = [math.log(s) for s in samples]
        m = sum(logs) / len(logs)
        den = sum((x - m) ** 2 for x in logs)
        for k in (1, 2, 3):
            num = sum((logs[i] - m) * (logs[i + k] - m) for i in range(len(logs) - k))
            print(f"  lag{k} 自相关 = {num / den:.3f}")

    if "--models" in flags:
        import sqlite3
        dbp = Path.home() / ".zcode" / "cli" / "db" / "db.sqlite"
        if dbp.exists():
            db = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
            ids = [e["id"] for e in evs]
            model: dict[str, str] = {}
            for i in range(0, len(ids), 400):
                chunk = ids[i:i + 400]
                q = f"SELECT id, model_id FROM model_usage WHERE id IN ({','.join('?' * len(chunk))})"
                model.update(db.execute(q, chunk))
            groups: dict[str, list[float]] = {}
            for e in evs:
                groups.setdefault(model.get(e["id"], "?"), []).append(e["bpt_sample"])
            print("  按模型分组（信息性：分模型校准对误差无改善，见模块注释）:")
            for name, xs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
                print(f"    {name:<28} n={len(xs):3d}  med={statistics.median(xs):.0f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
