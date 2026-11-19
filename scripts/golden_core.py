#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""G-1 core golden 对拍数据生成器（Python 参考实现 -> JSON fixture）。

用途
----
用生产 Python `chanlun.py` 在**确定性合成 60min 序列**上跑完整分析，把
merged / fractals / bis / zhongshus / b1 / b2 / sell 结果（以及共享的 macd 序列）
写成 JSON fixture；Rust 侧 `crates/chanlun-core/tests/golden_core.rs` 读取同一份
输入 bar + 配置，断言逐项一致（验收标准：0 差异，浮点先量化到 1e-9）。

Python 参考实现（只读，勿改）
----------------------------
默认从 `./chanlun-doc-pkg/code/chanlun.py` 导入；用环境变量 `CHANLUN_REF_ROOT`
（或 `--ref-root`）指向你自己的策略文档包（含 `code/chanlun.py`）。

运行（需要 pandas / numpy）
--------------------------
    export CHANLUN_REF_ROOT=/path/to/缠论二买策略文档_20261117
    python3 scripts/golden_core.py

产物默认写到 `crates/chanlun-core/tests/fixtures/golden_core.json`。

覆盖的配置组合（6 组，3 组走势）
------------------------------
- adjacent / pivot  × break_zd_confirm 0/1  × momentum_mode area/peak（含交叉组合）。

确定性
------
所有随机数来自 `random.Random(seed)`；每个走势在 0..SEED_SEARCH 里按
(b2 覆盖, b1 覆盖, 中枢覆盖, 信号数, 中枢数, 笔数) 打分选中固定 seed，
因此脚本重复运行输出逐字节一致。
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import sys
from datetime import date, timedelta
from pathlib import Path

DEFAULT_REF_ROOT = Path(os.environ.get("CHANLUN_REF_ROOT", "chanlun-doc-pkg"))

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUT = REPO_ROOT / "crates" / "chanlun-core" / "tests" / "fixtures" / "golden_core.json"

# ---------------------------------------------------------------------------
# 配置组合（Python 侧 kwargs = Rust AnalyzerConfig 字段名的 snake_case 版本）
# ---------------------------------------------------------------------------
BASE_CFG = {
    "min_bars_between_fractals": 4,
    "min_segments_in_zhongshu": 3,
    "momentum_mode": "area",
    "require_new_low": True,
    "tolerance_pct": 0.005,
    "use_momentum_weak_for_b2": True,
    "stop_loss_pct": 0.05,
    "max_hold_days": 20,
    "divergence_mode": "adjacent",
    "break_zd_confirm": False,
    "macd_fast": 12,
    "macd_slow": 26,
    "macd_signal": 9,
}

CONFIG_CASES = [
    ("adjacent_area_noconfirm", {}),
    ("pivot_area_noconfirm", {"divergence_mode": "pivot"}),
    ("adjacent_area_confirm", {"break_zd_confirm": True}),
    ("pivot_area_confirm", {"divergence_mode": "pivot", "break_zd_confirm": True}),
    ("adjacent_peak_noconfirm", {"momentum_mode": "peak"}),
    ("pivot_peak_confirm", {"divergence_mode": "pivot", "momentum_mode": "peak", "break_zd_confirm": True}),
]

# Python -> Rust serde 枚举名（fixture 直接被 Rust 反序列化，保持两端枚举字面量一致）
_RUST_FRACTAL_KIND = {"top": "Top", "bottom": "Bottom"}
_RUST_DIRECTION = {"up": "Up", "down": "Down"}

# 三种走势：起始价、分段斜率、两段正弦波 (幅值, 周期 bar, 相位)、逐 bar 对数噪声
SCENARIOS = {
    "swing_up": dict(
        start=18.0,
        slopes=[0.00055],
        waves=[(0.085, 56.0, 0.0), (0.025, 17.0, 1.1)],
        noise=0.0032,
    ),
    "range_oscillation": dict(
        start=25.0,
        slopes=[0.0],
        waves=[(0.12, 46.0, 0.4), (0.02, 11.0, 2.0)],
        noise=0.0028,
    ),
    "downtrend_reversal": dict(
        start=30.0,
        slopes=[-0.0011, 0.0013],  # 前 55% 下跌，之后反转上涨
        waves=[(0.075, 40.0, 0.8), (0.025, 13.0, 0.2)],
        noise=0.0030,
    ),
}

N_BARS = 420
BARS_PER_DAY = 4
SEED_SEARCH = 160
FLOAT_DECIMALS = 12  # 输出前把浮点归一到 1e-12，避免 repr 尾巴噪声（断言容差 1e-9）

# 尾部"顶背驰"形态（仅 downtrend_reversal 使用）：强势上涨 -> 回调 -> 创新高但斜率更弱 -> 回调确认。
# 目的：让序列最后两段已确认上涨笔满足"创新高 + MACD 力度衰竭"，从而覆盖卖出侧顶背驰。
TAIL_SEGMENTS = [(24, 0.0070), (16, -0.0050), (24, 0.0034), (12, -0.0025)]
TAIL_LEN = sum(n for n, _ in TAIL_SEGMENTS)


# ---------------------------------------------------------------------------
# 合成行情
# ---------------------------------------------------------------------------
def _dates(n_bars: int) -> list[int]:
    out: list[int] = []
    d = date(2024, 1, 2)
    while len(out) < n_bars:
        if d.weekday() < 5:
            ymd = d.year * 10000 + d.month * 100 + d.day
            out.extend([ymd] * BARS_PER_DAY)
        d += timedelta(days=1)
    return out[:n_bars]


def _close_path(kind: str, seed: int, n: int) -> list[float]:
    params = SCENARIOS[kind]
    rng = random.Random(seed * 1_000_003 + sum(kind.encode()))
    start, slopes = params["start"], params["slopes"]
    waves, noise = params["waves"], params["noise"]
    closes: list[float] = []
    logp = math.log(start)
    prev_wave = None
    for t in range(n):
        frac = t / max(1, n - 1)
        slope = slopes[0] if len(slopes) == 1 or frac < 0.55 else slopes[1]
        wave = 0.0
        for amp, period, phase in waves:
            wave += amp * math.sin(2.0 * math.pi * t / period + phase)
        dwave = 0.0 if prev_wave is None else wave - prev_wave
        prev_wave = wave
        logp += slope + dwave + rng.gauss(0.0, noise)
        closes.append(math.exp(logp))
    return closes


def _with_tail(kind: str, closes: list[float], rng: random.Random) -> list[float]:
    """给 downtrend_reversal 换一段"创新高但力度衰竭"的尾部（见 TAIL_SEGMENTS）。"""
    if kind != "downtrend_reversal":
        return closes
    k = len(closes) - TAIL_LEN
    p = closes[k - 1]
    out = closes[:k]
    for nseg, drift in TAIL_SEGMENTS:
        for _ in range(nseg):
            p *= math.exp(drift + rng.gauss(0.0, 0.0012))
            out.append(p)
    return out


def _bars(kind: str, seed: int, n: int) -> list[dict]:
    closes = _close_path(kind, seed, n)
    tail_rng = random.Random(seed * 3_571 + 101)
    closes = _with_tail(kind, closes, tail_rng)
    dates = _dates(n)
    rng = random.Random(seed * 7_919 + 13)
    bars: list[dict] = []
    prev = closes[0] * (1.0 + rng.gauss(0.0, 0.0005))
    for t in range(n):
        c = closes[t]
        o = prev
        amp = 0.0015 + abs(rng.gauss(0.0, 0.0022))
        hi = max(o, c) * (1.0 + amp * rng.uniform(0.45, 1.0))
        lo = min(o, c) * (1.0 - amp * rng.uniform(0.45, 1.0))
        bars.append(
            {
                "date": dates[t],
                "open": _q(o),
                "high": _q(hi),
                "low": _q(lo),
                "close": _q(c),
                "volume": 0.0,
                "amount": 0.0,
            }
        )
        prev = c
    return bars


def _q(x: float) -> float:
    """量化到 1e-12：去掉浮点 repr 尾巴，同时让 Python/Rust 输入完全一致。"""
    return round(float(x), FLOAT_DECIMALS)


# ---------------------------------------------------------------------------
# Python 分析 + fixture 序列化
# ---------------------------------------------------------------------------
def _analyze(an, df):
    return an.analyze(df)


def _merged_dict(m) -> dict:
    return {
        "idx": m.idx,
        "start": m.index,
        "end": m.index_end,
        "date_start": int(m.date),
        "date_end": int(m.date_end),
        "open": m.open,
        "high": m.high,
        "low": m.low,
        "close": m.close,
    }


def _fractal_dict(f) -> dict:
    return {
        "idx": f.idx,
        "raw_index": f.index,
        "date": int(f.date),
        "high": f.high,
        "low": f.low,
        "kind": _RUST_FRACTAL_KIND[f.type],
    }


def _bi_dict(b) -> dict:
    return {
        "start_idx": b.start_idx,
        "end_idx": b.end_idx,
        "start_raw": b.start_index,
        "end_raw": b.end_index,
        "start_date": int(b.start_date),
        "end_date": int(b.end_date),
        "start_price": b.start_price,
        "end_price": b.end_price,
        "direction": _RUST_DIRECTION[b.direction],
        "momentum": b.momentum,
        "is_confirmed": bool(b.is_confirmed),
    }


def _zs_dict(z) -> dict:
    return {
        "start_idx": z.start_idx,
        "end_idx": z.end_idx,
        "start_raw": z.start_index,
        "end_raw": z.end_index,
        "start_date": int(z.start_date),
        "end_date": int(z.end_date),
        "zg": z.zg,
        "zd": z.zd,
    }


def _bi_index_by_end(bis, end_idx: int, direction: str) -> int:
    for k, b in enumerate(bis):
        if b.end_idx == end_idx and b.direction == direction:
            return k
    raise AssertionError(f"找不到 end_idx={end_idx} direction={direction} 的笔")


def _zs_index_for(bi_idx: int, zhongshus) -> int | None:
    """镜像 Python `_find_zhongshu_info` 的中枢选择，但返回数组下标。"""
    for k, zs in enumerate(zhongshus):
        if zs.start_idx <= bi_idx <= zs.end_idx:
            return k
    best, best_dist = None, 10**9
    for k, zs in enumerate(zhongshus):
        d = min(abs(bi_idx - zs.start_idx), abs(bi_idx - zs.end_idx))
        if d < best_dist:
            best, best_dist = k, d
    return best


def _pivot_zs_index(an, bis, zhongshus, bi_idx: int) -> int | None:
    pair = an._pivot_entry_for_leave(bis, zhongshus, bi_idx)
    if pair is None:
        return None
    _, zs0 = pair
    for k, zs in enumerate(zhongshus):
        if zs is zs0:
            return k
    return None


def _signal_dict(an, bis, zhongshus, sig, *, is_b2: bool, source_b1=None) -> dict:
    """Python BuySignal -> Rust BuySignal 语义映射。

    - bi_idx：触发笔在 bis 数组中的下标（Python 信号里存的是 merged idx）；
    - raw_index：信号 bar 原始下标；price/low：结构低点；
    - first_buy_bi_idx：一买=自身笔下标，二买=前置一买所在笔下标；
    - zhongshu_idx：Python `zhongshu_info` 指代的中枢下标（pivot=配对中枢，
      adjacent=包含/最近中枢），二买沿用前置一买。
    """
    if is_b2:
        trigger_idx = _bi_index_by_end(bis, sig.bi_idx, "down")
        first_buy_idx = _bi_index_by_end(bis, sig.first_buy_index, "down")
        assert source_b1 is not None and source_b1.bi_idx == sig.first_buy_index
        if an.divergence_mode == "pivot":
            zs_idx = _pivot_zs_index(an, bis, zhongshus, first_buy_idx)
        else:
            zs_idx = _zs_index_for(first_buy_idx, zhongshus)
    else:
        trigger_idx = _bi_index_by_end(bis, sig.bi_idx, "down")
        first_buy_idx = trigger_idx
        if an.divergence_mode == "pivot":
            zs_idx = _pivot_zs_index(an, bis, zhongshus, trigger_idx)
        else:
            zs_idx = _zs_index_for(trigger_idx, zhongshus)
    return {
        "kind": sig.signal,
        "bi_idx": trigger_idx,
        "raw_index": sig.index,
        "date": int(sig.date),
        "price": sig.price,
        "low": sig.low,
        "first_buy_bi_idx": first_buy_idx,
        "zhongshu_idx": zs_idx,
    }


def _pick_entries(result, n_bars: int) -> list[int]:
    raw = {0, n_bars - 1}
    step = max(1, n_bars // 24)
    raw.update(range(step, n_bars, step))
    for b in result.bis:
        if b.is_confirmed:
            raw.add(b.end_index)
    for s in result.b1_signals:
        raw.add(s.index)
    for s in result.b2_signals:
        raw.add(s.index)
    return sorted(x for x in raw if 0 <= x < n_bars)


def _sell_cases(an, df, result, *, break_zd_confirm: bool) -> tuple[list[dict], int]:
    """每条 entry 同时跑 latest_sell_advice 与 sell_advice_from_result。

    返回 (以 latest_sell_advice 为准的用例列表, 两条 Python 路径不一致的条数)。

    Python 怪癖（已记录在 fixture meta）：`sell_advice_from_result` 没有实现
    `break_zd_confirm` 分支（缺少"回抽确认"逻辑），因此 break_zd_confirm=True 时
    它与 `latest_sell_advice` 会不一致；其余配置下两条路径逐条相同（本函数断言）。
    Rust 以回测实际调用的 `latest_sell_advice` 为对拍基准。
    """
    n_bars = len(df)
    out: list[dict] = []
    mismatches = 0
    for entry in _pick_entries(result, n_bars):
        px = float(df.iloc[entry]["close"])
        adv_latest = an.latest_sell_advice(df, entry_price=px, entry_idx=entry)
        adv_result = an.sell_advice_from_result(result, entry_price=px, entry_idx=entry)
        if adv_latest != adv_result:
            mismatches += 1
            assert break_zd_confirm, (
                f"非 confirm 模式下两条 Python 路径不一致: entry={entry} {adv_latest} vs {adv_result}"
            )
            # confirm 分支的差异只允许出现在 break_zd / note（且 latest 是更严格的三卖确认）。
            assert adv_latest["top_divergence"] == adv_result["top_divergence"], (
                f"confirm 模式下顶背驰不一致: entry={entry} {adv_latest} vs {adv_result}"
            )
        out.append(
            {
                "entry_raw": entry,
                "entry_price": _q(px),
                "top_divergence": bool(adv_latest["top_divergence"]),
                "break_zd": bool(adv_latest["break_zd"]),
                "note": adv_latest["note"],
            }
        )
    return out, mismatches


def _rust_config(cfg: dict) -> dict:
    out = dict(cfg)
    out["momentum_mode"] = {"area": "Area", "peak": "Peak"}[cfg["momentum_mode"]]
    out["divergence_mode"] = {"adjacent": "Adjacent", "pivot": "Pivot"}[cfg["divergence_mode"]]
    return out


def _fmt_config_for_analyzer(cfg: dict) -> dict:
    return dict(cfg)


def _top_divergence_proxy(result) -> int:
    """卖出侧顶背驰的廉价代理：最后两段已确认上涨笔满足 新高 + 力度衰竭。

    与 `latest_sell_advice` 的 adjacent 分支同条件（entry_idx=None 时 after_bis 全部已确认笔）。
    """
    ups = [b for b in result.bis if b.direction == "up" and b.is_confirmed]
    if len(ups) < 2:
        return 0
    latest, prev = ups[-1], ups[-2]
    latest_high = max(latest.start_price, latest.end_price)
    prev_high = max(prev.start_price, prev.end_price)
    ok = latest_high > prev_high and latest.momentum < prev.momentum and prev.momentum > 0
    return int(ok)


def _result_score(result) -> tuple:
    has_td = _top_divergence_proxy(result)
    has_b1 = 1 if result.b1_signals else 0
    has_b2 = 1 if result.b2_signals else 0
    has_zs = 1 if result.zhongshus else 0
    return (
        has_td + has_b1 + has_b2 + has_zs,
        has_td,
        has_b2,
        has_b1,
        has_zs,
        len(result.b2_signals) + len(result.b1_signals),
        len(result.zhongshus),
        len(result.bis),
    )


def _select_seed(kind: str, Analyzer) -> tuple[int, dict, dict]:
    """在固定 seed 范围内给走势选一个信号覆盖最好的确定性 seed。"""
    an = Analyzer(**BASE_CFG)
    best_seed, best_score, best_result, best_df = 0, None, None, None
    for seed in range(SEED_SEARCH):
        bars = _bars(kind, seed, N_BARS)
        df = _to_df(bars)
        result = _analyze(an, df)
        sc = _result_score(result)
        if best_score is None or sc > best_score:
            best_seed, best_score, best_result, best_df = seed, sc, result, df
        # 4 个覆盖维度（顶背驰/B1/B2/中枢）都齐 + 信号足够多 -> 取最小 seed。
        if best_score[0] == 4 and best_score[5] >= 8:
            break
    assert best_result is not None and best_df is not None
    return best_seed, best_df, best_result


def _to_df(bars: list[dict]):
    import pandas as pd

    df = pd.DataFrame(bars)
    # Python 侧列顺序无关，date 用 int 即可（merge_klines 内部 astype(str)）
    return df


# ---------------------------------------------------------------------------
# 主体
# ---------------------------------------------------------------------------
def build_fixture(ref_root: Path) -> dict:
    sys.path.insert(0, str(ref_root / "code"))
    from chanlun import ChanlunAnalyzer as Analyzer  # noqa: E402

    datasets = []
    for kind in SCENARIOS:
        seed, _, _ = _select_seed(kind, Analyzer)
        bars = _bars(kind, seed, N_BARS)
        # 用同一份量化后的 bars 重建 df，保证 Python/Rust 输入逐位一致。
        df = _to_df(bars)
        base_result = _analyze(Analyzer(**BASE_CFG), df)

        merged = [_merged_dict(m) for m in base_result.merged_klines]
        fractals = [_fractal_dict(f) for f in base_result.fractals]
        macd = {
            "dif": [_q(v) for v in base_result.macd["dif"].tolist()],
            "dea": [_q(v) for v in base_result.macd["dea"].tolist()],
            "hist": [_q(v) for v in base_result.macd["hist"].tolist()],
        }

        cases = []
        for name, overrides in CONFIG_CASES:
            cfg = dict(BASE_CFG)
            cfg.update(overrides)
            an = Analyzer(**_fmt_config_for_analyzer(cfg))
            result = _analyze(an, df)
            # 配置只应影响笔之后的阶段；合并K线/分型必须一致。
            assert [_merged_dict(m) for m in result.merged_klines] == merged, f"{kind}/{name} merged 不一致"
            assert [_fractal_dict(f) for f in result.fractals] == fractals, f"{kind}/{name} fractals 不一致"

            # 二买 -> 前置一买 的映射（Python 的 first_buy_index 指回 b1.bi_idx）
            b1_dicts = [
                _signal_dict(an, result.bis, result.zhongshus, s, is_b2=False)
                for s in result.b1_signals
            ]
            b2_dicts = []
            for s in result.b2_signals:
                src = [x for x in result.b1_signals if x.bi_idx == s.first_buy_index]
                assert len(src) == 1, f"{kind}/{name} B2 找不到唯一前置 B1: {s.first_buy_index}"
                b2_dicts.append(
                    _signal_dict(an, result.bis, result.zhongshus, s, is_b2=True, source_b1=src[0])
                )

            sell_cases, sell_mismatches = _sell_cases(
                an, df, result, break_zd_confirm=bool(cfg["break_zd_confirm"])
            )
            cases.append(
                {
                    "name": name,
                    "config": _rust_config(cfg),
                    "bis": [_bi_dict(b) for b in result.bis],
                    "zhongshus": [_zs_dict(z) for z in result.zhongshus],
                    "b1": b1_dicts,
                    "b2": b2_dicts,
                    "sell": sell_cases,
                    "sell_from_result_mismatch": sell_mismatches,
                }
            )

        datasets.append(
            {
                "name": kind,
                "seed": seed,
                "bars": bars,
                "expected": {
                    "merged": merged,
                    "fractals": fractals,
                    "macd": macd,
                    "cases": cases,
                },
            }
        )
        print(
            f"[golden] {kind}: seed={seed} bars={len(bars)} merged={len(merged)} "
            f"fractals={len(fractals)} bis={len(base_result.bis)} zs={len(base_result.zhongshus)} "
            f"b1={len(base_result.b1_signals)} b2={len(base_result.b2_signals)}"
        )

    fixture = {
        "meta": {
            "generated_by": "scripts/golden_core.py",
            "python_reference": "chanlun.py (缠论二买策略文档_20261117/code)",
            "n_bars": N_BARS,
            "bars_per_day": BARS_PER_DAY,
            "float_decimals": FLOAT_DECIMALS,
            "config_cases": [name for name, _ in CONFIG_CASES],
            "python_quirks": [
                "sell_advice_from_result 未实现 break_zd_confirm（回抽确认）分支；"
                "break_zd_confirm=True 时与 latest_sell_advice 不一致，Rust 以回测实际调用的 "
                "latest_sell_advice 为准。",
            ],
        },
        "datasets": datasets,
    }
    _check_coverage(fixture)
    return fixture


def _check_coverage(fixture: dict) -> None:
    """确保 fixture 覆盖 acceptance 要求的配置/信号组合。"""
    top_div = break_zd = confirm_note = 0
    b1_total = b2_total = 0
    confirm_mismatch = nonconfirm_mismatch = 0
    for ds in fixture["datasets"]:
        for case in ds["expected"]["cases"]:
            b1_total += len(case["b1"])
            b2_total += len(case["b2"])
            if case["config"]["break_zd_confirm"]:
                confirm_mismatch += case["sell_from_result_mismatch"]
            else:
                nonconfirm_mismatch += case["sell_from_result_mismatch"]
            for sell in case["sell"]:
                top_div += int(sell["top_divergence"])
                break_zd += int(sell["break_zd"])
                confirm_note += int("三卖确认" in sell["note"])
    names = {name for name, _ in CONFIG_CASES}
    for ds in fixture["datasets"]:
        got = {case["name"] for case in ds["expected"]["cases"]}
        assert got == names, f"配置组合缺失: {names - got}"
    assert b1_total > 0, "fixture 未覆盖任何一买信号"
    assert b2_total > 0, "fixture 未覆盖任何二买信号"
    assert top_div > 0, "fixture 未覆盖顶背驰卖出"
    assert break_zd > 0, "fixture 未覆盖跌破中枢ZD卖出"
    assert confirm_note > 0, "fixture 未覆盖 break_zd_confirm（三卖确认）路径"
    assert nonconfirm_mismatch == 0, "非 confirm 配置下两条 Python 卖出路径应完全一致"
    assert confirm_mismatch > 0, "confirm 配置下应复现 Python sell_advice_from_result 缺分支的怪癖"
    print(
        f"[golden] coverage: b1={b1_total} b2={b2_total} sell_top_div={top_div} "
        f"sell_break_zd={break_zd} sell_three_sell_confirm={confirm_note} "
        f"python_path_mismatch(confirm_only)={confirm_mismatch}"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description="生成 chanlun-core G-1 golden fixture")
    ap.add_argument("--ref-root", type=Path, default=Path(os.environ.get("CHANLUN_REF_ROOT", DEFAULT_REF_ROOT)))
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = ap.parse_args()
    if not (args.ref_root / "code" / "chanlun.py").exists():
        print(f"找不到 Python 参考实现: {args.ref_root / 'code' / 'chanlun.py'}", file=sys.stderr)
        return 2
    fixture = build_fixture(args.ref_root)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(fixture, ensure_ascii=False, separators=(",", ":"))
    args.out.write_text(text + "\n", encoding="utf-8")
    print(f"[golden] wrote {args.out} ({len(text) / 1024:.1f} KiB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
