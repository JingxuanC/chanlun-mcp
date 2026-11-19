//! 分析流水线编排（对齐 Python `ChanlunAnalyzer.analyze`）。

use crate::bi::find_bis;
use crate::fractal::find_fractals;
use crate::macd::compute_macd;
use crate::merge::merge_klines;
use crate::signals::find_buy_signals;
use crate::types::{Analysis, AnalyzerConfig, Bar, ChanError, Result};
use crate::zhongshu::find_zhongshus;

/// 对一段 60min bar 序列跑完整缠论分析。
///
/// 与 Python 一致的短序列保护：`len < 50` 直接返回空结果（Python 返回 `ChanlunResult(df)`）。
pub fn analyze(bars: &[Bar], cfg: &AnalyzerConfig) -> Result<Analysis> {
    if bars.len() < 50 {
        return Err(ChanError::NotEnoughBars {
            need: 50,
            got: bars.len(),
        });
    }

    let macd = compute_macd(bars, cfg.macd_fast, cfg.macd_slow, cfg.macd_signal);
    let merged = merge_klines(bars);
    let fractals = find_fractals(&merged);
    let mut bis = find_bis(&merged, &fractals, cfg);
    for bi in bis.iter_mut() {
        bi.momentum = bi_momentum(bi, &macd, cfg.momentum_mode);
    }
    let zhongshus = find_zhongshus(&bis, cfg);
    let (b1, b2) = find_buy_signals(&bis, &zhongshus, cfg);

    Ok(Analysis {
        macd,
        merged,
        fractals,
        bis,
        zhongshus,
        b1,
        b2,
    })
}

/// 一笔的 MACD 力度（对齐 Python `_bi_momentum`）。
///
/// 笔跨原始行 `[start_raw, end_raw]`（Python 是 `iloc[s:e+1]`，越界自动截断）：
/// - `MomentumMode::Area`：下跌笔取 `sum(max(-hist, 0))`，上涨笔取 `sum(max(hist, 0))`；
/// - `MomentumMode::Peak`：取 `max(|dif|)`（空区间返回 0，Python 有 `len(dif)==0 -> 0.0` 保护）。
pub fn bi_momentum(
    bi: &crate::types::Bi,
    macd: &crate::types::Macd,
    mode: crate::types::MomentumMode,
) -> f64 {
    let n = macd.dif.len().min(macd.hist.len());
    let s = bi.start_raw;
    if s >= n {
        return 0.0;
    }
    let e = bi.end_raw.min(n - 1);
    if s > e {
        return 0.0;
    }
    match mode {
        crate::types::MomentumMode::Peak => {
            macd.dif[s..=e].iter().map(|d| d.abs()).fold(0.0, f64::max)
        }
        crate::types::MomentumMode::Area => match bi.direction {
            crate::types::Direction::Down => macd.hist[s..=e].iter().map(|h| (-h).max(0.0)).sum(),
            crate::types::Direction::Up => macd.hist[s..=e].iter().map(|h| h.max(0.0)).sum(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Bar, ChanError};

    fn flat_bar(i: usize) -> Bar {
        Bar {
            date: 20240101 + i as u32,
            open: 10.0,
            high: 10.0,
            low: 10.0,
            close: 10.0,
            volume: 0.0,
            amount: 0.0,
        }
    }

    #[test]
    fn too_few_bars_returns_not_enough_bars() {
        // 与 engine 的契约：len<50 返回 Err(NotEnoughBars)，engine 按 Python 空结果处理。
        let cfg = AnalyzerConfig::default();
        let err = analyze(&[flat_bar(0); 49], &cfg).unwrap_err();
        assert_eq!(err, ChanError::NotEnoughBars { need: 50, got: 49 });
    }

    #[test]
    fn fifty_flat_bars_produce_empty_structure() {
        let cfg = AnalyzerConfig::default();
        let bars: Vec<Bar> = (0..50).map(flat_bar).collect();
        let a = analyze(&bars, &cfg).expect("50 根应能分析");
        assert_eq!(a.macd.dif.len(), 50);
        assert!(a.fractals.is_empty());
        assert!(a.bis.is_empty());
        assert!(a.zhongshus.is_empty());
        assert!(a.b1.is_empty() && a.b2.is_empty());
    }
}
