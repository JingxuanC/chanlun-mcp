//! G-2 golden 对拍骨架：同一 60min 数据仓上，Python `backtest_portfolio.py` vs Rust 引擎。
//!
//! Python 侧产物由 `scripts/golden_engine.py` 生成（它只读调用参考实现并落 JSON）：
//!
//! ```bash
//! # 1) 生成产物（真实数据仓或 --synthetic）
//! python scripts/golden_engine.py --data-dir <60min仓> --out-dir /tmp/chanlun-golden-engine \
//!     --case legacy,dynpos
//!
//! # 2) Rust 侧比对（本测试默认 #[ignore]）
//! export CHANLUN_GOLDEN_DIR=/tmp/chanlun-golden-engine
//! export CHANLUN_GOLDEN_CASE=legacy           # 或 dynpos
//! export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo-home}"
//! cargo test -p chanlun-engine --test golden_engine -- --ignored --nocapture
//! ```
//!
//! 验收口径（docs/plan.md M3/G-2）：逐笔成交一致（code/date/price/qty/reason）、
//! 净值逐日偏差 < 1e-6。
//!
//! 已用合成数据验证通过（2026-11-17，core 实现就绪后）：
//! - legacy / dynpos：17 笔逐笔一致、nav 最大相对偏差 1.17e-16；
//! - 仅缠论卖点（`--ma13-exit 0 --stop-loss 0`）：顶背驰/跌破中枢ZD 的 reason 文案也逐字一致；
//! - 期末估值平仓：截断数据造出持仓过期末，`估值平仓（非成交）` 一致、nav 一致。
//! 真实数据仓的 G-2 由 Lead 在 core/data 定稿后跑一次（命令见上）。
//!
//! 三个测试：
//! 1. `golden_artifacts_schema_compatible`：不依赖 core，校验 Python 产物与 spec 契约；
//! 2. `golden_mock_signal_parity_without_core`：复用 Python `signals.json` 作为 MockSignalSource
//!    只跑撮合主循环（该 case 无缠论卖点时有效），可在 core 未就绪时定位撮合/成本/仓位/净值差异；
//! 3. `golden_engine_python_parity`（`#[ignore]`）：全链路 `run()`（core 信号 + 缠论卖点）。
//! - 卖点 reason 形如 `跌破中枢ZD(12.520);`，末位浮点格式由 core 文案决定；比较时对
//!   「顶背驰 / 跌破中枢ZD」只比分类前缀（逐笔 date/price/qty/code 仍严格一致），
//!   其余 reason（止损/跌破MA13/超时/估值平仓）严格相等。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chanlun_data::{StoreFormat, store::Store};
use chanlun_engine::sell::MockSellAdvisor;
use chanlun_engine::signals::MockSignalSource;
use chanlun_engine::{
    BacktestConfig, Stats, Trade, VALUATION_REASON, load_st_table, run, simulate::run_with,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GoldenSpec {
    data_dir: String,
    config: BacktestConfig,
    universe: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TradesFile {
    trades: Vec<Trade>,
}

#[derive(Debug, Deserialize)]
struct SignalsFile {
    signals: BTreeMap<String, Vec<Signal>>,
}

#[derive(Debug, Clone, Deserialize)]
struct Signal {
    buy_bar: usize,
    buy_date: u32,
    price: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PyStats {
    years: f64,
    cagr: f64,
    max_dd: f64,
    nav_end: f64,
    n_trades_real: usize,
    n_mark: usize,
    win_rate: f64,
    avg_ret: f64,
    median_ret: f64,
    avg_win: f64,
    avg_lose: f64,
    pl_ratio: f64,
    profit_factor: f64,
    hold_avg: f64,
    hold_med: f64,
    top5pct_profit_share: f64,
    top1pct_profit_share: f64,
}

#[derive(Debug, Deserialize)]
struct NavFile {
    nav: Vec<NavPoint>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct NavPoint {
    date: u32,
    nav: f64,
}

fn golden_dir() -> Option<PathBuf> {
    std::env::var_os("CHANLUN_GOLDEN_DIR").map(PathBuf::from)
}

fn case_name() -> String {
    std::env::var("CHANLUN_GOLDEN_CASE").unwrap_or_else(|_| "legacy".to_string())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("解析 {} 失败: {e}", path.display()))
}

/// 校验 Python 产物与 Rust spec 的 schema 兼容（不需要 core，可在 core 移植完成前先跑）。
#[test]
fn golden_artifacts_schema_compatible() {
    let Some(root) = golden_dir() else {
        eprintln!("[golden] 未设置 CHANLUN_GOLDEN_DIR，跳过（骨架测试）");
        return;
    };
    let dir = root.join(case_name());
    let spec_path = dir.join("spec.json");
    if !spec_path.exists() {
        eprintln!(
            "[golden] {} 不存在，先运行 scripts/golden_engine.py；跳过",
            spec_path.display()
        );
        return;
    }
    let spec: GoldenSpec = read_json(&spec_path).expect("spec.json 解析失败");
    assert!(!spec.universe.is_empty(), "universe 为空");
    assert!(
        Path::new(&spec.data_dir).is_dir(),
        "spec.data_dir 不存在: {}",
        spec.data_dir
    );
    let trades: TradesFile = read_json(&dir.join("trades.json")).expect("trades.json 解析失败");
    let nav: NavFile = read_json(&dir.join("nav.json")).expect("nav.json 解析失败");
    for t in &trades.trades {
        assert_eq!(t.code.len(), 6, "code 未补齐 6 位: {}", t.code);
        assert!(t.buy_date >= 19900101 && t.buy_date <= 21001231);
        assert!(t.qty > 0 && t.qty % 100 == 0, "qty 非整手: {}", t.qty);
    }
    if !nav.nav.is_empty() {
        let mut prev = 0u32;
        for p in &nav.nav {
            assert!(p.date > prev, "nav 日期未严格升序: {} -> {}", prev, p.date);
            prev = p.date;
        }
    }
    eprintln!(
        "[golden] schema OK: case={} universe={} trades={} nav={}",
        case_name(),
        spec.universe.len(),
        trades.trades.len(),
        nav.nav.len()
    );
}

/// G-2 主对拍：Rust 引擎 vs Python 参考产物（逐笔 + 逐日净值）。
#[test]
#[ignore = "需要 core 实现 + scripts/golden_engine.py 产物（CHANLUN_GOLDEN_DIR）；由 Lead 在真实数据仓启用"]
fn golden_engine_python_parity() {
    let Some(root) = golden_dir() else {
        eprintln!("[golden] 未设置 CHANLUN_GOLDEN_DIR，跳过");
        return;
    };
    let case = case_name();
    let dir = root.join(&case);
    let spec: GoldenSpec =
        read_json(&dir.join("spec.json")).expect("spec.json 缺失：先运行 scripts/golden_engine.py");
    let expected: TradesFile = read_json(&dir.join("trades.json")).expect("trades.json 缺失");
    let expected_nav: NavFile = read_json(&dir.join("nav.json")).expect("nav.json 缺失");
    let expected_stats: PyStats = read_json(&dir.join("stats.json")).unwrap_or_default();

    let data_dir = std::env::var("CHANLUN_GOLDEN_DATA").unwrap_or_else(|_| spec.data_dir.clone());
    let store = Store::open(&data_dir, StoreFormat::Csv).expect("打开数据仓失败");
    eprintln!(
        "[golden] case={} data_dir={} universe={} python_trades={}",
        case,
        data_dir,
        spec.universe.len(),
        expected.trades.len()
    );

    let result = run(&store, &spec.universe, &spec.config).expect("Rust 引擎回测失败");

    let expected_fills = split_fills(&expected.trades);
    let actual_fills = split_fills(&result.trades);
    let mut errors: Vec<String> = Vec::new();

    compare_fills(&case, &expected_fills, &actual_fills, &mut errors);
    compare_nav(&expected_nav.nav, &result.nav, &mut errors);
    compare_stats(&case, &expected_stats, &result.stats, &mut errors);

    // 估值平仓：数量与关键字段（卖出日/价格）一致
    if expected_fills.1.len() != actual_fills.1.len() {
        errors.push(format!(
            "估值平仓笔数不一致: python={} rust={}",
            expected_fills.1.len(),
            actual_fills.1.len()
        ));
    }
    for (e, a) in expected_fills.1.iter().zip(actual_fills.1.iter()) {
        if e.code != a.code || e.buy_date != a.buy_date {
            errors.push(format!(
                "估值平仓标的/买入日不一致: python={}@{} rust={}@{}",
                e.code, e.buy_date, a.code, a.buy_date
            ));
        }
    }

    // 统计粗校验
    if result.stats.n_trades != expected_fills.0.len() {
        errors.push(format!(
            "stats.n_trades={} != python fills={}",
            result.stats.n_trades,
            expected_fills.0.len()
        ));
    }
    eprintln!(
        "[golden] Rust: fills={} marks={} nav_days={} nav_end={:.6}",
        actual_fills.0.len(),
        actual_fills.1.len(),
        result.nav.len(),
        result.stats.nav_end
    );
    if !expected_nav.nav.is_empty() {
        let py_end = expected_nav.nav.last().unwrap().nav;
        let rel = (result.stats.nav_end - py_end).abs() / py_end.abs().max(1.0);
        eprintln!(
            "[golden] nav_end: python={py_end:.6} rust={:.6} rel_diff={rel:.3e}",
            result.stats.nav_end
        );
    }

    if !errors.is_empty() {
        panic!(
            "G-2 对拍失败：{} 处差异\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    eprintln!("[golden] ✅ G-2 对拍通过：逐笔成交一致、逐日净值偏差 < 1e-6");
}

/// **不需要 core 的端到端 parity**：复用 Python 产出的 signals.json 作为 MockSignalSource，
/// 在 Rust 里只跑撮合主循环，与 Python 的逐笔成交/净值对拍。
///
/// 适用条件：该 case 的 Python 成交里没有缠论卖点（顶背驰/跌破中枢ZD）。合成数据通常满足
/// （MA13 先触发）；真实数据大概率含缠论卖点 -> 本测试打印原因后跳过，等 core 完成后用
/// `golden_engine_python_parity` 全量对拍。
#[test]
fn golden_mock_signal_parity_without_core() {
    let Some(root) = golden_dir() else {
        eprintln!("[golden] 未设置 CHANLUN_GOLDEN_DIR，跳过 mock-signal parity");
        return;
    };
    let case = case_name();
    let dir = root.join(&case);
    let signals_path = dir.join("signals.json");
    if !signals_path.exists() {
        eprintln!(
            "[golden] {} 不存在；用 scripts/golden_engine.py --dump-signals 生成后启用",
            signals_path.display()
        );
        return;
    }
    let spec: GoldenSpec = read_json(&dir.join("spec.json")).expect("spec.json");
    let expected: TradesFile = read_json(&dir.join("trades.json")).expect("trades.json");
    let expected_nav: NavFile = read_json(&dir.join("nav.json")).expect("nav.json");
    let signals: SignalsFile = read_json(&signals_path).expect("signals.json");
    let expected_stats: PyStats = read_json(&dir.join("stats.json")).unwrap_or_default();

    let expected_fills = split_fills(&expected.trades);
    let has_chanlun_sell = expected_fills.0.iter().any(|t| {
        matches!(
            reason_class(&t.reason),
            "顶背驰" | "跌破中枢ZD" | "缠论卖点"
        )
    });
    if has_chanlun_sell {
        eprintln!(
            "[golden] case={case} 含缠论卖点成交（需 core 的 latest_sell_advice 才能复现），跳过 mock-signal parity"
        );
        return;
    }

    let data_dir = std::env::var("CHANLUN_GOLDEN_DATA").unwrap_or_else(|_| spec.data_dir.clone());
    let store = Store::open(&data_dir, StoreFormat::Csv).expect("打开数据仓失败");
    let mut src = MockSignalSource::new();
    let mut n_sig = 0usize;
    for (code, sigs) in &signals.signals {
        for s in sigs {
            src.push(code, s.buy_bar, s.buy_date, s.price);
            n_sig += 1;
        }
    }
    let st = load_st_table(&spec.config);
    let mut adv = MockSellAdvisor::new(); // 无缠论卖点 -> 不会触发（见上方条件）
    let result = run_with(
        &store,
        &spec.universe,
        &spec.config,
        &mut src,
        &mut adv,
        &st,
    )
    .expect("mock-signal 回测失败");

    let actual_fills = split_fills(&result.trades);
    let mut errors: Vec<String> = Vec::new();
    compare_fills(&case, &expected_fills, &actual_fills, &mut errors);
    compare_nav(&expected_nav.nav, &result.nav, &mut errors);
    compare_stats(&case, &expected_stats, &result.stats, &mut errors);

    eprintln!(
        "[golden] mock-signal parity: case={case} universe={} signals={} python_fills={} rust_fills={}",
        spec.universe.len(),
        n_sig,
        expected_fills.0.len(),
        actual_fills.0.len()
    );
    if !errors.is_empty() {
        panic!(
            "mock-signal parity 失败：{} 处差异\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    eprintln!("[golden] ✅ mock-signal parity 通过（撮合/成本/闸门/仓位/净值与 Python 一致）");
}

fn split_fills(trades: &[Trade]) -> (Vec<&Trade>, Vec<&Trade>) {
    let mut fills = Vec::new();
    let mut marks = Vec::new();
    for t in trades {
        if t.reason == VALUATION_REASON {
            marks.push(t);
        } else {
            fills.push(t);
        }
    }
    fills.sort_by_key(|t| (t.buy_date, t.sell_date, t.code.clone()));
    marks.sort_by_key(|t| (t.buy_date, t.code.clone()));
    (fills, marks)
}

fn reason_class(reason: &str) -> &str {
    if reason.starts_with("跌破中枢ZD") {
        "跌破中枢ZD"
    } else if reason.contains("顶背驰") {
        "顶背驰"
    } else {
        reason
    }
}

fn close_enough(a: f64, b: f64) -> bool {
    let tol = 1e-6 * b.abs().max(1.0);
    (a - b).abs() <= tol
}

fn compare_fills(
    case: &str,
    expected: &(Vec<&Trade>, Vec<&Trade>),
    actual: &(Vec<&Trade>, Vec<&Trade>),
    errors: &mut Vec<String>,
) {
    let (exp, act) = (&expected.0, &actual.0);
    if exp.len() != act.len() {
        errors.push(format!(
            "[{case}] 成交笔数不一致: python={} rust={}",
            exp.len(),
            act.len()
        ));
    }
    for (i, (e, a)) in exp.iter().zip(act.iter()).enumerate() {
        let tag = format!(
            "[{case}] #{i} py={}@{}->{} rust={}@{}->{}",
            e.code, e.buy_date, e.sell_date, a.code, a.buy_date, a.sell_date
        );
        if e.code != a.code {
            errors.push(format!("{tag} code 不一致"));
        }
        if e.buy_date != a.buy_date || e.sell_date != a.sell_date {
            errors.push(format!("{tag} 日期不一致"));
        }
        if e.qty != a.qty {
            errors.push(format!("{tag} qty 不一致: {} vs {}", e.qty, a.qty));
        }
        if !close_enough(e.buy_price, a.buy_price) {
            errors.push(format!(
                "{tag} buy_price 不一致: {:.6} vs {:.6}",
                e.buy_price, a.buy_price
            ));
        }
        if !close_enough(e.sell_price, a.sell_price) {
            errors.push(format!(
                "{tag} sell_price 不一致: {:.6} vs {:.6}",
                e.sell_price, a.sell_price
            ));
        }
        if reason_class(&e.reason) != reason_class(&a.reason) {
            errors.push(format!(
                "{tag} reason 不一致: {:?} vs {:?}",
                e.reason, a.reason
            ));
        } else if e.reason != a.reason {
            eprintln!(
                "[golden] note 文案差异（分类一致，仅记录）: py={:?} rust={:?}",
                e.reason, a.reason
            );
        }
        if e.hold_days != a.hold_days {
            errors.push(format!(
                "{tag} hold_days 不一致: {} vs {}",
                e.hold_days, a.hold_days
            ));
        }
    }
}

fn compare_stats(case: &str, expected: &PyStats, actual: &Stats, errors: &mut Vec<String>) {
    if expected.n_trades_real == 0 && expected.nav_end == 0.0 {
        return; // 无信号/空 case：无统计可比
    }
    if expected.n_trades_real != actual.n_trades {
        errors.push(format!(
            "[{case}] stats.n_trades: python={} rust={}",
            expected.n_trades_real, actual.n_trades
        ));
    }
    if expected.n_mark != actual.n_mark {
        errors.push(format!(
            "[{case}] stats.n_mark: python={} rust={}",
            expected.n_mark, actual.n_mark
        ));
    }
    let checks: [(&str, f64, f64); 14] = [
        ("years", expected.years, actual.years),
        ("cagr", expected.cagr, actual.cagr),
        ("max_dd", expected.max_dd, actual.max_dd),
        ("win_rate", expected.win_rate, actual.win_rate),
        ("avg_ret", expected.avg_ret, actual.avg_ret),
        ("median_ret", expected.median_ret, actual.median_ret),
        ("avg_win", expected.avg_win, actual.avg_win),
        ("avg_lose", expected.avg_lose, actual.avg_lose),
        ("pl_ratio", expected.pl_ratio, actual.pl_ratio),
        (
            "profit_factor",
            expected.profit_factor,
            actual.profit_factor,
        ),
        ("hold_avg", expected.hold_avg, actual.hold_avg),
        ("hold_med", expected.hold_med, actual.hold_med),
        (
            "top5pct_profit_share",
            expected.top5pct_profit_share,
            actual.top5pct_profit_share,
        ),
        (
            "top1pct_profit_share",
            expected.top1pct_profit_share,
            actual.top1pct_profit_share,
        ),
    ];
    for (name, py, rs) in checks {
        if (py - rs).abs() > 1e-9 * py.abs().max(1.0) {
            errors.push(format!(
                "[{case}] stats.{name}: python={py:.12} rust={rs:.12}"
            ));
        }
    }
    let rel = (expected.nav_end - actual.nav_end).abs() / expected.nav_end.abs().max(1.0);
    if rel > 1e-9 {
        errors.push(format!(
            "[{case}] stats.nav_end: python={:.6} rust={:.6} rel={rel:.3e}",
            expected.nav_end, actual.nav_end
        ));
    }
}

fn compare_nav(
    expected: &[NavPoint],
    actual: &[chanlun_engine::NavPoint],
    errors: &mut Vec<String>,
) {
    if expected.len() != actual.len() {
        errors.push(format!(
            "净值天数不一致: python={} rust={}",
            expected.len(),
            actual.len()
        ));
    }
    let mut actual_map: BTreeMap<u32, f64> = BTreeMap::new();
    for p in actual {
        actual_map.insert(p.date, p.nav);
    }
    let mut worst = 0.0f64;
    let mut worst_date = 0u32;
    for p in expected {
        let Some(&a) = actual_map.get(&p.date) else {
            errors.push(format!("净值缺日期: python={} 有、rust 无", p.date));
            continue;
        };
        let rel = (a - p.nav).abs() / p.nav.abs().max(1.0);
        if rel > worst {
            worst = rel;
            worst_date = p.date;
        }
        if rel > 1e-6 {
            errors.push(format!(
                "净值偏差超限 date={} python={:.9} rust={:.9} rel={:.3e}",
                p.date, p.nav, a, rel
            ));
            if errors.len() > 50 {
                return;
            }
        }
    }
    eprintln!("[golden] 净值最大相对偏差 {worst:.3e} @ {worst_date}（阈值 1e-6）");
}
