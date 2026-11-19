//! 组合回测撮合主循环集成测试（tempdir 合成 60min 数据仓 + MockSignalSource）。
//!
//! 覆盖任务书验收点：资金/持仓约束、T+1、止损优先于 MA13、一字跌停顺延、停牌顺延、
//! dynpos 预算（复利 + 100 万封顶）、期末估值平仓、P1/P2 闸门、ST 时点表、卖点注入、
//! 统计口径（净值/回撤/分年/利润集中度）。

use std::path::PathBuf;

use chanlun_core::{Date8, SellAdvice};
use chanlun_data::{StoreFormat, store::Store};
use chanlun_engine::sell::{MockSellAdvisor, SellAdvisor};
use chanlun_engine::signals::{MockSignalSource, SignalSource};
use chanlun_engine::slippage::slippage_for;
use chanlun_engine::st::StTable;
use chanlun_engine::{
    BacktestConfig, BacktestResult, EngineError, ExitCheckMode, FundingMode, VALUATION_REASON,
    simulate::run_with,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 合成数据/仓工具
// ---------------------------------------------------------------------------

/// 合成 bar（amount 必须显式给出，P2 闸门依赖）。
#[derive(Clone, Copy, Debug)]
struct K {
    date: Date8,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    amount: f64,
}

fn dates(n: usize) -> Vec<Date8> {
    // 20260101..20260130（测试用固定 1 月内递增日期）
    (1..=n).map(|i| 20260100 + i as u32).collect()
}

/// 平盘 bar：open=close=price，high/low ±1%，流动性充足。
fn flat(date: Date8, price: f64) -> K {
    K {
        date,
        open: price,
        high: price * 1.01,
        low: price * 0.99,
        close: price,
        volume: 1e6,
        amount: price * 1e6,
    }
}

/// 指定开/收盘的 bar。
fn mk(date: Date8, open: f64, close: f64) -> K {
    K {
        date,
        open,
        high: open.max(close) * 1.01,
        low: open.min(close) * 0.99,
        close,
        volume: 1e6,
        amount: open.max(close) * 1e6,
    }
}

/// 一字板 bar（O=H=L=C）。
fn one_word(date: Date8, price: f64) -> K {
    K {
        date,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 1e5,
        amount: price * 1e5,
    }
}

fn write_code(store: &Store, code: &str, bars: &[K]) {
    let mut s = String::from("date,open,high,low,close,volume,amount\n");
    for b in bars {
        s.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            b.date, b.open, b.high, b.low, b.close, b.volume, b.amount
        ));
    }
    std::fs::write(store.path_for(code), s).unwrap();
}

/// 建临时数据仓，返回 (tempdir, store, universe)。
fn make_store(codes: &[(&str, Vec<K>)]) -> (TempDir, Store, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
    let mut universe = Vec::new();
    for (code, bars) in codes {
        write_code(&store, code, bars);
        universe.push((*code).to_string());
    }
    (dir, store, universe)
}

fn base_cfg() -> BacktestConfig {
    let mut cfg = BacktestConfig::default();
    // 默认关掉会干扰时序断言的风控/过滤（各测试按需打开）
    cfg.ma13_exit = false;
    cfg.stop_loss = 0.0;
    cfg.max_amount = 0.0;
    cfg.ma13_entry_filter = 0.0;
    cfg.gate_liquidity = false;
    cfg
}

fn run(
    store: &Store,
    universe: &[String],
    cfg: &BacktestConfig,
    src: &mut MockSignalSource,
) -> BacktestResult {
    let mut adv = MockSellAdvisor::new();
    let st = chanlun_engine::load_st_table(cfg);
    run_with(store, universe, cfg, src, &mut adv, &st).unwrap()
}

fn run_with_advisor(
    store: &Store,
    universe: &[String],
    cfg: &BacktestConfig,
    src: &mut MockSignalSource,
    adv: &mut MockSellAdvisor,
    st: &StTable,
) -> BacktestResult {
    run_with(store, universe, cfg, src, adv, st).unwrap()
}

fn trade_pairs(r: &BacktestResult) -> Vec<(String, Date8, Date8, f64, String)> {
    r.trades
        .iter()
        .map(|t| {
            (
                t.code.clone(),
                t.buy_date,
                t.sell_date,
                t.qty as f64,
                t.reason.clone(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1) T+1 + 止损优先于 MA13 + 一字跌停顺延（全天一字 -> 次日）
// ---------------------------------------------------------------------------

#[test]
fn t1_stop_loss_priority_and_one_word_limit_down_deferral() {
    let days = dates(30);
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    // 买入 bar = 下标 13（date 20260114）
    // 次一交易日收盘 9.4：同时触发止损（<=9.5）与跌破 MA13 -> 必须记为「止损」
    bars[14] = mk(days[14], 9.9, 9.4);
    // 再次日全天一字跌停（9.4*0.9=8.46，gap=-10% <= -9.8%）-> 顺延
    bars[15] = one_word(days[15], 9.4 * 0.9);
    // 再下一日恢复交易 -> 开盘价成交
    bars[16] = mk(days[16], 8.5, 8.6);

    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let mut cfg = base_cfg();
    cfg.stop_loss = 0.05;
    cfg.ma13_exit = true;
    cfg.gate_tradeability = true;
    cfg.capital = 1_000_000.0;

    let mut src = MockSignalSource::new();
    src.push("600000", 13, bars[13].date, bars[13].open);
    let r = run(&store, &universe, &cfg, &mut src);

    assert_eq!(r.trades.len(), 1, "只应有一笔成交");
    let t = &r.trades[0];
    assert_eq!(t.code, "600000");
    assert_eq!(t.buy_date, 20260114);
    // T+1：卖出日必须晚于买入日
    assert!(t.sell_date > t.buy_date, "T+1 违规: {:?}", trade_pairs(&r));
    assert_eq!(t.sell_date, 20260117, "一字跌停应顺延，且在次日开盘成交");
    assert_eq!(t.reason, "止损", "止损优先级必须高于 MA13");
    // qty：legacy 单笔 10 万、价 10、5 万起滑 -> 9900 股
    assert_eq!(t.qty, 9900);
    assert!((t.buy_price - 10.0).abs() < 1e-12);
    // 卖出价 = 8.5 * (1 - slippage_for(9900*8.5))
    let slip = slippage_for(9900.0 * 8.5, 50_000.0, 2_000_000.0, 0.01);
    let expected_sell = 8.5 * (1.0 - slip);
    assert!(
        (t.sell_price - chanlun_engine::stats::round_like_python(expected_sell, 3)).abs() < 1e-9,
        "sell_price={} expected~{}",
        t.sell_price,
        expected_sell
    );
    // hold_days = 交易日历下标差（13 -> 16 共 3 天）
    assert_eq!(t.hold_days, 3);
    // 净值序列 30 天、无负净值
    assert_eq!(r.nav.len(), 30);
    assert!(r.nav.iter().all(|p| p.nav > 0.0));
    // 统计：1 笔真实成交、1 笔亏损、无估值平仓
    assert_eq!(r.stats.n_trades, 1);
    assert_eq!(r.stats.n_mark, 0);
    assert_eq!(r.stats.win_rate, 0.0);
    assert!((r.stats.nav_end - r.nav.last().unwrap().nav).abs() < 1e-9);
    assert!((r.stats.total_ret - (r.stats.nav_end / 1_000_000.0 - 1.0)).abs() < 1e-12);
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 2) MA13 退出（止损关闭时应记为「跌破MA13」）
// ---------------------------------------------------------------------------

#[test]
fn ma13_exit_reason_and_next_open_fill() {
    let days = dates(30);
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    bars[20] = mk(days[20], 9.5, 9.0); // 收盘跌破 13 根均线
    bars[21] = mk(days[21], 9.2, 9.2); // 次日开盘成交

    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let mut cfg = base_cfg();
    cfg.ma13_exit = true;

    let mut src = MockSignalSource::new();
    src.push("600000", 13, bars[13].date, bars[13].open);
    let r = run(&store, &universe, &cfg, &mut src);

    assert_eq!(r.trades.len(), 1);
    let t = &r.trades[0];
    assert_eq!(t.reason, "跌破MA13");
    assert_eq!(t.buy_date, 20260114);
    assert_eq!(t.sell_date, 20260122);
    let slip = slippage_for(9900.0 * 9.2, 50_000.0, 2_000_000.0, 0.01);
    assert!(
        (t.sell_price - chanlun_engine::stats::round_like_python(9.2 * (1.0 - slip), 3)).abs()
            < 1e-9
    );
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 3) 缠论卖点注入（顶背驰/跌破中枢ZD）-> 次日开盘卖；note 为空 -> "缠论卖点"
// ---------------------------------------------------------------------------

#[test]
fn injected_chanlun_sell_point_fills_next_open() {
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let cfg = base_cfg();

    let mut src = MockSignalSource::new();
    src.push("600000", 13, bars[13].date, bars[13].open);

    let mut adv = MockSellAdvisor::new();
    adv.set_break_zd("600000", days[15], "跌破中枢ZD(12.520);");
    let r = run_with_advisor(
        &store,
        &universe,
        &cfg,
        &mut src,
        &mut adv,
        &StTable::empty(),
    );
    assert_eq!(r.trades.len(), 1);
    assert_eq!(r.trades[0].buy_date, 20260114);
    assert_eq!(r.trades[0].sell_date, 20260117, "收盘决策 -> 次日开盘成交");
    assert_eq!(r.trades[0].reason, "跌破中枢ZD(12.520);");
    assert_eq!(r.diagnostics.n_sell_advice_errors, 0);

    // note 为空 -> reason 兜底 "缠论卖点"
    let mut src2 = MockSignalSource::new();
    src2.push("600000", 13, bars[13].date, bars[13].open);
    let mut adv2 = MockSellAdvisor::new();
    adv2.set(
        "600000",
        days[15],
        SellAdvice {
            top_divergence: true,
            break_zd: false,
            note: String::new(),
        },
    );
    let r2 = run_with_advisor(
        &store,
        &universe,
        &cfg,
        &mut src2,
        &mut adv2,
        &StTable::empty(),
    );
    assert_eq!(r2.trades[0].reason, "缠论卖点");
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 4) 停牌（当日无 bar）保留 pending_sell，复牌日开盘卖
// ---------------------------------------------------------------------------

#[test]
fn suspension_keeps_pending_sell_until_resume() {
    let days = dates(30);
    // A：买入后一天触发止损；随后一天停牌（无 bar）；再复牌
    let mut a: Vec<K> = Vec::new();
    for (i, &d) in days.iter().enumerate() {
        match i {
            14 => a.push(mk(d, 9.9, 9.4)), // 收盘触发止损
            15 => {}                       // 停牌：无 bar
            16 => a.push(mk(d, 8.5, 8.6)), // 复牌 -> 开盘卖
            _ => a.push(flat(d, 10.0)),
        }
    }
    // B：仅用于让停牌日的日期仍出现在交易日历里
    let b: Vec<K> = days.iter().map(|&d| flat(d, 5.0)).collect();
    let (dir, store, universe) = make_store(&[("600000", a.clone()), ("600001", b)]);
    let mut cfg = base_cfg();
    cfg.stop_loss = 0.05;

    let mut src = MockSignalSource::new();
    src.push("600000", 13, days[13], 10.0);
    let r = run(&store, &universe, &cfg, &mut src);

    assert_eq!(
        r.trades.len(),
        1,
        "停牌日应保留 pending_sell: {:?}",
        trade_pairs(&r)
    );
    assert_eq!(r.trades[0].reason, "止损");
    assert_eq!(
        r.trades[0].sell_date, 20260117,
        "停牌日不顺延丢单，复牌日开盘卖"
    );
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 5) dynpos 预算：昨日净值 × 10%（复利）+ 100 万封顶；legacy 固定 10 万
// ---------------------------------------------------------------------------

#[test]
fn dynpos_budget_cap_and_compounding() {
    // (a) 100 万封顶：资金 2000 万时 legacy 单笔 200 万 vs dynpos 封顶 100 万
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let mut src = MockSignalSource::new();
    src.push("600000", 13, bars[13].date, bars[13].open);

    let mut legacy = base_cfg();
    legacy.capital = 20_000_000.0;
    let r_legacy = run(&store, &universe, &legacy, &mut src.clone());
    let mut src2 = MockSignalSource::new();
    src2.push("600000", 13, bars[13].date, bars[13].open);
    let mut dynpos = base_cfg();
    dynpos.capital = 20_000_000.0;
    dynpos.funding_mode = FundingMode::DynPosCap100w;
    let r_dyn = run(&store, &universe, &dynpos, &mut src2);

    assert_eq!(r_legacy.trades[0].qty, 198_000, "legacy 200 万预算");
    assert_eq!(r_dyn.trades[0].qty, 99_500, "dynpos 预算封顶 100 万");

    // (b) 复利：第二笔预算按「昨日净值」而非初始资金
    // A：13 买入，2 个交易日超时后在 16 开盘以 12 卖出 -> 净值上涨
    let mut a: Vec<K> = Vec::new();
    for (i, &d) in days.iter().enumerate() {
        match i {
            14 => a.push(mk(d, 10.0, 11.0)),
            15 => a.push(mk(d, 11.0, 12.0)),
            16 => a.push(mk(d, 12.0, 12.0)),
            i if i > 16 => a.push(flat(d, 12.0)),
            _ => a.push(flat(d, 10.0)),
        }
    }
    let b: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (dir2, store2, universe2) = make_store(&[("600000", a.clone()), ("600001", b)]);
    let mut cfg = base_cfg();
    cfg.max_hold_days = 2;
    cfg.funding_mode = FundingMode::DynPosCap100w;

    let mut src3 = MockSignalSource::new();
    src3.push("600000", 13, days[13], 10.0); // 第一笔
    src3.push("600001", 21, days[21], 10.0); // 第二笔（净值已上涨）
    let r = run(&store2, &universe2, &cfg, &mut src3);
    // 成交顺序：A 先卖，B 后买
    assert_eq!(r.trades.len(), 2);
    let qty_a = r.trades[0].qty as f64;
    let qty_b = r.trades[1].qty as f64;
    assert!(
        qty_b > qty_a,
        "dynpos 第二笔预算应随昨日净值复利增长: qty_a={qty_a} qty_b={qty_b}"
    );

    // legacy 下两笔预算相同（固定 10 万）
    let mut cfg_legacy = cfg;
    cfg_legacy.funding_mode = FundingMode::Legacy;
    let mut src4 = MockSignalSource::new();
    src4.push("600000", 13, days[13], 10.0);
    src4.push("600001", 21, days[21], 10.0);
    let r2 = run(&store2, &universe2, &cfg_legacy, &mut src4);
    assert_eq!(r2.trades[0].qty, r2.trades[1].qty, "legacy 固定预算");
    let _ = (dir, dir2);
}

// ---------------------------------------------------------------------------
// 5b) 现金不足：legacy 整笔跳过；dynpos 用剩余现金缩量
// ---------------------------------------------------------------------------

#[test]
fn cash_shortage_legacy_skips_but_dynpos_uses_remaining_cash() {
    let days = dates(20);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let codes: Vec<(&str, Vec<K>)> = (0..12)
        .map(|i| {
            let c = format!("6000{:02}", i);
            (Box::leak(c.into_boxed_str()) as &str, bars.clone())
        })
        .collect();
    let (dir, store, universe) = make_store(&codes);

    let mk_src = || {
        let mut src = MockSignalSource::new();
        for c in &codes {
            src.push(c.0, 13, days[13], 10.0);
        }
        src
    };

    // legacy：单笔 3 万，现金 30 万只够 9 笔；第 10 笔起现金不足整笔跳过
    let mut cfg = base_cfg();
    cfg.capital = 300_000.0;
    cfg.max_buy_per_day = 20;
    cfg.max_positions = 20;
    cfg.funding_mode = FundingMode::Legacy;
    let r = run(&store, &universe, &cfg, &mut mk_src());
    assert_eq!(r.diagnostics.skipped_nofund, 3, "legacy 整笔跳过 3 笔");
    assert_eq!(r.trades.len(), 9, "legacy 只买 9 笔");
    assert!(r.trades.iter().all(|t| t.qty % 100 == 0 && t.qty > 0));

    // dynpos：现金不足时用剩余现金缩量，多买 1 笔（最后 2 笔不足 100 股才跳过）
    let mut cfg2 = cfg;
    cfg2.funding_mode = FundingMode::DynPosCap100w;
    let r2 = run(&store, &universe, &cfg2, &mut mk_src());
    assert_eq!(r2.trades.len(), 10, "dynpos 用剩余现金缩量多买 1 笔");
    assert_eq!(r2.diagnostics.skipped_nofund, 2, "不足 100 股的两笔跳过");
    assert!(r2.trades.iter().all(|t| t.qty % 100 == 0 && t.qty > 0));
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 6) 单日买入上限 / 最大持仓数
// ---------------------------------------------------------------------------

#[test]
fn per_day_and_max_position_limits() {
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let codes = ["600000", "600001", "600002", "600003", "600004"];
    let owned: Vec<(&str, Vec<K>)> = codes.iter().map(|c| (*c, bars.clone())).collect();
    let (dir, store, universe) = make_store(&owned);

    let mk_src = || {
        let mut src = MockSignalSource::new();
        for c in codes {
            src.push(c, 13, days[13], 10.0);
        }
        src
    };

    let mut cfg = base_cfg();
    cfg.max_buy_per_day = 3;
    cfg.max_positions = 10;
    let r = run(&store, &universe, &cfg, &mut mk_src());
    assert_eq!(r.diagnostics.n_signals, 5);
    assert_eq!(r.diagnostics.skipped_perday, 2);
    assert_eq!(r.diagnostics.skipped_maxpos, 0);
    // 3 笔持仓到期末 -> 3 笔估值平仓（非成交）
    assert_eq!(r.trades.len(), 3);
    assert!(r.trades.iter().all(|t| t.reason == VALUATION_REASON));
    assert_eq!(r.stats.n_trades, 0);
    assert_eq!(r.stats.n_mark, 3);

    let mut cfg2 = base_cfg();
    cfg2.max_buy_per_day = 10;
    cfg2.max_positions = 2;
    let r2 = run(&store, &universe, &cfg2, &mut mk_src());
    assert_eq!(r2.diagnostics.skipped_maxpos, 3);
    assert_eq!(r2.trades.len(), 2);
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 7) skip-held：已持仓再次出信号（跳过 vs 覆盖旧仓）
// ---------------------------------------------------------------------------

#[test]
fn skip_held_semantics() {
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let mut src = MockSignalSource::new();
    src.push("600000", 13, days[13], 10.0);
    src.push("600000", 14, days[14], 10.0);

    let mut cfg = base_cfg();
    cfg.skip_held = true;
    let r = run(&store, &universe, &cfg, &mut src.clone());
    assert_eq!(r.diagnostics.skipped_held, 1);
    assert_eq!(r.diagnostics.n_overwrite, 0);
    assert_eq!(r.trades.len(), 1, "只保留第一笔仓位");
    assert_eq!(r.trades[0].reason, VALUATION_REASON);
    let nav_skip = r.stats.nav_end;

    let mut cfg2 = base_cfg();
    cfg2.skip_held = false;
    let r2 = run(&store, &universe, &cfg2, &mut src);
    assert_eq!(r2.diagnostics.skipped_held, 0);
    assert_eq!(r2.diagnostics.n_overwrite, 1);
    assert_eq!(r2.trades.len(), 1, "覆盖旧仓不产生旧仓卖出记录");
    // 覆盖 = 旧仓成本凭空消失 -> 净值显著更低
    assert!(
        r2.stats.nav_end < nav_skip - 90_000.0,
        "覆盖旧仓应造成成本丢失: nav_skip={nav_skip} nav_overwrite={}",
        r2.stats.nav_end
    );
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 8) 期末估值平仓：按最后一日收盘价、非成交、不进统计
// ---------------------------------------------------------------------------

#[test]
fn end_of_period_valuation_liquidation() {
    let days = dates(30);
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    bars[29] = flat(days[29], 12.0); // 期末收盘 12
    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let cfg = base_cfg();
    let mut src = MockSignalSource::new();
    src.push("600000", 13, days[13], 10.0);
    let r = run(&store, &universe, &cfg, &mut src);

    assert_eq!(r.trades.len(), 1);
    let t = &r.trades[0];
    assert_eq!(t.reason, VALUATION_REASON);
    assert_eq!(t.sell_date, *days.last().unwrap());
    // 估值平仓按期末收盘 12 卖出（滑点按 9900*12 计）
    let slip = slippage_for(9900.0 * 12.0, 50_000.0, 2_000_000.0, 0.01);
    let expected = chanlun_engine::stats::round_like_python(12.0 * (1.0 - slip), 3);
    assert!((t.sell_price - expected).abs() < 1e-9);
    // 估值平仓不计入统计
    assert_eq!(r.stats.n_trades, 0);
    assert_eq!(r.stats.n_mark, 1);
    // 期末净值 = 现金 + 持仓市值（用平仓前序列，不扣估值平仓费用）
    let nav_end = r.nav.last().unwrap().nav;
    assert!((r.stats.nav_end - nav_end).abs() < 1e-9);
    assert!(nav_end > 1_000_000.0, "期末持仓按 12 估值应高于成本");
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 9) P1 可成交性闸门：一字板 / 涨停开盘 / ST 涨停开盘
// ---------------------------------------------------------------------------

#[test]
fn gate_p1_one_word_and_limit_up_open() {
    let days = dates(30);

    // (a) 执行 bar 一字板 -> 不成交
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    bars[13] = one_word(days[13], 10.0);
    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let cfg = base_cfg(); // gate_tradeability 默认 true
    let mut src = MockSignalSource::new();
    src.push("600000", 13, days[13], 10.0);
    let r = run(&store, &universe, &cfg, &mut src);
    assert_eq!(r.trades.len(), 0);
    assert_eq!(r.blocked.len(), 1);
    assert_eq!(r.blocked[0].reason, "一字板");
    assert_eq!(r.diagnostics.n_blocked, 1);

    // (b) 主板开盘涨幅 >= 10%-0.2% -> 涨停开盘
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    bars[13] = K {
        date: days[13],
        open: 11.0,
        high: 11.5,
        low: 10.8,
        close: 11.2,
        volume: 1e6,
        amount: 1e8,
    };
    let (_d2, store2, universe2) = make_store(&[("600000", bars.clone())]);
    let mut src2 = MockSignalSource::new();
    src2.push("600000", 13, days[13], 11.0);
    let r2 = run(&store2, &universe2, &cfg, &mut src2);
    assert_eq!(r2.blocked.len(), 1);
    assert_eq!(r2.blocked[0].reason, "涨停开盘");
    assert!((r2.blocked[0].limit_pct - 10.0).abs() < 1e-9);

    // (c) ST 时点表：ST 涨停幅 5%，开盘 +4.9% 即买不进
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    bars[13] = K {
        date: days[13],
        open: 10.5,
        high: 10.8,
        low: 10.4,
        close: 10.6,
        volume: 1e6,
        amount: 1e8,
    };
    let (_d3, store3, universe3) = make_store(&[("600000", bars.clone())]);
    let st_json = PathBuf::from(dir.path()).join("st_flags.json");
    std::fs::write(
        &st_json,
        r#"{"stocks":{"sh_600000":{"st":[[20260101,20260131]]}}}"#,
    )
    .unwrap();
    let mut cfg_st = base_cfg();
    cfg_st.st_flags_path = Some(st_json);
    let mut src3 = MockSignalSource::new();
    src3.push("600000", 13, days[13], 10.5);
    let r3 = run(&store3, &universe3, &cfg_st, &mut src3);
    assert_eq!(r3.blocked.len(), 1);
    assert_eq!(r3.blocked[0].reason, "ST涨停开盘");
    assert!(r3.blocked[0].is_st);
    assert!((r3.blocked[0].limit_pct - 5.0).abs() < 1e-9);
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 10) P2 相对流动性闸门
// ---------------------------------------------------------------------------

#[test]
fn gate_p2_relative_liquidity() {
    let days = dates(30);
    // 执行 bar 成交额只有 10 万；单笔 ~9.9 万 > 5%*10 万 = 5000 -> 不成交
    let mut bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    bars[13] = K {
        date: days[13],
        open: 10.0,
        high: 10.1,
        low: 9.9,
        close: 10.0,
        volume: 1e4,
        amount: 100_000.0,
    };
    let (dir, store, universe) = make_store(&[("600000", bars.clone())]);
    let mut cfg = base_cfg();
    cfg.gate_liquidity = true;
    cfg.liquidity_pct = 0.05;
    let mut src = MockSignalSource::new();
    src.push("600000", 13, days[13], 10.0);
    let r = run(&store, &universe, &cfg, &mut src);
    assert_eq!(r.trades.len(), 0);
    assert_eq!(r.blocked.len(), 1);
    assert_eq!(r.blocked[0].reason, "相对流动性>5%");

    // 关闭闸门 -> 正常成交
    let mut cfg2 = cfg;
    cfg2.gate_liquidity = false;
    let mut src2 = MockSignalSource::new();
    src2.push("600000", 13, days[13], 10.0);
    let r2 = run(&store, &universe, &cfg2, &mut src2);
    assert_eq!(r2.trades.len(), 1);
    assert!(r2.blocked.is_empty());
    let _ = dir;
}

// ---------------------------------------------------------------------------
// 11) 其他：Every60MinBar 未实现 / universe 为空回退 list_symbols / 无信号空结果
// ---------------------------------------------------------------------------

#[test]
fn every_60min_bar_is_explicitly_unimplemented() {
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (_dir, store, universe) = make_store(&[("600000", bars)]);
    let mut cfg = base_cfg();
    cfg.exit_check = ExitCheckMode::Every60MinBar;
    let mut src = MockSignalSource::new();
    let mut adv = MockSellAdvisor::new();
    let err = run_with(
        &store,
        &universe,
        &cfg,
        &mut src,
        &mut adv,
        &StTable::empty(),
    )
    .unwrap_err();
    assert!(matches!(err, EngineError::Unimplemented(_)));
}

#[test]
fn empty_universe_falls_back_to_store_symbols() {
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (dir, store, _universe) = make_store(&[("600000", bars.clone())]);
    let cfg = base_cfg();
    let mut src = MockSignalSource::new();
    src.push("600000", 13, days[13], 10.0);
    let r = run(&store, &[], &cfg, &mut src);
    assert_eq!(r.trades.len(), 1);
    let _ = dir;
}

#[test]
fn no_signals_returns_empty_result() {
    let days = dates(30);
    let bars: Vec<K> = days.iter().map(|&d| flat(d, 10.0)).collect();
    let (dir, store, universe) = make_store(&[("600000", bars)]);
    let cfg = base_cfg();
    let mut src = MockSignalSource::new();
    let r = run(&store, &universe, &cfg, &mut src);
    assert!(r.trades.is_empty());
    assert!(r.nav.is_empty());
    assert_eq!(r.diagnostics.n_signals, 0);
    let _ = dir;
}

#[test]
fn mock_signal_source_exposes_signals_for_golden_reuse() {
    // 确保 trait 对象可用（golden/其他 crate 可替换实现）
    let mut src = MockSignalSource::new();
    src.push("600000", 3, 20260104, 10.0);
    let dyn_src: &mut dyn SignalSource = &mut src;
    let got = dyn_src
        .scan("600000", &[], &BacktestConfig::default())
        .unwrap();
    assert_eq!(got.len(), 1);
    let _dyn_adv: &mut dyn SellAdvisor = &mut MockSellAdvisor::new();
}
