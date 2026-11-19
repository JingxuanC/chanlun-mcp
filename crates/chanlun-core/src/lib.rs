//! # chanlun-core
//!
//! 缠论结构计算核心（纯函数、无 IO、无全局状态）。
//!
//! 逐行对齐生产 Python 实现 `code/chanlun.py`（v1.0 文档包，919 行），
//! 目标是在相同输入下**逐笔逐信号**与 Python 完全一致 —— 这是整个
//! chanlun-mcp 项目的验收底线（见 `docs/plan.md` 的 G-1 golden test）。
//!
//! 计算链：
//! ```text
//! 原始 60min K 线
//!   -> merge_klines    包含关系合并（注意：Python 用最后一根合并K线的阴阳判定方向，
//!                      并非严格缠论的"笔方向"，这里刻意保持 parity，见 merge.rs）
//!   -> find_fractals   严格顶/底分型
//!   -> find_bis        新笔定义（分型序号差 >= 4，末尾可留未确认笔）
//!   -> bi_momentum     每笔 MACD 力度（hist 正/负柱面积）
//!   -> find_zhongshus  连续三笔重叠起中枢，ZG=min(高点), ZD=max(低点)
//!   -> find_buy_signals 一买（底背驰）/ 二买（回调不破前低 + 回调力度 < 反弹力度）
//!   -> latest_sell_advice 顶背驰 / 跌破中枢 ZD
//! ```

pub mod analyze;
pub mod bi;
pub mod fractal;
pub mod macd;
pub mod merge;
pub mod sell;
pub mod signals;
pub mod types;
pub mod zhongshu;

pub use analyze::*;
pub use bi::*;
pub use fractal::*;
pub use macd::*;
pub use merge::*;
pub use sell::*;
pub use signals::*;
pub use types::*;
pub use zhongshu::*;
