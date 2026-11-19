//! MCP Prompts：把"怎么用这些数据"固化成可复用模板。
//!
//! - `premarket_briefing`：盘前简报（明日卖/明日买/持仓风险/数据健康）
//! - `position_review`：持仓复盘（离场条件、浮盈亏、论点是否破坏）
//! - `signal_explain`：把某个二买信号讲给非技术用户听

pub const PROMPT_NAMES: &[&str] = &["premarket_briefing", "position_review", "signal_explain"];
