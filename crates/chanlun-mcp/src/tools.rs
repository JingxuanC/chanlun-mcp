//! MCP 工具契约（与语言实现无关的部分：工具名、入参、响应结构）。
//!
//! 设计原则：
//! 1. **少而粗**：LLM 不需要 47 个工具；这里 8 个 + 2 个 job 工具即可。
//! 2. **口径内建**：所有响应都带 `Caliber`，防止模型把回测数字当实盘业绩。
//! 3. **默认只读**：`refresh_data` 是唯一的写工具，需要显式开关。
//! 4. **长任务不阻塞**：`run_backtest` / `scan_signals(all_market=true)` 走 job。

use chanlun_core::Date8;
use serde::{Deserialize, Serialize};

/// 所有响应的口径头。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caliber {
    /// 数据截止日（YYYYMMDD）
    pub as_of: Date8,
    /// legacy / dynpos_cap100w
    pub funding_mode: String,
    /// 价格口径："qfq"（前复权）
    pub adjust: String,
    /// 是否包含 P1/P2/P5 闸门
    pub with_execution_gates: bool,
    /// 必须原样转述给用户的警告（不可与券商对账、容量不成立等）
    pub warnings: Vec<String>,
}

impl Caliber {
    pub fn production(as_of: Date8, funding_mode: &str) -> Self {
        Self {
            as_of,
            funding_mode: funding_mode.to_string(),
            adjust: "qfq".into(),
            with_execution_gates: true,
            warnings: vec![
                "前复权口径：收益率/胜率/回撤有效，但买入价与股数不是当时真实盘口，不可与券商成交单对账".into(),
                "dynpos 复利口径容量不成立，实盘请按 legacy 或更小单笔规模设计".into(),
                "回测未建模涨跌停排队与真实盘口深度，实盘可成交性需折让".into(),
            ],
        }
    }
}

/// `scan_signals` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResponse {
    pub caliber: Caliber,
    pub date: Date8,
    pub signals: Vec<SignalItem>,
    /// 被 Phase B/C 过滤掉的候选数
    pub filtered_out: usize,
    /// 被 P1/P2 闸门拦下的候选数
    pub blocked: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalItem {
    pub code: String,
    pub name: Option<String>,
    /// 信号 bar 日期与时间（10:30 / 11:30 / 14:00 / 15:00）
    pub signal_bar: String,
    /// 一买低点 / 反弹高点 / 回调低点
    pub b1_low: f64,
    pub bounce_high: f64,
    pub pullback_low: f64,
    /// 回调笔力度 / 反弹笔力度
    pub momentum_pullback: f64,
    pub momentum_bounce: f64,
    /// 确认 bar 收盘相对 MA13 乖离（%）
    pub ma13_deviation_pct: f64,
    /// 确认时 20 根均成交额（元）
    pub amount20: f64,
    /// 建议单笔金额（按资金口径）与止损位
    pub suggested_notional: f64,
    pub stop_loss_price: f64,
    /// 可成交性提示（一字板/涨停开盘/相对流动性）
    pub tradability_note: Option<String>,
}

/// `analyze_stock` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzeResponse {
    pub caliber: Caliber,
    pub code: String,
    pub last_close: f64,
    pub ma13: f64,
    /// 最近 3 段笔的方向与价格
    pub recent_bis: Vec<RecentBi>,
    pub latest_zhongshu: Option<ZhongshuItem>,
    /// 入场后的离场条件距离
    pub exit_distances: ExitDistances,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentBi {
    pub direction: String,
    pub start_date: Date8,
    pub end_date: Date8,
    pub start_price: f64,
    pub end_price: f64,
    pub momentum: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZhongshuItem {
    pub zg: f64,
    pub zd: f64,
    pub start_date: Date8,
    pub end_date: Date8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitDistances {
    /// 现价距 5% 止损的百分比
    pub to_stop_loss_pct: f64,
    /// 现价距 MA13 的百分比（负=已跌破）
    pub to_ma13_pct: f64,
    /// 现价距中枢 ZD 的百分比
    pub to_zd_pct: Option<f64>,
}

/// `get_plan` 响应（跟单卡）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResponse {
    pub caliber: Caliber,
    pub nav: f64,
    pub next_budget: f64,
    pub positions: Vec<PositionItem>,
    pub pending_sells: Vec<PendingSell>,
    pub pending_buys: Vec<PendingBuy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionItem {
    pub code: String,
    pub qty: i64,
    pub entry_price: f64,
    pub buy_date: Date8,
    pub hold_days: u32,
    pub last_close: f64,
    pub pnl_pct: f64,
    pub stop_loss_price: f64,
    pub ma13: f64,
    pub pending_sell_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSell {
    pub code: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBuy {
    pub code: String,
    pub ref_close: f64,
    pub ma13: f64,
}

/// `get_performance` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceResponse {
    pub caliber: Caliber,
    pub nav_end: f64,
    pub total_ret: f64,
    pub cagr: f64,
    pub max_dd: f64,
    pub n_trades: usize,
    pub win_rate: f64,
    pub avg_ret: f64,
    pub profit_factor: f64,
    /// 文档未披露但必须补的风险指标：前 5% 交易贡献的利润占比
    pub top5pct_profit_share: f64,
    pub yearly: Vec<YearlyItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YearlyItem {
    pub year: u16,
    pub ret: f64,
    pub max_dd: f64,
    pub trades: usize,
}

/// 工具名常量（MCP tool 注册表用）。
pub const TOOL_NAMES: &[&str] = &[
    "get_plan",
    "scan_signals",
    "analyze_stock",
    "explain_signal",
    "get_performance",
    "data_health",
    "run_backtest",
    "get_job",
];
