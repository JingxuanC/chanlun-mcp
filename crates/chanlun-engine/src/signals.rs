//! 买点信号源（可注入）。
//!
//! 引擎本身不关心信号怎么来的：`SignalSource::scan` 输入单票 bar 序列 + 配置，
//! 输出**已确认**的买点列表（`buy_bar` 为买入 bar 在输入序列中的下标、`buy_date` 为该 bar
//! 日期、`price` 为该 bar 开盘价）。
//!
//! 两个实现：
//!
//! - [`ChanlunSignalSource`]：生产实现，对齐 Python `scan_confirm_stock`：
//!   先 `find_b2_signals(全序列)` 拿候选，再对每个候选在 `g+confirm_delay(+k*recalc_every)`
//!   处截断重算做确认（无前视），确认后 `g+confirm_delay+(confirm_k-1)*recalc_every+1`
//!   开盘买入；随后过 Phase B（MA13 乖离上限）/ Phase C（amt20 流动性上限）过滤。
//!   **依赖 `chanlun_core::analyze`（core-porter 并行移植中）**。
//! - [`MockSignalSource`]：测试/回放用，直接注入固定信号，与 core 完全解耦。
//!
//! ## Python 对齐细节（`code/backtest_portfolio.py:scan_confirm_stock`）
//!
//! ```text
//! cands = find_b2_signals(df)                 # 全序列，候选 index = g（底分型 raw bar）
//! 常规回测（非 snapshot）:
//!   if g + confirm_delay >= n - 2: skip       # 确认 bar 离末尾至少留 2 根
//!   for k in 0..confirm_k:
//!       cut = g + confirm_delay + k*recalc_every
//!       if cut < 50 or cut > n-2: reject
//!       if g not in {s.index for s in find_b2_signals(df[:cut+1])}: reject
//!   buy_bar = g + confirm_delay + (confirm_k-1)*recalc_every + 1
//!   Phase B: close[buy_bar-1] > MA13(13).mean() * (1+ma13_entry_filter) -> skip
//!   Phase C: mean(amount[buy_bar-20 ..= buy_bar-1]) >= max_amount -> skip
//!   signal = { buy_bar, buy_date=date[buy_bar], price=open[buy_bar] }
//! ```
//!
//! 注：Python 的 `allow_pending`（快照）分支在回测里用不到，这里不实现。
//! Phase D（ST/次新/小市值全域过滤，`universe_filters.json`）由 MCP 层在构造 universe 时
//! 预过滤，引擎不重复实现（见任务书）。

use std::collections::HashMap;

use chanlun_core::{Bar, Date8};

use crate::{BacktestConfig, EngineError};

/// 已确认买点。
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmedSignal {
    /// 买入 bar 在输入序列中的下标（Python `df.iloc[buy_bar]`）。
    pub buy_bar: usize,
    /// 买入 bar 日期（YYYYMMDD）。
    pub buy_date: Date8,
    /// 买入 bar 开盘价（Python 扫描用的 float64 原始价）。
    pub price: f64,
}

/// 可注入的买点信号源。
pub trait SignalSource {
    /// 扫描单票，返回**已确认**的买点（按 buy_bar 升序）。
    fn scan(
        &mut self,
        code: &str,
        bars: &[Bar],
        cfg: &BacktestConfig,
    ) -> Result<Vec<ConfirmedSignal>, EngineError>;

    /// 只用于报告/日志。
    fn name(&self) -> &'static str {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// Mock 信号源
// ---------------------------------------------------------------------------

/// 注入固定信号的测试/回放信号源。
#[derive(Debug, Clone, Default)]
pub struct MockSignalSource {
    signals: HashMap<String, Vec<ConfirmedSignal>>,
}

impl MockSignalSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条信号（同一 code 多次调用会累积）。
    pub fn push(&mut self, code: &str, buy_bar: usize, buy_date: Date8, price: f64) {
        self.signals
            .entry(code.to_string())
            .or_default()
            .push(ConfirmedSignal {
                buy_bar,
                buy_date,
                price,
            });
    }

    pub fn push_signal(&mut self, code: &str, sig: ConfirmedSignal) {
        self.signals.entry(code.to_string()).or_default().push(sig);
    }

    pub fn is_empty(&self) -> bool {
        self.signals.values().all(|v| v.is_empty())
    }
}

impl SignalSource for MockSignalSource {
    fn scan(
        &mut self,
        code: &str,
        _bars: &[Bar],
        _cfg: &BacktestConfig,
    ) -> Result<Vec<ConfirmedSignal>, EngineError> {
        let mut out = self.signals.get(code).cloned().unwrap_or_default();
        out.sort_by_key(|s| (s.buy_date, s.buy_bar));
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

// ---------------------------------------------------------------------------
// Chanlun 信号源（生产实现；依赖 core）
// ---------------------------------------------------------------------------

/// 生产信号源：`chanlun_core::analyze` + `g+confirm_delay` 截断重算确认 + Phase B/C。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChanlunSignalSource;

impl ChanlunSignalSource {
    pub fn new() -> Self {
        Self
    }
}

impl SignalSource for ChanlunSignalSource {
    fn scan(
        &mut self,
        _code: &str,
        bars: &[Bar],
        cfg: &BacktestConfig,
    ) -> Result<Vec<ConfirmedSignal>, EngineError> {
        let n = bars.len();
        // Python `if len(df) < 300: result["error"] = "K线不足"` → 该票跳过
        if n < 300 {
            return Ok(Vec::new());
        }

        // 全序列候选（find_b2_signals = analyze + 取 status=="valid" 的 b2）
        let base = match chanlun_core::analyze(bars, &cfg.analyzer) {
            Ok(a) => a,
            // Python 里 analyze 对 len<50 返回空结果；core 用 Err 表达，等价于无候选。
            Err(_) => return Ok(Vec::new()),
        };

        let max_cut = n - 2; // 常规回测：确认 bar 上界 = 倒数第 2 根
        let mut out: Vec<ConfirmedSignal> = Vec::new();

        for cand in &base.b2 {
            let g = cand.raw_index; // Python `s["index"]` = 底分型所在 raw bar 下标
            // 常规回测预检：确认 bar 之后还要留出买入 bar（g+5 <= n-2）
            if g + cfg.confirm_delay >= n - 2 {
                continue;
            }

            // ---- 时点确认（无前视）：截断到 cut 重算，g 仍是有效二买 ----
            let mut confirmed = true;
            for k in 0..cfg.confirm_k.max(1) {
                let cut = g + cfg.confirm_delay + k * cfg.recalc_every;
                if cut < 50 || cut > max_cut {
                    confirmed = false;
                    break;
                }
                let prefix = &bars[..=cut];
                let sigs = match chanlun_core::analyze(prefix, &cfg.analyzer) {
                    Ok(a) => a,
                    Err(_) => {
                        confirmed = false;
                        break;
                    }
                };
                if !sigs.b2.iter().any(|s| s.raw_index == g) {
                    confirmed = false;
                    break;
                }
            }
            if !confirmed {
                continue;
            }

            // ---- 买入 bar / Phase B / Phase C ----
            let buy_bar =
                g + cfg.confirm_delay + cfg.confirm_k.saturating_sub(1) * cfg.recalc_every + 1;
            if buy_bar >= n {
                continue;
            }
            if phase_b_reject(bars, buy_bar, cfg.ma13_entry_filter) {
                continue;
            }
            if phase_c_reject(bars, buy_bar, cfg.max_amount) {
                continue;
            }
            out.push(ConfirmedSignal {
                buy_bar,
                buy_date: bars[buy_bar].date,
                price: bars[buy_bar].open,
            });
        }

        // find_b2_signals 内部已按 index 排序；确认过滤保持顺序。
        out.sort_by_key(|s| s.buy_bar);
        out.dedup_by_key(|s| s.buy_bar);
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "chanlun"
    }
}

/// Phase B：入场 MA13 乖离过滤。`true` = 丢弃信号（对齐 Python）。
///
/// `confirm_bar = buy_bar - 1`，`confirm_bar < 12` 时放行（Python `if cb < 12: continue`）。
pub(crate) fn phase_b_reject(bars: &[Bar], buy_bar: usize, ma13_entry_filter: f64) -> bool {
    if ma13_entry_filter <= 0.0 || buy_bar == 0 {
        return false;
    }
    let cb = buy_bar - 1;
    if cb < 12 {
        return false;
    }
    // 对齐 Python：pandas float32 Series.mean()（numpy pairwise 求和）
    let closes: Vec<f32> = bars[cb - 12..=cb].iter().map(|b| b.close as f32).collect();
    let ma13 = crate::precision::mean_f32_numpy(&closes);
    let threshold = ma13 * (1.0f32 + ma13_entry_filter as f32);
    (bars[cb].close as f32) > threshold
}

/// Phase C：amt20 流动性上限过滤。`true` = 丢弃信号（对齐 Python）。
///
/// `amount` 缺失（全 0）或 bar 不足时放行（不误杀）。
pub(crate) fn phase_c_reject(bars: &[Bar], buy_bar: usize, max_amount: f64) -> bool {
    if max_amount <= 0.0 || buy_bar == 0 {
        return false;
    }
    let cb = buy_bar - 1;
    if cb < 19 {
        return false;
    }
    let slice = &bars[cb - 19..=cb];
    let has_amount = slice.iter().any(|b| b.amount != 0.0);
    if !has_amount {
        return false;
    }
    let amt20 = slice.iter().map(|b| b.amount).sum::<f64>() / slice.len() as f64;
    amt20 >= max_amount
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(close: f64, amount: f64) -> Bar {
        Bar {
            date: 20260101,
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
            amount,
        }
    }

    fn bars_with_closes(closes: &[f64]) -> Vec<Bar> {
        closes.iter().map(|c| bar(*c, 0.0)).collect()
    }

    #[test]
    fn mock_source_is_injected_verbatim() {
        let mut src = MockSignalSource::new();
        src.push("600000", 5, 20260110, 10.0);
        src.push("600000", 3, 20260108, 9.0);
        let cfg = BacktestConfig::default();
        let sigs = src.scan("600000", &[], &cfg).unwrap();
        assert_eq!(sigs.len(), 2);
        assert_eq!(sigs[0].buy_bar, 3);
        assert_eq!(sigs[0].buy_date, 20260108);
        assert_eq!(sigs[1].buy_date, 20260110);
        assert!(src.scan("000001", &[], &cfg).unwrap().is_empty());
    }

    #[test]
    fn phase_b_boundary() {
        // 13 根收盘价：MA13 = 10；confirm bar close = 11 > 10*1.01? 11 > 10.1 -> 丢弃
        let mut closes = vec![10.0; 13];
        closes[12] = 11.0;
        let bars = bars_with_closes(&closes);
        // buy_bar = 13 -> confirm bar = 12；需要 14 根（buy_bar 指向第 14 根）
        let mut bars = bars;
        bars.push(bar(10.0, 0.0));
        assert!(phase_b_reject(&bars, 13, 0.01));
        // 阈值 0.2（20%）：11 <= 12 -> 放行
        assert!(!phase_b_reject(&bars, 13, 0.20));
        // 阈值 0 = 关闭
        assert!(!phase_b_reject(&bars, 13, 0.0));
        // confirm bar < 12 -> 放行
        assert!(!phase_b_reject(&bars, 8, 0.01));
    }

    #[test]
    fn phase_c_boundary() {
        // 20 根金额 1000 万 -> amt20 = 1000 万；上限 5000 万 -> 放行
        let mut bars: Vec<Bar> = (0..20).map(|_| bar(10.0, 1e7)).collect();
        bars.push(bar(10.0, 1e7)); // buy_bar = 20, confirm bar=19，取 [0..=19]
        assert!(!phase_c_reject(&bars, 20, 5e7));
        // 上限 500 万 -> 1000万 >= 500万 -> 丢弃
        assert!(phase_c_reject(&bars, 20, 5e6));
        // 上限 0 -> 关闭
        assert!(!phase_c_reject(&bars, 20, 0.0));
        // amount 全 0（源无成交额）-> 放行
        let bars0: Vec<Bar> = (0..21).map(|_| bar(10.0, 0.0)).collect();
        assert!(!phase_c_reject(&bars0, 20, 5e6));
        // bar 不足 20 -> 放行
        let few: Vec<Bar> = (0..10).map(|_| bar(10.0, 1e7)).collect();
        assert!(!phase_c_reject(&few, 9, 5e6));
    }
}
