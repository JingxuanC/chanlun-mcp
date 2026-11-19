//! 撮合主循环（逐行对齐 Python `code/backtest_portfolio.py:main` 的阶段 3）。
//!
//! 单资金池、T+1、昨日收盘决策 / 今日开盘成交、一字跌停顺延、单日 3 买 / 最多 10 仓、
//! dynpos（昨日净值 × 10% 封顶 100 万）/ legacy 固定 10 万、100 股取整、期末估值平仓。
//!
//! ## 与 Python 的逐行对应
//!
//! ```text
//! for day in 交易日历:                        # 日历 = 有信号股票 bar 日期的并集
//!   0) 执行昨日 pending_sell（今日开盘价；一字跌停顺延；停牌保留）
//!   1) 卖出决策（当日最后一根 bar 收盘；T+1；止损 > MA13 > 超时 > 缠论卖点）
//!   2) 买入（当日信号；skip-held / 单日上限 / 持仓上限 / 预算 / 100 股取整 / P1+P2 闸门）
//!   3) 当日净值 = 现金 + Σ 持仓 × 当日最后一根 bar 收盘（停牌按成本）
//! 期末：估值平仓（非成交，reason="估值平仓（非成交）"，不进统计；净值用平仓前）
//! ```
//!
//! ## 刻意对齐的 Python 细节
//!
//! - 扫描阶段用 float64 原始价（`downcast=False`）；撮合阶段把 OHLCV **量化到 float32**
//!   （`downcast=True`，amount 不量化）——qty 用扫描价算、成本与 entry_price 用撮合价算；
//! - `pending_sell` 停牌时保留、复牌日继续尝试开盘价卖出；
//! - 一字跌停顺延到**当日首个非一字跌停 bar**（不是顺延到次日）；
//! - 现金不足时：legacy 整笔跳过；dynpos/max_trade_amt 模式用剩余现金缩量；
//! - 已持仓再次出信号：`skip_held=true` 跳过；`false` 覆盖旧仓（旧仓成本丢失，审计口径）；
//! - 期末估值平仓顺序按代码排序（Python 用 dict 插入序；只影响交易明细行序，不影响统计）。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chanlun_core::{Bar, Date8};

use crate::gates::{board_limit, exceeds_relative_liquidity, is_one_word};
use crate::sell::SellAdvisor;
use crate::signals::{ConfirmedSignal, SignalSource};
use crate::slippage::slippage_for;
use crate::st::StTable;
use crate::stats::compute_stats;
use crate::{
    BacktestConfig, BacktestResult, BlockedSignal, Diagnostics, EngineError, ExitCheckMode,
    FundingMode, NavPoint, Trade, VALUATION_REASON, py_round, stats::days_between,
};

/// 撮合阶段的单票数据（OHLCV 已量化到 float32，与 Python `downcast=True` 对齐）。
#[derive(Debug, Clone)]
struct SimStock {
    bars: Vec<Bar>,
    /// 日期 -> `[start, end)` 原始 bar 下标区间（bar 按日期升序）。
    day_ranges: HashMap<Date8, (usize, usize)>,
}

impl SimStock {
    fn day_range(&self, day: Date8) -> Option<(usize, usize)> {
        self.day_ranges.get(&day).copied()
    }

    /// 当日最后一根 bar 的收盘价。
    fn day_close(&self, day: Date8) -> Option<f64> {
        self.day_range(day).map(|(_, end)| self.bars[end - 1].close)
    }
}

#[derive(Debug, Clone)]
struct Position {
    qty: i64,
    /// 含滑点与买入费用的总成本
    cost: f64,
    entry_price: f64,
    buy_date: Date8,
    buy_bar_idx: usize,
    /// 上次缠论卖点重算的交易日下标（Python `last_check_day`）
    last_check_day: usize,
}

/// 扫描结果（code + 已确认信号）。
#[derive(Debug, Clone)]
struct ScanSignal {
    code: String,
    sig: ConfirmedSignal,
}

/// 扫描阶段清洗 bar：删 NaN OHLC 行 + 零价停牌占位行（对齐 Python `load_stock`）。
fn clean_scan_bars(mut bars: Vec<Bar>) -> Vec<Bar> {
    bars.retain(|b| !(b.open.is_nan() || b.high.is_nan() || b.low.is_nan() || b.close.is_nan()));
    bars.retain(|b| {
        // (OHLC 全 0) & (volume 0) -> 停牌占位行
        !(b.open == 0.0 && b.high == 0.0 && b.low == 0.0 && b.close == 0.0 && b.volume == 0.0)
    });
    bars
}

/// 撮合阶段加载（对齐 Python `load_stock(..., downcast=True)`）：
/// OHLCV 量化到 float32 再转回 f64，`amount` 保持 f64。
fn load_sim_stock(
    store: &chanlun_data::store::Store,
    code: &str,
) -> Result<Option<SimStock>, EngineError> {
    let mut bars = clean_scan_bars(store.load_series(code)?.bars);
    if bars.is_empty() {
        return Ok(None);
    }
    for b in &mut bars {
        b.open = f64::from(b.open as f32);
        b.high = f64::from(b.high as f32);
        b.low = f64::from(b.low as f32);
        b.close = f64::from(b.close as f32);
        b.volume = f64::from(b.volume as f32);
        // amount 不量化（Python 只 downcast OHLCV）
    }
    let mut day_ranges: HashMap<Date8, (usize, usize)> = HashMap::new();
    for (i, b) in bars.iter().enumerate() {
        day_ranges
            .entry(b.date)
            .and_modify(|e| e.1 = i + 1)
            .or_insert((i, i + 1));
    }
    Ok(Some(SimStock { bars, day_ranges }))
}

/// 组合回测（可注入信号源 + 卖点判定器 + ST 表）。
///
/// 这是引擎的真正入口；[`crate::run`] 用生产实现调它，单测用 Mock 调它。
pub fn run_with(
    store: &chanlun_data::store::Store,
    universe: &[String],
    cfg: &BacktestConfig,
    signals: &mut dyn SignalSource,
    sells: &mut dyn SellAdvisor,
    st: &StTable,
) -> Result<BacktestResult, EngineError> {
    if cfg.exit_check != ExitCheckMode::DailyClose {
        // 文档口径（60min bar 级出场）与 Python 代码不一致；先 parity，bar 级另开档位。
        return Err(EngineError::Unimplemented(
            "ExitCheckMode::Every60MinBar（文档口径，尚未实现；当前只支持 DailyClose）",
        ));
    }

    // ---- 阶段 1：扫描 + 确认 ----
    let universe: Vec<String> = if universe.is_empty() {
        store.list_symbols()?
    } else {
        universe.to_vec()
    };

    let mut n_scan_errors = 0usize;
    let mut scan_signals: Vec<ScanSignal> = Vec::new();
    for code in &universe {
        let bars = match store.load_series(code) {
            Ok(series) => clean_scan_bars(series.bars),
            Err(_) => {
                // Python: 单票读取/解析失败 -> n_err++，继续
                n_scan_errors += 1;
                continue;
            }
        };
        // 信号源自身对「K线不足 / 分析异常」的语义与 Python `scan_confirm_stock` 对齐：
        // 返回空列表表示该票无有效确认信号（不视为整次回测失败）。
        let sigs = match signals.scan(code, &bars, cfg) {
            Ok(sigs) => sigs,
            Err(_) => {
                n_scan_errors += 1;
                continue;
            }
        };
        for sig in sigs {
            scan_signals.push(ScanSignal {
                code: code.clone(),
                sig,
            });
        }
    }

    let mut diag = Diagnostics {
        n_signals: scan_signals.len(),
        n_scan_errors,
        ..Diagnostics::default()
    };

    // Python: signals = sorted(all_signals, key=(buy_date, code))
    scan_signals
        .sort_by(|a, b| (a.sig.buy_date, a.code.as_str()).cmp(&(b.sig.buy_date, b.code.as_str())));

    // 无信号：Python 直接「无信号，退出」（不产生净值/交易）
    if scan_signals.is_empty() {
        return Ok(BacktestResult {
            diagnostics: diag,
            ..BacktestResult::default()
        });
    }

    // ---- 阶段 2：只加载有信号的票（撮合口径）----
    let codes: BTreeSet<&str> = scan_signals.iter().map(|s| s.code.as_str()).collect();
    let mut sim: HashMap<String, SimStock> = HashMap::new();
    for code in &codes {
        match load_sim_stock(store, code) {
            // Python: 文件不存在 -> 不入 cache（对应信号后续按 df=None 处理）
            Ok(Some(stock)) => {
                sim.insert((*code).to_string(), stock);
            }
            Ok(None) | Err(_) => {}
        }
    }

    // 交易日历 = 已缓存票的日期并集（Python 行为；未加载成功的票不贡献日历日）
    let mut day_set: BTreeSet<Date8> = BTreeSet::new();
    for stock in sim.values() {
        for b in &stock.bars {
            day_set.insert(b.date);
        }
    }
    let days: Vec<Date8> = day_set.into_iter().collect();
    if days.is_empty() {
        return Ok(BacktestResult {
            diagnostics: diag,
            ..BacktestResult::default()
        });
    }
    let day_idx: HashMap<Date8, usize> = days.iter().enumerate().map(|(i, &d)| (d, i)).collect();

    // 信号按日分组（保持 (buy_date, code) 排序序）
    let mut sig_by_day: BTreeMap<Date8, Vec<ScanSignal>> = BTreeMap::new();
    for s in scan_signals {
        sig_by_day.entry(s.sig.buy_date).or_default().push(s);
    }

    // ---- 阶段 3：组合模拟 ----
    let mut cash = cfg.capital;
    // Python 用 dict 插入序；BTreeMap 保证确定性输出（卖/买/净值计算与持仓顺序无关）
    let mut positions: BTreeMap<String, Position> = BTreeMap::new();
    let mut pending_sell: BTreeMap<String, String> = BTreeMap::new();
    let mut trades: Vec<Trade> = Vec::new();
    let mut nav_series: Vec<NavPoint> = Vec::with_capacity(days.len());
    let mut blocked: Vec<BlockedSignal> = Vec::new();

    let mut prev_nav = cfg.capital;
    let mut peak_nav = cfg.capital; // 仅 dd_guard 用（未实现），保留语义

    let slip = |amt: f64| {
        slippage_for(
            amt,
            cfg.slippage_base_amt,
            cfg.slippage_cap_amt,
            cfg.slippage_max,
        )
    };

    for (di, &day) in days.iter().enumerate() {
        // ---- 0) 执行昨日卖出决策（今日开盘价）----
        let pending_codes: Vec<String> = pending_sell.keys().cloned().collect();
        for code in pending_codes {
            let Some(stock) = sim.get(&code) else {
                continue; // df 缺 -> 保留 pending，顺延
            };
            let Some((start, end)) = stock.day_range(day) else {
                continue; // 停牌，顺延
            };

            // 可成交性：一字跌停不顺延到开盘价，而是顺延至当日首个非一字跌停 bar
            let open_px = if cfg.gate_tradeability {
                let limit = board_limit(&code, st.is_st(&code, day));
                let mut chosen: Option<f64> = None;
                for idx in start..end {
                    let bar = &stock.bars[idx];
                    let prev_close = if idx > 0 {
                        Some(stock.bars[idx - 1].close)
                    } else {
                        None
                    };
                    let gap = prev_close.filter(|p| *p > 0.0).map(|p| bar.open / p - 1.0);
                    if is_one_word(bar) && gap.is_some_and(|g| g <= -(limit - 0.002)) {
                        continue; // 一字跌停，继续找当日后续 bar
                    }
                    chosen = Some(bar.open);
                    break;
                }
                match chosen {
                    Some(px) => px,
                    None => continue, // 全天一字跌停 -> 顺延到下一交易日
                }
            } else {
                stock.bars[start].open
            };

            let reason = pending_sell.remove(&code).expect("pending_sell 必须存在");
            let Some(pos) = positions.remove(&code) else {
                continue;
            };
            // 冲击成本滑点：按实际卖出金额计算
            let sell_px = open_px * (1.0 - slip(pos.qty as f64 * open_px));
            let proceeds = pos.qty as f64
                * sell_px
                * (1.0 - crate::COMMISSION_RATE - crate::STAMP_TAX_RATE - crate::TRANSFER_RATE);
            let ret = proceeds / pos.cost - 1.0;
            let buy_i = day_idx.get(&pos.buy_date).copied();
            let hold_days = match buy_i {
                Some(bi) => (di - bi) as u32,
                None => days_between(pos.buy_date, day).max(0) as u32,
            };
            trades.push(Trade {
                code: code.clone(),
                buy_date: pos.buy_date,
                sell_date: day,
                buy_price: py_round(pos.entry_price, 3),
                sell_price: py_round(sell_px, 3),
                qty: pos.qty,
                return_pct: py_round(ret * 100.0, 2),
                reason,
                hold_days,
            });
            cash += proceeds;
        }

        // ---- 1) 卖出决策（当日收盘数据）----
        let held_codes: Vec<String> = positions.keys().cloned().collect();
        for code in held_codes {
            let Some(stock) = sim.get(&code) else {
                continue;
            };
            let Some((_, end)) = stock.day_range(day) else {
                continue; // 当日无 bar（停牌）
            };
            let last = stock.bars[end - 1];
            let close = last.close;
            // 复制持仓字段，避免后续 `positions.get_mut` 与不可变借用冲突
            let (entry_price, buy_date, buy_bar_idx, last_check_day) = {
                let pos = positions.get(&code).expect("持仓必须存在");
                (
                    pos.entry_price,
                    pos.buy_date,
                    pos.buy_bar_idx,
                    pos.last_check_day,
                )
            };

            // 买入当日不卖（T+1）
            if buy_date == day {
                continue;
            }

            // 止损（每日收盘检查）
            if cfg.stop_loss > 0.0 && close <= entry_price * (1.0 - cfg.stop_loss) {
                pending_sell.insert(code, "止损".to_string());
                continue;
            }

            // 60min MA13 退出（收盘跌破 13 根 60min 均线）
            if cfg.ma13_exit {
                let i_last = end - 1;
                let lo = i_last.saturating_sub(12);
                // 对齐 Python：pandas float32 Series.mean()（numpy pairwise 求和）
                let closes: Vec<f32> = stock.bars[lo..=i_last].iter().map(|b| b.close as f32).collect();
                let ma13 = crate::precision::mean_f32_numpy(&closes);
                if (close as f32) < ma13 {
                    pending_sell.insert(code, "跌破MA13".to_string());
                    continue;
                }
            }

            // 超时（默认关闭）
            if cfg.max_hold_days > 0 {
                let buy_i = day_idx.get(&buy_date).copied().unwrap_or(di);
                if di - buy_i >= cfg.max_hold_days {
                    pending_sell.insert(code, "超时".to_string());
                    continue;
                }
            }

            // 缠论卖点：Python `day_idx - last_check_day >= recalc_every // 4`
            if (di as i64 - last_check_day as i64) >= (cfg.recalc_every / 4) as i64 {
                if let Some(p) = positions.get_mut(&code) {
                    p.last_check_day = di;
                }
                let i_last = end - 1;
                let win_start = (i_last + 1).saturating_sub(cfg.window.max(1));
                let window_bars = &stock.bars[win_start..=i_last];
                // Python: entry_idx = buy_bar_idx - start（负数时过滤效果等价于 0）
                let entry_raw = buy_bar_idx.saturating_sub(win_start);
                match sells.advise(&code, window_bars, entry_price, entry_raw, cfg) {
                    Ok(advice) => {
                        if advice.top_divergence || advice.break_zd {
                            let note = if advice.note.is_empty() {
                                "缠论卖点".to_string()
                            } else {
                                advice.note
                            };
                            pending_sell.insert(code, note);
                        }
                    }
                    Err(_) => {
                        // Python: except Exception: pass
                        diag.n_sell_advice_errors += 1;
                    }
                }
            }
        }

        // ---- 3) 买入（当日信号）----
        let in_range = {
            let after_start = cfg.start_date.is_none_or(|s| day >= s);
            let before_end = cfg.end_date.is_none_or(|e| day <= e);
            after_start && before_end
        };
        if in_range {
            let mut n_bought = 0usize;
            // 单笔预算：dynpos = 昨日净值×pos_pct（封顶 100 万）；legacy = 初始资金×pos_pct
            let mut capital_px = if cfg.is_dyn_pos() {
                prev_nav * cfg.pos_pct
            } else {
                cfg.capital * cfg.pos_pct
            };
            if cfg.funding_mode == FundingMode::DynPosCap100w {
                capital_px = capital_px.min(BacktestConfig::DYN_POS_CAP);
            }
            // 回撤防护（dd_guard）未实现：Python 默认关闭，不影响基线。
            let _ = peak_nav;

            let day_signals: Vec<ScanSignal> = sig_by_day.get(&day).cloned().unwrap_or_default();
            for s in day_signals {
                let code = s.code;
                let sig = s.sig;

                // 已持仓再次出信号
                if positions.contains_key(&code) {
                    if cfg.skip_held {
                        diag.skipped_held += 1;
                        continue;
                    }
                    diag.n_overwrite += 1;
                }
                if n_bought >= cfg.max_buy_per_day {
                    diag.skipped_perday += 1;
                    continue;
                }
                if positions.len() >= cfg.max_positions {
                    diag.skipped_maxpos += 1;
                    continue;
                }

                let mut budget = capital_px;
                if cfg.max_trade_amt > 0.0 {
                    budget = budget.min(cfg.max_trade_amt);
                }
                // 现金不足：legacy 整笔跳过；dynpos/限价模式用剩余现金缩量
                let new_mode = cfg.is_dyn_pos() || cfg.max_trade_amt > 0.0;
                if cash < budget {
                    if !new_mode {
                        diag.skipped_nofund += 1;
                        continue;
                    }
                    budget = cash;
                }

                let px = sig.price; // 扫描口径 float64 原始价
                if !(px > 0.0) || !px.is_finite() {
                    diag.skipped_nofund += 1;
                    continue;
                }
                // 先用无滑点价估算股数，再按实际金额迭代 2 轮收敛
                let mut qty = ((budget / px / 100.0) as i64) * 100;
                for _ in 0..2 {
                    if qty < 100 {
                        break;
                    }
                    let slip_px = slip(qty as f64 * px);
                    qty = ((budget / (px * (1.0 + slip_px)) / 100.0) as i64) * 100;
                }
                if qty < 100 {
                    diag.skipped_nofund += 1;
                    continue;
                }
                let mut slip_px = slip(qty as f64 * px);
                let mut cost = qty as f64
                    * px
                    * (1.0 + slip_px)
                    * (1.0 + crate::COMMISSION_RATE + crate::TRANSFER_RATE);
                if cost > cash {
                    qty = ((cash / (px * (1.0 + slip_px)) / 100.0) as i64) * 100;
                    if qty < 100 {
                        diag.skipped_nofund += 1;
                        continue;
                    }
                    slip_px = slip(qty as f64 * px);
                    cost = qty as f64
                        * px
                        * (1.0 + slip_px)
                        * (1.0 + crate::COMMISSION_RATE + crate::TRANSFER_RATE);
                }

                // ---- 可成交性 / 相对流动性闸门（用执行 bar 数据，决策时点已知）----
                let mut entry_price = px; // dfc 缺失时 Python 回退为信号价
                if let Some(stock) = sim.get(&code) {
                    if let Some(bar) = stock.bars.get(sig.buy_bar) {
                        entry_price = bar.open;
                        if cfg.gate_tradeability || cfg.gate_liquidity {
                            let bar_amt = if bar.amount.is_nan() { 0.0 } else { bar.amount };
                            let st_flag = st.is_st(&code, day);
                            let limit = board_limit(&code, st_flag);
                            let prev_close = if sig.buy_bar > 0 {
                                Some(stock.bars[sig.buy_bar - 1].close)
                            } else {
                                None
                            };
                            let gap = prev_close.filter(|p| *p > 0.0).map(|p| bar.open / p - 1.0);
                            let mut b_reason: Option<String> = None;
                            let mut b_detail = String::new();

                            if cfg.gate_tradeability {
                                if is_one_word(bar) {
                                    b_reason = Some("一字板".to_string());
                                } else if let Some(g) = gap {
                                    if g >= limit - 0.002 {
                                        b_reason = Some(if st_flag {
                                            "ST涨停开盘".to_string()
                                        } else {
                                            "涨停开盘".to_string()
                                        });
                                    }
                                }
                                if b_reason.is_some() {
                                    b_detail = format!(
                                        "gap={:+.2}% lim={:.0}% one_word={} st={}",
                                        gap.unwrap_or(0.0) * 100.0,
                                        limit * 100.0,
                                        is_one_word(bar),
                                        st_flag
                                    );
                                }
                            }
                            if b_reason.is_none() && cfg.gate_liquidity && bar_amt > 0.0 {
                                let notional = qty as f64 * px;
                                if exceeds_relative_liquidity(notional, bar_amt, cfg.liquidity_pct)
                                {
                                    b_reason = Some(format!(
                                        "相对流动性>{:.0}%",
                                        cfg.liquidity_pct * 100.0
                                    ));
                                    b_detail = format!(
                                        "notional={:.0} bar_amt={:.0} pct={:.1}%",
                                        notional,
                                        bar_amt,
                                        notional / bar_amt * 100.0
                                    );
                                }
                            }
                            if let Some(reason) = b_reason {
                                blocked.push(BlockedSignal {
                                    code: code.clone(),
                                    buy_date: day,
                                    reason,
                                    detail: b_detail,
                                    price: py_round(px, 3),
                                    qty,
                                    notional: py_round(qty as f64 * px, 2),
                                    bar_amount: py_round(bar_amt, 2),
                                    gap_pct: gap.map(|g| py_round(g * 100.0, 3)),
                                    limit_pct: py_round(limit * 100.0, 1),
                                    is_st: st_flag,
                                });
                                continue;
                            }
                        }
                    }
                }

                cash -= cost;
                positions.insert(
                    code,
                    Position {
                        qty,
                        cost,
                        entry_price,
                        buy_date: day,
                        buy_bar_idx: sig.buy_bar,
                        last_check_day: di,
                    },
                );
                n_bought += 1;
            }
        }

        // ---- 4) 当日净值（收盘市值）----
        let mut pos_val = 0.0;
        for (code, pos) in &positions {
            match sim.get(code) {
                None => pos_val += pos.cost,
                Some(stock) => match stock.day_close(day) {
                    None => pos_val += pos.cost, // 停牌按成本计
                    Some(close) => pos_val += pos.qty as f64 * close,
                },
            }
        }
        let nav_today = cash + pos_val;
        nav_series.push(NavPoint {
            date: day,
            nav: nav_today,
        });
        prev_nav = nav_today;
        peak_nav = peak_nav.max(nav_today);
    }

    // ---- 期末估值平仓（非成交）：为把持仓按最后一日收盘价折算成现金 ----
    let final_day = *days.last().expect("days 非空");
    let final_i = days.len() - 1;
    for (code, pos) in std::mem::take(&mut positions) {
        let Some(stock) = sim.get(&code) else {
            continue; // Python: df is None -> continue（不产生估值平仓，也不加现金）
        };
        let px = stock.day_close(final_day).unwrap_or(pos.entry_price);
        // 冲击成本滑点：按实际卖出金额计算
        let sell_px = px * (1.0 - slip(pos.qty as f64 * px));
        let proceeds = pos.qty as f64
            * sell_px
            * (1.0 - crate::COMMISSION_RATE - crate::STAMP_TAX_RATE - crate::TRANSFER_RATE);
        let ret = proceeds / pos.cost - 1.0;
        let buy_i = day_idx.get(&pos.buy_date).copied().unwrap_or(final_i);
        trades.push(Trade {
            code: code.clone(),
            buy_date: pos.buy_date,
            sell_date: final_day,
            buy_price: py_round(pos.entry_price, 3),
            sell_price: py_round(sell_px, 3),
            qty: pos.qty,
            return_pct: py_round(ret * 100.0, 2),
            reason: VALUATION_REASON.to_string(),
            hold_days: (final_i - buy_i) as u32,
        });
        cash += proceeds;
    }
    let _final_cash = cash; // Python 同样加现金但不参与输出；净值用平仓前序列

    diag.n_blocked = blocked.len();
    let stats = compute_stats(&trades, &nav_series);
    Ok(BacktestResult {
        trades,
        nav: nav_series,
        stats,
        blocked,
        diagnostics: diag,
    })
}
