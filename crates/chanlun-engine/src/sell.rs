//! 缠论卖点判定（可注入）。
//!
//! 引擎在**每个交易日最后一根 bar 收盘**（`ExitCheckMode::DailyClose`，Python 生产行为）
//! 对持仓做离场判定，顺序固定：止损 → 跌破 MA13 → 超时 → 缠论卖点（顶背驰 / 跌破中枢 ZD）。
//! 前三者由引擎直接判定；缠论卖点通过 [`SellAdvisor`] 注入，生产实现是
//! [`ChanlunSellAdvisor`]，测试用 [`MockSellAdvisor`]。
//!
//! 时序（对齐 Python `backtest_portfolio.py:525-568`）：
//!
//! ```text
//! 当日收盘：i_last = 当日最后一根 bar；start = max(0, i_last+1-window)
//!          advice = latest_sell_advice(bars[start..=i_last],
//!                                      entry_price=成本价,
//!                                      entry_idx=buy_bar_idx - start)
//!          advice.top_divergence || advice.break_zd -> pending_sell[code] = advice.note
//! 次日开盘：以开盘价卖出（一字跌停顺延）
//! ```
//!
//! 注意 Python 的重算节流是 `day_idx - last_check_day >= recalc_every // 4`（默认 = 1，
//! 即**每个交易日都重算**），不是每 4 天一次。

use std::collections::HashMap;

use chanlun_core::{Bar, Date8, SellAdvice};

use crate::{BacktestConfig, EngineError};

/// 缠论卖点判定接口。
pub trait SellAdvisor {
    /// `window_bars` 是引擎已截断到 `window` 的 bar 窗口；`entry_raw` 是买点 bar 相对窗口
    /// 起点的偏移（买点早于窗口时传 0，与 Python 传负数的过滤效果一致）。
    fn advise(
        &mut self,
        code: &str,
        window_bars: &[Bar],
        entry_price: f64,
        entry_raw: usize,
        cfg: &BacktestConfig,
    ) -> Result<SellAdvice, EngineError>;

    fn name(&self) -> &'static str {
        "unknown"
    }
}

/// 生产实现：`chanlun_core::analyze` + `chanlun_core::latest_sell_advice`。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChanlunSellAdvisor;

impl ChanlunSellAdvisor {
    pub fn new() -> Self {
        Self
    }
}

impl SellAdvisor for ChanlunSellAdvisor {
    fn advise(
        &mut self,
        _code: &str,
        window_bars: &[Bar],
        entry_price: f64,
        entry_raw: usize,
        cfg: &BacktestConfig,
    ) -> Result<SellAdvice, EngineError> {
        // Python: analyze 对 len<50 返回空 ChanlunResult -> latest_sell_advice 返回全 false。
        let analysis = match chanlun_core::analyze(window_bars, &cfg.analyzer) {
            Ok(a) => a,
            Err(_) => return Ok(SellAdvice::default()),
        };
        // 契约（已与 core-porter 对齐）：analysis 已是窗口内结果，entry_raw 为窗口内偏移；
        // window_bars 参数 core 可忽略（引擎已在调用前截窗）。
        Ok(chanlun_core::latest_sell_advice(
            &analysis,
            entry_price,
            entry_raw,
            cfg.window,
            &cfg.analyzer,
        ))
    }

    fn name(&self) -> &'static str {
        "chanlun"
    }
}

/// 测试/回放用卖点注入器：按 `(code, 窗口最后一根 bar 的日期)` 查表，查不到返回默认（不卖）。
#[derive(Debug, Clone, Default)]
pub struct MockSellAdvisor {
    rules: HashMap<(String, Date8), SellAdvice>,
}

impl MockSellAdvisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 在 `date8` 当天收盘（该票当日最后一根 bar）给出指定卖点。
    pub fn set(&mut self, code: &str, date8: Date8, advice: SellAdvice) {
        self.rules.insert((code.to_string(), date8), advice);
    }

    /// 便捷方法：在 `date8` 触发「跌破中枢ZD」卖点。
    pub fn set_break_zd(&mut self, code: &str, date8: Date8, note: &str) {
        self.set(
            code,
            date8,
            SellAdvice {
                top_divergence: false,
                break_zd: true,
                note: note.to_string(),
            },
        );
    }

    /// 便捷方法：在 `date8` 触发「顶背驰」卖点。
    pub fn set_top_divergence(&mut self, code: &str, date8: Date8, note: &str) {
        self.set(
            code,
            date8,
            SellAdvice {
                top_divergence: true,
                break_zd: false,
                note: note.to_string(),
            },
        );
    }
}

impl SellAdvisor for MockSellAdvisor {
    fn advise(
        &mut self,
        code: &str,
        window_bars: &[Bar],
        _entry_price: f64,
        _entry_raw: usize,
        _cfg: &BacktestConfig,
    ) -> Result<SellAdvice, EngineError> {
        let Some(last) = window_bars.last() else {
            return Ok(SellAdvice::default());
        };
        Ok(self
            .rules
            .get(&(code.to_string(), last.date))
            .cloned()
            .unwrap_or_default())
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(date: Date8) -> Bar {
        Bar {
            date,
            open: 10.0,
            high: 10.0,
            low: 10.0,
            close: 10.0,
            volume: 1.0,
            amount: 0.0,
        }
    }

    #[test]
    fn mock_advisor_matches_by_code_and_last_date() {
        let mut adv = MockSellAdvisor::new();
        adv.set_break_zd("600000", 20260103, "跌破中枢ZD(12.520);");
        let cfg = BacktestConfig::default();
        let bars = vec![bar(20260102), bar(20260103)];
        let a = adv.advise("600000", &bars, 10.0, 0, &cfg).unwrap();
        assert!(a.break_zd);
        assert!(!a.top_divergence);
        assert_eq!(a.note, "跌破中枢ZD(12.520);");
        // 日期不匹配 -> 无卖点
        let a = adv
            .advise("600000", &[bar(20260102)], 10.0, 0, &cfg)
            .unwrap();
        assert!(!a.break_zd && !a.top_divergence && a.note.is_empty());
        // 空窗口 -> 无卖点
        let a = adv.advise("600000", &[], 10.0, 0, &cfg).unwrap();
        assert!(!a.break_zd);
    }
}
