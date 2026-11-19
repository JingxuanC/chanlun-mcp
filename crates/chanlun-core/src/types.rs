//! 核心数据模型。字段命名/语义刻意对齐 Python `chanlun.py` 的 dataclass，
//! 便于做 golden diff；日期使用 `u32`（YYYYMMDD）而非字符串，3000 万根 bar
//! 的量级下可省下大量分配。

use serde::{Deserialize, Serialize};

/// 日期，YYYYMMDD（例：20261116）
pub type Date8 = u32;

/// 原始 60min K 线。`amount` 单位：元（成交额，不存在的源填 0.0）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub date: Date8,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    /// 成交额（元）。Phase C 流动性过滤 / P2 相对流动性闸门依赖它。
    pub amount: f64,
}

/// 单只标的的完整 bar 序列（按时间升序）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BarSeries {
    pub symbol: String,
    pub bars: Vec<Bar>,
}

/// 笔/线段方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Up,
    Down,
}

/// 合并后 K 线；`start`/`end` 是原始 bar 下标区间（闭区间）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergedKline {
    /// 合并后序列下标（0 起）
    pub idx: usize,
    /// 原始 bar 起始下标
    pub start: usize,
    /// 原始 bar 结束下标
    pub end: usize,
    pub date_start: Date8,
    pub date_end: Date8,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FractalKind {
    Top,
    Bottom,
}

/// 分型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fractal {
    /// 所在合并 K 线下标
    pub idx: usize,
    /// 所在合并 K 线的首根原始 bar 下标
    pub raw_index: usize,
    pub date: Date8,
    pub high: f64,
    pub low: f64,
    pub kind: FractalKind,
}

/// 笔。`momentum` 在 `analyze` 阶段由 `bi_momentum` 填充。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bi {
    /// 起点合并 K 线下标
    pub start_idx: usize,
    /// 终点合并 K 线下标
    pub end_idx: usize,
    /// 起点原始 bar 下标
    pub start_raw: usize,
    /// 终点原始 bar 下标
    pub end_raw: usize,
    pub start_date: Date8,
    pub end_date: Date8,
    pub start_price: f64,
    pub end_price: f64,
    pub direction: Direction,
    pub momentum: f64,
    pub is_confirmed: bool,
}

/// 中枢。`zg`/`zd` 为中枢上下沿。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zhongshu {
    /// 起始笔下标（`bis` 数组下标）
    pub start_idx: usize,
    /// 终止笔下标（`bis` 数组下标）
    pub end_idx: usize,
    pub start_raw: usize,
    pub end_raw: usize,
    pub start_date: Date8,
    pub end_date: Date8,
    pub zg: f64,
    pub zd: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalKind {
    /// 一买（底背驰）
    B1,
    /// 二买（本策略入场信号）
    B2,
}

/// 买点信号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuySignal {
    pub kind: SignalKind,
    /// 触发笔下标（`bis` 数组下标）
    pub bi_idx: usize,
    /// 信号 bar 的原始下标（底分型所在 bar）
    pub raw_index: usize,
    pub date: Date8,
    /// 分型低点（一买/二买的结构低点）
    pub price: f64,
    pub low: f64,
    /// 一买：自身笔下标；二买：前置一买所在笔下标
    pub first_buy_bi_idx: Option<usize>,
    /// 关联中枢下标（可读性用）
    pub zhongshu_idx: Option<usize>,
}

/// MACD 三条序列（长度 = bar 数）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Macd {
    pub dif: Vec<f64>,
    pub dea: Vec<f64>,
    pub hist: Vec<f64>,
}

/// 背驰比较口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DivergenceMode {
    /// 相邻同向笔比较（生产默认）
    Adjacent,
    /// 严格缠论：围绕同一中枢的进入段 vs 离开段
    Pivot,
}

/// 力度口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MomentumMode {
    /// hist 正/负柱面积（生产默认）
    Area,
    /// |DIF| 峰值
    Peak,
}

/// 分析器参数。默认值与生产口径一致（文档附录 B）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    /// 顶底分型间最少合并 K 线数（新笔定义），生产值 4
    pub min_bars_between_fractals: usize,
    /// 构成中枢的最小笔数，生产值 3
    pub min_segments_in_zhongshu: usize,
    pub momentum_mode: MomentumMode,
    /// 一买（底背驰）是否必须创新低
    pub require_new_low: bool,
    /// 二买回调不破一买低点的容差，生产值 0.005
    pub tolerance_pct: f64,
    /// 二买是否要求回调笔力度 < 反弹笔力度
    pub use_momentum_weak_for_b2: bool,
    /// 止损比例（卖出判定用），生产值 0.05
    pub stop_loss_pct: f64,
    /// 最大持有（交易日），生产值 0 表示关闭由引擎控制
    pub max_hold_days: usize,
    pub divergence_mode: DivergenceMode,
    /// 跌破中枢 ZD 是否需要回抽确认
    pub break_zd_confirm: bool,
    /// MACD (fast, slow, signal)，生产值 (12, 26, 9)
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            min_bars_between_fractals: 4,
            min_segments_in_zhongshu: 3,
            momentum_mode: MomentumMode::Area,
            require_new_low: true,
            tolerance_pct: 0.005,
            use_momentum_weak_for_b2: true,
            stop_loss_pct: 0.05,
            max_hold_days: 20,
            divergence_mode: DivergenceMode::Adjacent,
            break_zd_confirm: false,
            macd_fast: 12,
            macd_slow: 26,
            macd_signal: 9,
        }
    }
}

/// 一次完整分析的结果（对应 Python `ChanlunResult`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Analysis {
    pub macd: Macd,
    pub merged: Vec<MergedKline>,
    pub fractals: Vec<Fractal>,
    pub bis: Vec<Bi>,
    pub zhongshus: Vec<Zhongshu>,
    pub b1: Vec<BuySignal>,
    pub b2: Vec<BuySignal>,
}

/// 卖出建议（对应 Python `latest_sell_advice` 的返回）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SellAdvice {
    pub top_divergence: bool,
    pub break_zd: bool,
    /// 可读原因，如 "顶背驰(上涨笔力度衰竭);跌破中枢ZD(12.520);"
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChanError {
    #[error("K 线不足：需要至少 {need} 根，实际 {got} 根")]
    NotEnoughBars { need: usize, got: usize },
    #[error("尚未实现：{0}")]
    Unimplemented(&'static str),
    #[error("输入非法：{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ChanError>;
