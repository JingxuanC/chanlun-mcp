//! # chanlun-engine
//!
//! 组合回测引擎，逐行对齐 Python `code/backtest_portfolio.py`（862 行）。
//! 单资金池、单日最多 3 买、最多 10 仓、单笔 10% 资金（或 dynpos 复利封顶 100 万）。
//!
//! ## 必须复刻的时序（无前视）
//!
//! ```text
//! bar g      : 底分型处产生 B2 候选
//! bar g+4    : 截断到 g+4 重算，g 仍存在 -> 确认（confirm_delay=4, confirm_k=1）
//! 确认 bar 收盘: Phase B（MA13 乖离 > +1% 丢弃）/ Phase C（amt20 >= 5000 万丢弃）
//! bar g+5 开盘: 以该 bar 开盘价买入（可成交性 P1 + 相对流动性 P2 闸门）
//! 每交易日收盘 : 卖出判定（止损 / 跌破 MA13 / 超时 / 缠论卖点），挂次日开盘卖
//! 次日首根 bar : 以开盘价卖出（一字跌停顺延至当日首个非一字跌停 bar）
//! ```
//!
//! ⚠️ **文档与代码不一致（口径以 Python 代码为准）**：文档 §4.4/§7.1 写"每根 60min bar 收盘
//! 判定卖出"，但 Python 代码实际是**每个交易日最后一根 bar 收盘**判一次
//! （`backtest_portfolio.py:525-568`，源码注释也自认"简化为每日一次"）。
//! 本 crate 按 **Python 行为**移植（`ExitCheckMode::DailyClose`，保持与 518.79 万基线可对拍）；
//! `ExitCheckMode::Every60MinBar` 只保留口径开关，**当前返回 `Unimplemented`**。
//!
//! ## 架构：信号/卖点可注入
//!
//! core（笔/中枢/买卖点）由 `core-porter` 并行移植，引擎不阻塞：
//!
//! - 信号：[`signals::SignalSource`]（生产 [`signals::ChanlunSignalSource`]，测试
//!   [`signals::MockSignalSource`]）；
//! - 缠论卖点：[`sell::SellAdvisor`]（生产 [`sell::ChanlunSellAdvisor`]，测试
//!   [`sell::MockSellAdvisor`]）。
//!
//! 撮合/成本/闸门/仓位/统计完全不依赖 core，可用 Mock 全覆盖单测。
//!
//! ## 与 Python 的已知口径差异（刻意保留，见完成报告）
//!
//! - Phase D（ST/次新/小市值全域过滤）在 MCP 层做，引擎不实现；
//! - ATR 波动率仓位（`--risk-per-trade`）、回撤防护（`--dd-guard`）未纳入配置；
//! - 快照模式（`--snapshot`）不实现（MCP 跟单卡另算）。

pub mod gates;
pub mod precision;
pub mod sell;
pub mod signals;
pub mod simulate;
pub mod slippage;
pub mod st;
pub mod stats;

use std::path::PathBuf;

use chanlun_core::{AnalyzerConfig, Date8, DivergenceMode};
use serde::{Deserialize, Serialize};

use crate::sell::ChanlunSellAdvisor;
use crate::signals::ChanlunSignalSource;
pub use st::StTable;

pub const COMMISSION_RATE: f64 = 0.000_085;
pub const STAMP_TAX_RATE: f64 = 0.000_5;
pub const TRANSFER_RATE: f64 = 0.000_01;

/// 期末估值平仓的 reason（非成交，不计入统计）。
pub const VALUATION_REASON: &str = "估值平仓（非成交）";

/// 资金口径（两套不可横比）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingMode {
    /// 固定单笔 = 初始资金 × 10%
    Legacy,
    /// 复利：昨日净值 × 10%，封顶 100 万
    DynPosCap100w,
}

/// 卖出判定频率口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitCheckMode {
    /// Python 生产行为：每个交易日最后一根 bar 收盘判一次（对拍基线）
    DailyClose,
    /// 文档描述的 60min bar 级（**尚未实现**，切换会返回 `Unimplemented`）
    Every60MinBar,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BacktestConfig {
    pub capital: f64,
    pub funding_mode: FundingMode,
    pub max_buy_per_day: usize,
    pub max_positions: usize,
    pub pos_pct: f64,
    pub stop_loss: f64,
    pub ma13_exit: bool,
    pub max_hold_days: usize,
    pub confirm_delay: usize,
    pub confirm_k: usize,
    pub recalc_every: usize,
    pub ma13_entry_filter: f64,
    pub max_amount: f64,
    pub window: usize,
    pub dynamic_pos: bool,
    pub max_trade_amt: f64,
    pub skip_held: bool,
    pub gate_tradeability: bool,
    pub gate_liquidity: bool,
    pub liquidity_pct: f64,
    pub slippage_base_amt: f64,
    pub slippage_cap_amt: f64,
    pub slippage_max: f64,
    pub start_date: Option<Date8>,
    pub end_date: Option<Date8>,
    pub exit_check: ExitCheckMode,
    /// 时点 ST 表 JSON（`st_flags.json`）；`None` / 加载失败 → 全部按非 ST。
    pub st_flags_path: Option<PathBuf>,
    pub analyzer: AnalyzerConfig,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            capital: 1_000_000.0,
            funding_mode: FundingMode::Legacy,
            max_buy_per_day: 3,
            max_positions: 10,
            pos_pct: 0.10,
            stop_loss: 0.05,
            ma13_exit: true,
            max_hold_days: 0,
            confirm_delay: 4,
            confirm_k: 1,
            recalc_every: 4,
            ma13_entry_filter: 0.01,
            max_amount: 50_000_000.0,
            window: 1500,
            dynamic_pos: false,
            max_trade_amt: 0.0,
            skip_held: true,
            gate_tradeability: true,
            gate_liquidity: true,
            liquidity_pct: 0.05,
            slippage_base_amt: 50_000.0,
            slippage_cap_amt: 2_000_000.0,
            slippage_max: 0.01,
            start_date: None,
            end_date: None,
            exit_check: ExitCheckMode::DailyClose,
            st_flags_path: None,
            analyzer: AnalyzerConfig {
                divergence_mode: DivergenceMode::Adjacent,
                break_zd_confirm: false,
                ..AnalyzerConfig::default()
            },
        }
    }
}

impl BacktestConfig {
    /// dynpos 单笔预算封顶（100 万）。
    pub const DYN_POS_CAP: f64 = 1_000_000.0;

    /// 是否复利（dynpos）口径。
    pub fn is_dyn_pos(&self) -> bool {
        self.funding_mode == FundingMode::DynPosCap100w || self.dynamic_pos
    }
}

/// 一笔成交（估值平仓用 `reason = "估值平仓（非成交）"` 表示，不计入统计）。
///
/// `buy_price` / `sell_price` / `return_pct` 按 Python 规则保留 3 / 3 / 2 位小数，
/// 这样与 Python 导出的交易 CSV、以及 `_gen_doc_stats.py` 的统计口径一致。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub code: String,
    pub buy_date: Date8,
    pub sell_date: Date8,
    pub buy_price: f64,
    pub sell_price: f64,
    pub qty: i64,
    pub return_pct: f64,
    pub reason: String,
    pub hold_days: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NavPoint {
    pub date: Date8,
    pub nav: f64,
}

/// 绩效统计（对应 `stats.json` 字段，另补文档要求的扩展项）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    // ---- 净值 ----
    pub nav0: f64,
    pub nav_end: f64,
    pub total_ret: f64,
    pub cagr: f64,
    pub max_dd: f64,
    pub dd_peak_date: Date8,
    pub dd_trough_date: Date8,
    pub years: f64,
    pub days: usize,
    pub sharpe: f64,
    pub sortino: f64,
    pub calmar: f64,
    pub month_win: f64,
    // ---- 成交 ----
    pub n_trades: usize,
    /// 估值平仓笔数（非成交）
    pub n_mark: usize,
    pub n_codes: usize,
    pub win_rate: f64,
    pub avg_ret: f64,
    pub median_ret: f64,
    pub avg_win: f64,
    pub avg_lose: f64,
    pub pl_ratio: f64,
    pub profit_factor: f64,
    pub expectancy: f64,
    pub hold_avg: f64,
    pub hold_med: f64,
    pub best: f64,
    pub worst: f64,
    pub best_code: String,
    pub best_range: String,
    pub worst_code: String,
    // ---- 集中度（review-notes §9：收益右尾依赖，必须暴露）----
    /// 前 5% 交易利润占全部交易净利润比例
    pub top5pct_profit_share: f64,
    /// 前 1% 交易利润占全部交易净利润比例
    pub top1pct_profit_share: f64,
    // ---- 分组 ----
    pub yearly: Vec<YearlyStat>,
    pub exits: Vec<ExitStat>,
    pub top_drawdowns: Vec<DrawdownSegment>,
    pub years_pos: usize,
    pub n_years: usize,
}

/// 分年收益（年收益以上年末净值为基准，首年用年内首日）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct YearlyStat {
    pub year: u32,
    pub ret: f64,
    pub nav_end: f64,
    pub trades: usize,
}

/// 离场原因分类统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExitStat {
    pub reason: String,
    pub n: usize,
    pub avg_ret: f64,
    pub win_rate: f64,
}

/// 前 N 大回撤段（峰到谷）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DrawdownSegment {
    pub peak: Date8,
    pub trough: Date8,
    pub dd: f64,
    pub days: usize,
}

/// 被闸门拦下的信号（审计用，字段对齐 Python `blocked`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BlockedSignal {
    pub code: String,
    pub buy_date: Date8,
    pub reason: String,
    pub detail: String,
    pub price: f64,
    pub qty: i64,
    pub notional: f64,
    pub bar_amount: f64,
    pub gap_pct: Option<f64>,
    pub limit_pct: f64,
    pub is_st: bool,
}

/// 运行诊断（约束是否生效的审计依据；Python 只打印，不落盘）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Diagnostics {
    /// 扫描出的确认信号数
    pub n_signals: usize,
    /// 扫描阶段加载/分析失败被跳过的票数
    pub n_scan_errors: usize,
    pub n_blocked: usize,
    pub skipped_perday: usize,
    pub skipped_maxpos: usize,
    pub skipped_nofund: usize,
    pub skipped_held: usize,
    /// 已持仓再次买入覆盖旧仓次数（skip_held=false 时）
    pub n_overwrite: usize,
    /// 缠论卖点判定抛错被吞掉的次数（Python: except Exception: pass）
    pub n_sell_advice_errors: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BacktestResult {
    pub trades: Vec<Trade>,
    pub nav: Vec<NavPoint>,
    pub stats: Stats,
    /// 被闸门拦下的信号（审计用）
    pub blocked: Vec<BlockedSignal>,
    pub diagnostics: Diagnostics,
}

/// 运行组合回测（生产入口：`ChanlunSignalSource` + `ChanlunSellAdvisor`）。
///
/// `universe` 为空时使用 `store.list_symbols()`（全市场）。
///
/// 注意：依赖 `chanlun_core::analyze`（core-porter 并行移植中）。core 就绪前调用会 panic
/// （`unimplemented!`），单测请用 [`signals::MockSignalSource`] + [`sell::MockSellAdvisor`]
/// 走 [`simulate::run_with`]。
pub fn run(
    store: &chanlun_data::store::Store,
    universe: &[String],
    cfg: &BacktestConfig,
) -> Result<BacktestResult> {
    let st = load_st_table(cfg);
    let mut signals = ChanlunSignalSource;
    let mut sells = ChanlunSellAdvisor;
    simulate::run_with(store, universe, cfg, &mut signals, &mut sells, &st)
}

/// 按配置加载 ST 表；文件缺失 / 解析失败 → 空表（全部按非 ST，与 Python 降级行为一致）。
pub fn load_st_table(cfg: &BacktestConfig) -> StTable {
    match &cfg.st_flags_path {
        Some(path) => StTable::load(path).unwrap_or_else(|_| StTable::empty()),
        None => StTable::empty(),
    }
}

/// Python `round(x, ndigits)` 语义（half-even，作用于真实二进制值）。
///
/// 直接用 Rust 的定点格式化实现（Rust 对 f64 的定点格式化同样按真实值舍入），
/// 避免 `(x*100).round()` 在二进制边界上与 Python 不一致（如 `round(2.675,2)`）。
pub(crate) fn py_round(x: f64, ndigits: usize) -> f64 {
    format!("{x:.ndigits$}").parse::<f64>().unwrap_or(x)
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("数据错误：{0}")]
    Data(#[from] chanlun_data::DataError),
    #[error("缠论计算错误：{0}")]
    Chan(#[from] chanlun_core::ChanError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 解析错误：{0}")]
    Json(#[from] serde_json::Error),
    #[error("尚未实现：{0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, EngineError>;
