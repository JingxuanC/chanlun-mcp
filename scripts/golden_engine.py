#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""G-2 golden 对拍：Python `backtest_portfolio.py` 参考产物生成器（只读调用参考实现）。

用途
----
在**同一份 60min 数据仓**上跑 Python 组合回测，把逐笔成交 / 净值 / 等价配置落盘成
JSON，供 Rust 侧 `crates/chanlun-engine/tests/golden_engine.rs`（`#[ignore]`）读取并逐笔比对。

产物（`<out-dir>/<case>/`）：

```text
spec.json      # Rust BacktestConfig + universe + data_dir（Rust 测试据此复跑）
trades.json    # {"trades":[{code,buy_date,sell_date,buy_price,sell_price,qty,return_pct,reason,hold_days}]}
nav.json       # {"nav":[{date,nav}]}
python_cmd.txt # 实际执行的 Python 命令（审计用）
stdout.txt     # 参考实现完整输出
```

用法
----
准备参考实现（策略文档包里的 `code/` 目录，自行设置 `CHANLUN_DOC_PKG`）：

```bash
export CHANLUN_DOC_PKG=/path/to/缠论二买策略文档_20261117
```

真实数据仓（推荐）：

```bash
python scripts/golden_engine.py \
  --data-dir /path/to/60min_repo \
  --out-dir /tmp/chanlun-golden-engine \
  --case legacy,dynpos \
  --py-code-dir "$CHANLUN_DOC_PKG/code"
```

合成数据（core 完成后自测用，确定性随机游走，无需真实仓）：

```bash
python scripts/golden_engine.py --synthetic --out-dir /tmp/chanlun-golden-engine --case legacy
```

Rust 侧：

```bash
export CHANLUN_GOLDEN_DIR=/tmp/chanlun-golden-engine
export CHANLUN_GOLDEN_CASE=legacy
export CARGO_HOME=.../cargo-home
cargo test -p chanlun-engine --test golden_engine -- --ignored --nocapture
```

注意
----
- 参考实现目录是只读的：`--out` 一律传绝对路径（backtest_portfolio.py 会写到 out 的同目录）。
- Python 侧行为由其 CLI 决定；本脚本把**所有影响回测的参数显式传满**，并与 spec.json 一一对应，
  避免默认值漂移（Phase D 全域过滤显式关闭，因为 Rust 引擎不实现它）。
- `--dump-signals` 额外落 `signals.json`（Python `scan_confirm_stock` 的已确认信号），
  供 Rust `golden_mock_signal_parity_without_core` 在 core 未完成时单独验证撮合主循环。
- 每个 case 目录还会写 `stats.json`（与 `_gen_doc_stats.py::metrics` 同逻辑的子集 +
  前 5%/前 1% 交易利润占比），供 Rust 侧比对统计口径。
- 退出码非 0 或产物缺失 → 抛异常（CI/Lead 能立刻发现）。
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import os
import subprocess
import sys
import traceback
from pathlib import Path

DEFAULT_PY_CODE_DIR = Path(os.environ.get("CHANLUN_DOC_PKG", "chanlun-doc-pkg")) / "code"

CASES = ("legacy", "dynpos")

# 引擎/参考实现共用的默认参数（spec.json 与 CLI 同源，改这里两边一起变）
BASE = dict(
    capital=1_000_000.0,
    max_buy_per_day=3,
    max_positions=10,
    pos_pct=0.10,
    stop_loss=0.05,
    ma13_exit=True,
    max_hold_days=0,
    confirm_k=1,
    recalc_every=4,
    ma13_entry_filter=0.01,  # Phase B（生产口径）
    max_amount=50_000_000.0,  # Phase C（生产口径）
    window=1500,
    skip_held=True,
    gate_tradeability=True,
    gate_liquidity=True,
    liquidity_pct=0.05,
    slippage_base_amt=50_000.0,
    slippage_cap_amt=2_000_000.0,
    slippage_max=0.01,
    divergence="adjacent",
    break_zd_confirm=False,
    start_date="",
    end_date="",
)


def analyzer_dict(divergence: str, break_zd_confirm: bool) -> dict:
    """与 chanlun_core::AnalyzerConfig 的 serde 字段完全对应。"""
    return {
        "min_bars_between_fractals": 4,
        "min_segments_in_zhongshu": 3,
        "momentum_mode": "Area",
        "require_new_low": True,
        "tolerance_pct": 0.005,
        "use_momentum_weak_for_b2": True,
        "stop_loss_pct": 0.05,
        "max_hold_days": 20,
        "divergence_mode": "Pivot" if divergence == "pivot" else "Adjacent",
        "break_zd_confirm": bool(break_zd_confirm),
        "macd_fast": 12,
        "macd_slow": 26,
        "macd_signal": 9,
    }


def build_spec(case: str, data_dir: Path, st_flags: Path | None, p: dict) -> dict:
    """生成 Rust `BacktestConfig`（serde 字段名/枚举名必须一致）。"""
    dyn = case == "dynpos"
    cfg = {
        "capital": float(p["capital"]),
        "funding_mode": "dyn_pos_cap100w" if dyn else "legacy",
        "max_buy_per_day": int(p["max_buy_per_day"]),
        "max_positions": int(p["max_positions"]),
        "pos_pct": float(p["pos_pct"]),
        "stop_loss": float(p["stop_loss"]),
        "ma13_exit": bool(p["ma13_exit"]),
        "max_hold_days": int(p["max_hold_days"]),
        "confirm_delay": 4,
        "confirm_k": int(p["confirm_k"]),
        "recalc_every": int(p["recalc_every"]),
        "ma13_entry_filter": float(p["ma13_entry_filter"]),
        "max_amount": float(p["max_amount"]),
        "window": int(p["window"]),
        "dynamic_pos": dyn,
        "max_trade_amt": 1_000_000.0 if dyn else 0.0,
        "skip_held": bool(p["skip_held"]),
        "gate_tradeability": bool(p["gate_tradeability"]),
        "gate_liquidity": bool(p["gate_liquidity"]),
        "liquidity_pct": float(p["liquidity_pct"]),
        "slippage_base_amt": float(p["slippage_base_amt"]),
        "slippage_cap_amt": float(p["slippage_cap_amt"]),
        "slippage_max": float(p["slippage_max"]),
        "start_date": int(p["start_date"]) if p["start_date"] else None,
        "end_date": int(p["end_date"]) if p["end_date"] else None,
        "exit_check": "daily_close",
        "st_flags_path": str(st_flags) if st_flags else None,
        "analyzer": analyzer_dict(p["divergence"], p["break_zd_confirm"]),
    }
    return {"case": case, "data_dir": str(data_dir), "config": cfg}


def code_from_filename(name: str) -> str:
    """与 backtest_portfolio.py 的 code 推导完全一致。"""
    return os.path.basename(name).replace("_60min.csv", "").replace("sh_", "").replace("sz_", "")


def list_universe(data_dir: Path) -> list[str]:
    files = sorted(data_dir.glob("*.csv"))
    return sorted({code_from_filename(f) for f in files})


def python_argv(
    py: str,
    py_code_dir: Path,
    data_dir: Path,
    case: str,
    st_flags: Path | None,
    out_trades: Path,
    p: dict,
    workers: int,
) -> list[str]:
    dyn = case == "dynpos"
    cmd = [
        py,
        str(py_code_dir / "backtest_portfolio.py"),
        "--workers",
        str(workers),
        "--limit",
        "0",
        "--capital",
        repr(float(p["capital"])),
        "--max-buy-per-day",
        str(int(p["max_buy_per_day"])),
        "--max-positions",
        str(int(p["max_positions"])),
        "--pos-pct",
        repr(float(p["pos_pct"])),
        "--stop-loss",
        repr(float(p["stop_loss"])),
        "--ma13-exit",
        "1" if p["ma13_exit"] else "0",
        "--max-hold",
        str(int(p["max_hold_days"])),
        "--confirm-k",
        str(int(p["confirm_k"])),
        "--recalc-every",
        str(int(p["recalc_every"])),
        "--divergence",
        str(p["divergence"]),
        "--break-zd-confirm",
        "1" if p["break_zd_confirm"] else "0",
        "--ma13-entry-filter",
        repr(float(p["ma13_entry_filter"])),
        "--max-amount",
        repr(float(p["max_amount"])),
        "--window",
        str(int(p["window"])),
        "--dynamic-pos",
        "1" if dyn else "0",
        "--max-trade-amt",
        repr(1_000_000.0 if dyn else 0.0),
        "--skip-held",
        "1" if p["skip_held"] else "0",
        "--gate-tradeability",
        "1" if p["gate_tradeability"] else "0",
        "--gate-liquidity",
        "1" if p["gate_liquidity"] else "0",
        "--liquidity-pct",
        repr(float(p["liquidity_pct"])),
        "--slippage-base-amt",
        repr(float(p["slippage_base_amt"])),
        "--slippage-cap-amt",
        repr(float(p["slippage_cap_amt"])),
        "--slippage-max",
        repr(float(p["slippage_max"])),
        # Phase D 全域过滤：Rust 引擎不实现，显式关闭
        "--universe-filters",
        "",
        "--exclude-st",
        "0",
        "--min-daily-bars",
        "0",
        "--min-cap-yi",
        "0.0",
        "--snapshot",
        "0",
        "--out",
        str(out_trades),
    ]
    if st_flags is not None and st_flags.exists():
        cmd += ["--st-flags", str(st_flags)]
    if p["start_date"]:
        cmd += ["--start-date", str(p["start_date"])]
    if p["end_date"]:
        cmd += ["--end-date", str(p["end_date"])]
    return cmd


def parse_trades_csv(path: Path) -> list[dict]:
    rows: list[dict] = []
    with open(path, encoding="utf-8-sig", newline="") as f:
        for r in csv.DictReader(f):
            code = str(r["code"]).strip()
            if code.endswith(".0"):
                code = code[:-2]
            code = code.zfill(6)
            buy_date, sell_date = norm_date(r["buy_date"]), norm_date(r["sell_date"])
            rows.append(
                {
                    "code": code,
                    "buy_date": buy_date,
                    "sell_date": sell_date,
                    "buy_price": float(r["buy_price"]),
                    "sell_price": float(r["sell_price"]),
                    "qty": int(float(r["qty"])),
                    "return_pct": float(r["return_pct"]),
                    "reason": str(r["reason"]),
                    "hold_days": int(float(r["hold_days"])),
                }
            )
    return rows


def norm_date(x) -> int:
    s = "".join(ch for ch in str(x) if ch.isdigit())
    if len(s) >= 8:
        return int(s[:8])
    s2 = str(int(float(x)))
    return int(s2[:8])


def parse_nav_csv(path: Path) -> list[dict]:
    out: list[dict] = []
    with open(path, encoding="utf-8-sig", newline="") as f:
        rows = list(csv.reader(f))
    for row in rows:
        if len(row) < 2:
            continue
        d = row[0].strip()
        if len(d) != 8 or not d.isdigit():
            continue
        try:
            nav = float(row[1])
        except ValueError:
            continue
        if math.isfinite(nav):
            out.append({"date": int(d), "nav": nav})
    return out


# ---------------------------------------------------------------------------
# 合成数据仓（core 就绪后可无真实数据自测；确定性，Python/Rust 读同一份 CSV）
# ---------------------------------------------------------------------------


def gen_synthetic_repo(data_dir: Path, n_stocks: int = 6, n_days: int = 420, seed: int = 42):
    """生成确定性 60min 合成仓（每交易日 4 根；导出格式 date,open,...,amount）。"""
    import datetime as dt

    import numpy as np

    rng = np.random.default_rng(seed)
    data_dir.mkdir(parents=True, exist_ok=True)
    codes = ["600000", "600001", "000001", "000002", "300001", "688001"][:n_stocks]
    # 交易日历（跳过周末）
    days: list[dt.date] = []
    d = dt.date(2020, 1, 2)
    while len(days) < n_days:
        if d.weekday() < 5:
            days.append(d)
        d += dt.timedelta(days=1)
    for ci, code in enumerate(codes):
        n_bars = len(days) * 4
        # 慢趋势 + 快波动，保证出现笔/中枢结构
        t = np.arange(n_bars)
        trend = 0.12 * np.sin(2 * np.pi * t / (n_bars / 3.0 + ci))
        noise = rng.normal(0.0, 0.011, n_bars)
        rets = trend * 0.002 + noise
        closes = 10.0 * np.exp(np.cumsum(rets))
        opens = np.empty_like(closes)
        opens[0] = closes[0]
        opens[1:] = closes[:-1] * (1.0 + rng.normal(0.0, 0.002, n_bars - 1))
        highs = np.maximum(opens, closes) * (1.0 + np.abs(rng.normal(0.0, 0.002, n_bars)))
        lows = np.minimum(opens, closes) * (1.0 - np.abs(rng.normal(0.0, 0.002, n_bars)))
        vols = rng.integers(200_000, 2_000_000, n_bars).astype(float)
        amounts = vols * (opens + closes) / 2.0
        ex = "sh" if code[0] == "6" or code[0] == "9" else "sz"
        path = data_dir / f"{ex}_{code}_60min.csv"
        with open(path, "w", encoding="utf-8", newline="") as f:
            w = csv.writer(f)
            w.writerow(["date", "open", "high", "low", "close", "volume", "amount"])
            for bi in range(n_bars):
                day = days[bi // 4]
                w.writerow(
                    [
                        day.strftime("%Y%m%d"),
                        round(float(opens[bi]), 3),
                        round(float(highs[bi]), 3),
                        round(float(lows[bi]), 3),
                        round(float(closes[bi]), 3),
                        round(float(vols[bi]), 0),
                        round(float(amounts[bi]), 0),
                    ]
                )
    print(f"[synthetic] {len(codes)} 只 × {len(days)} 交易日 × 4 根 -> {data_dir}")
    return codes


# ---------------------------------------------------------------------------
# 单 case 执行
# ---------------------------------------------------------------------------


def compute_doc_stats(trades: list[dict], nav: list[dict]) -> dict:
    """与 `code/_gen_doc_stats.py::metrics` 同逻辑的统计子集（不产图，故不依赖 matplotlib）。

    另加 review-notes §9 要求的前 5% / 前 1% 交易利润占比（定义已用基线 CSV 复核：
    利润 = return_pct × qty × buy_price，分母为全部交易净利润之和）。
    """
    import numpy as np
    import pandas as pd

    if not trades or not nav:
        return {}
    tr = pd.DataFrame(trades)
    nav_df = pd.DataFrame(nav)
    d = pd.to_datetime(nav_df["date"].astype(str), format="%Y%m%d")
    v = nav_df["nav"].astype(float)
    years = (d.iloc[-1] - d.iloc[0]).days / 365.25
    cagr = (v.iloc[-1] / v.iloc[0]) ** (1 / years) - 1 if years > 0 else 0.0
    dd = v / v.cummax() - 1

    real = tr[tr["reason"] != "估值平仓（非成交）"].copy()
    real["return_pct"] = real["return_pct"].astype(float)
    win = real[real["return_pct"] > 0]
    lose = real[real["return_pct"] <= 0]
    gross_win = (win["return_pct"] * win["qty"] * win["buy_price"]).sum()
    gross_lose = (lose["return_pct"] * lose["qty"] * lose["buy_price"]).sum()
    profits = real["return_pct"] * real["qty"] * real["buy_price"]
    total = float(profits.sum())

    def share(pct: float) -> float:
        if len(real) == 0 or total == 0:
            return 0.0
        k = int(np.ceil(len(real) * pct))
        return float(profits.nlargest(k).sum() / total)

    return {
        "years": float(years),
        "cagr": float(cagr),
        "max_dd": float(dd.min()),
        "nav_end": float(v.iloc[-1]),
        "n_trades_real": int(len(real)),
        "n_mark": int(len(tr) - len(real)),
        "win_rate": float((real["return_pct"] > 0).mean()) if len(real) else 0.0,
        "avg_ret": float(real["return_pct"].mean()) if len(real) else 0.0,
        "median_ret": float(real["return_pct"].median()) if len(real) else 0.0,
        "avg_win": float(win["return_pct"].mean()) if len(win) else 0.0,
        "avg_lose": float(lose["return_pct"].mean()) if len(lose) else 0.0,
        "pl_ratio": (
            float(abs(win["return_pct"].mean() / lose["return_pct"].mean()))
            if len(win) and len(lose) and lose["return_pct"].mean() != 0
            else 0.0
        ),
        "profit_factor": float(gross_win / abs(gross_lose)) if gross_lose != 0 else 0.0,
        "hold_avg": float(real["hold_days"].astype(float).mean()) if len(real) else 0.0,
        "hold_med": float(real["hold_days"].astype(float).median()) if len(real) else 0.0,
        "top5pct_profit_share": share(0.05),
        "top1pct_profit_share": share(0.01),
    }


def dump_signals(
    out_dir: Path, data_dir: Path, py_code_dir: Path, params: dict
) -> dict[str, list[dict]]:
    """用参考实现 `scan_confirm_stock` 逐票重算「已确认信号」，供 Rust MockSignalSource 复跑。

    说明：信号的扫描/确认与资金口径无关；这里独立重算一遍（不会改变参考实现的回测产物）。
    注意与 `backtest_portfolio.main` 用完全相同的参数，保证信号集合一致。
    """
    import importlib.util

    ref = py_code_dir / "backtest_portfolio.py"
    spec = importlib.util.spec_from_file_location("_backtest_portfolio_ref", ref)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"无法加载参考实现: {ref}")
    mod = importlib.util.module_from_spec(spec)
    sys.path.insert(0, str(py_code_dir))
    spec.loader.exec_module(mod)

    an_kwargs = {
        "divergence_mode": str(params["divergence"]),
        "break_zd_confirm": bool(params["break_zd_confirm"]),
    }
    out: dict[str, list[dict]] = {}
    n_conf = 0
    for f in sorted(data_dir.glob("*.csv")):
        r = mod.scan_confirm_stock(
            str(f),
            int(params["recalc_every"]),
            int(params["confirm_k"]),
            4,
            an_kwargs,
            float(params["ma13_entry_filter"]),
            float(params["max_amount"]),
            False,  # risk_sizing
            False,  # allow_pending
            "",  # universe_path（Phase D 关闭，与引擎一致）
            False,
            0,
            0.0,
        )
        sigs = []
        for s in r["signals"]:
            if s.get("pending"):
                continue
            sigs.append(
                {
                    "buy_bar": int(s["buy_bar"]),
                    "buy_date": int(norm_date(s["buy_date"])),
                    "price": float(s["price"]),
                }
            )
        if sigs:
            out[str(r["code"])] = sigs
            n_conf += len(sigs)
    (out_dir / "signals.json").write_text(
        json.dumps({"signals": out}, ensure_ascii=False, indent=1), encoding="utf-8"
    )
    print(f"[signals] 已确认信号 {n_conf} 个 / {len(out)} 只 -> {out_dir / 'signals.json'}")
    return out


def run_case(
    case: str,
    data_dir: Path,
    out_root: Path,
    py_code_dir: Path,
    st_flags: Path | None,
    params: dict,
    workers: int,
) -> dict:
    assert case in CASES, f"未知 case: {case}"
    out_dir = out_root / case
    out_dir.mkdir(parents=True, exist_ok=True)
    trades_csv = (out_dir / "trades.csv").resolve()
    nav_csv = Path(str(trades_csv).replace(".csv", "_nav.csv"))

    cmd = python_argv(
        sys.executable, py_code_dir, data_dir, case, st_flags, trades_csv, params, workers
    )
    env = dict(os.environ)
    env["PF_DATA_DIR"] = str(data_dir)
    env["PYTHONIOENCODING"] = "utf-8"
    print(f"[{case}] 运行参考实现（{len(list_universe(data_dir))} 只）...")
    proc = subprocess.run(
        cmd,
        cwd=str(out_dir),
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    (out_dir / "stdout.txt").write_text(
        (proc.stdout or "") + "\n---- STDERR ----\n" + (proc.stderr or ""), encoding="utf-8"
    )
    (out_dir / "python_cmd.txt").write_text(" ".join(cmd), encoding="utf-8")
    if proc.returncode != 0:
        raise RuntimeError(
            f"[{case}] 参考实现退出码 {proc.returncode}\n"
            f"cmd: {' '.join(cmd)}\n"
            f"stdout tail:\n{(proc.stdout or '')[-3000:]}\n"
            f"stderr tail:\n{(proc.stderr or '')[-3000:]}"
        )
    if not trades_csv.exists():
        # 无信号时 backtest_portfolio.py 直接退出、不写 CSV
        print(f"[{case}] ⚠️ 无成交（参考实现未写 trades.csv），仍写空产物")
        trades: list[dict] = []
    else:
        trades = parse_trades_csv(trades_csv)
    nav = parse_nav_csv(nav_csv) if nav_csv.exists() else []
    spec = build_spec(case, data_dir, st_flags, params)
    spec["universe"] = list_universe(data_dir)
    stats = compute_doc_stats(trades, nav)

    (out_dir / "trades.json").write_text(
        json.dumps({"trades": trades}, ensure_ascii=False, indent=1), encoding="utf-8"
    )
    (out_dir / "nav.json").write_text(
        json.dumps({"nav": nav}, ensure_ascii=False, indent=1), encoding="utf-8"
    )
    (out_dir / "spec.json").write_text(
        json.dumps(spec, ensure_ascii=False, indent=1), encoding="utf-8"
    )
    (out_dir / "stats.json").write_text(
        json.dumps(stats, ensure_ascii=False, indent=1), encoding="utf-8"
    )

    n_fills = sum(1 for t in trades if t["reason"] != "估值平仓（非成交）")
    nav_end = nav[-1]["nav"] if nav else float("nan")
    total_ret = (nav_end / params["capital"] - 1.0) if nav else float("nan")
    print(
        f"[{case}] 完成：成交 {n_fills} 笔（含估值平仓 {len(trades) - n_fills} 笔）、"
        f"净值 {nav_end / 1e4:.1f} 万（{total_ret * 100:+.1f}%）-> {out_dir}"
    )
    return {"trades": len(trades), "fills": n_fills, "nav": len(nav), "nav_end": nav_end}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="G-2 golden：Python 参考产物生成器")
    ap.add_argument("--data-dir", default="", help="60min 数据仓目录（与 PF_DATA_DIR 同义）")
    ap.add_argument("--synthetic", action="store_true", help="自动生成确定性合成数据仓")
    ap.add_argument("--synthetic-stocks", type=int, default=6)
    ap.add_argument("--synthetic-days", type=int, default=420)
    ap.add_argument("--out-dir", required=True, help="产物根目录（每 case 一个子目录）")
    ap.add_argument("--case", default="legacy", help="legacy / dynpos / legacy,dynpos / all")
    ap.add_argument("--py-code-dir", default=DEFAULT_PY_CODE_DIR, help="参考实现 code/ 目录")
    ap.add_argument("--st-flags", default="", help="st_flags.json（缺省用参考实现目录下同名文件，不存在则 null）")
    ap.add_argument("--workers", type=int, default=8, help="参考实现扫描进程数")
    ap.add_argument(
        "--dump-signals",
        action="store_true",
        help="额外落 signals.json（供 Rust MockSignalSource 端到端复跑，验证撮合回测 parity）",
    )
    # 以下与 BASE 同义，可显式覆盖
    ap.add_argument("--capital", type=float, default=BASE["capital"])
    ap.add_argument("--stop-loss", type=float, default=BASE["stop_loss"])
    ap.add_argument("--ma13-exit", type=int, default=1)
    ap.add_argument("--max-hold", type=int, default=BASE["max_hold_days"])
    ap.add_argument("--skip-held", type=int, default=1)
    ap.add_argument("--divergence", choices=["adjacent", "pivot"], default=BASE["divergence"])
    ap.add_argument("--break-zd-confirm", type=int, default=0)
    ap.add_argument("--start-date", default="")
    ap.add_argument("--end-date", default="")
    ap.add_argument("--ma13-entry-filter", type=float, default=BASE["ma13_entry_filter"])
    ap.add_argument("--max-amount", type=float, default=BASE["max_amount"])
    args = ap.parse_args(argv)

    out_root = Path(args.out_dir).resolve()
    out_root.mkdir(parents=True, exist_ok=True)

    py_code_dir = Path(args.py_code_dir)
    ref = py_code_dir / "backtest_portfolio.py"
    if not ref.exists():
        print(f"❌ 参考实现不存在: {ref}\n   用 --py-code-dir 指向 缠论二买策略文档_20261117/code",
              file=sys.stderr)
        return 2

    if args.data_dir:
        data_dir = Path(args.data_dir).resolve()
        if not data_dir.is_dir():
            print(f"❌ 数据仓不存在: {data_dir}", file=sys.stderr)
            return 2
    else:
        if not args.synthetic:
            print("❌ 需要 --data-dir 或 --synthetic", file=sys.stderr)
            return 2
        data_dir = (out_root / "data").resolve()
        gen_synthetic_repo(data_dir, args.synthetic_stocks, args.synthetic_days)

    st_flags: Path | None = Path(args.st_flags).resolve() if args.st_flags else None
    if st_flags is None:
        cand = py_code_dir / "st_flags.json"
        st_flags = cand if cand.exists() else None

    params = dict(BASE)
    params["capital"] = args.capital
    params["stop_loss"] = args.stop_loss
    params["ma13_exit"] = bool(args.ma13_exit)
    params["max_hold_days"] = args.max_hold
    params["skip_held"] = bool(args.skip_held)
    params["divergence"] = args.divergence
    params["break_zd_confirm"] = bool(args.break_zd_confirm)
    params["start_date"] = args.start_date
    params["end_date"] = args.end_date
    params["ma13_entry_filter"] = args.ma13_entry_filter
    params["max_amount"] = args.max_amount

    if args.case == "all":
        cases = list(CASES)
    elif args.case == "legacy,dynpos":
        cases = list(CASES)
    else:
        cases = [c.strip() for c in args.case.split(",") if c.strip()]

    try:
        results = {}
        for case in cases:
            results[case] = run_case(
                case, data_dir, out_root, py_code_dir, st_flags, params, args.workers
            )
            if args.dump_signals:
                dump_signals(out_root / case, data_dir, py_code_dir, params)
        (out_root / "manifest.json").write_text(
            json.dumps(
                {
                    "data_dir": str(data_dir),
                    "cases": cases,
                    "results": results,
                    "py_code_dir": str(py_code_dir),
                },
                ensure_ascii=False,
                indent=1,
            ),
            encoding="utf-8",
        )
    except Exception:
        traceback.print_exc()
        return 1
    print(f"\n✅ 产物已写入 {out_root}")
    print("Rust 侧执行：")
    print(f"  export CHANLUN_GOLDEN_DIR={out_root}")
    print("  export CHANLUN_GOLDEN_CASE=legacy   # 或 dynpos")
    print("  cargo test -p chanlun-engine --test golden_engine -- --ignored --nocapture")
    return 0


if __name__ == "__main__":
    sys.exit(main())
