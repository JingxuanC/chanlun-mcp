//! 包含关系合并（对齐 Python `merge_klines`）。
//!
//! # 注意：这里刻意复刻 Python 的"方向判定"启发式
//!
//! Python 实现**不是**严格按照"当前笔方向"决定 `取高高/取低低`，而是：
//! - 初始方向 = 第一根 K 线阴阳（`close >= open`）；
//! - 每次无包含时，方向 = 当前 K 线阴阳；
//! - 每次有包含时，若合并后的 K 线为阳线则方向 = 当前 K 线阴阳，否则保持旧方向。
//!
//! 文档 §4.1 写的是"按当前笔方向取高高/低低"，与代码不一致（见 docs/review-notes.md）。
//! 移植这里是**保持 parity 优先**：先做到与 Python 逐信号一致，是否改成严格缠论
//! 口径作为独立参数 `merge_direction_mode` 后续再议。

use crate::types::{Bar, Date8, MergedKline};

/// 合并包含关系 K 线。返回合并后的序列（`start`/`end` 为原始 bar 下标，闭区间）。
pub fn merge_klines(bars: &[Bar]) -> Vec<MergedKline> {
    let mut merged: Vec<MergedKline> = Vec::with_capacity(bars.len());
    if bars.is_empty() {
        return merged;
    }

    // 与 Python 一致：首根 K 线阴阳
    let mut direction_up = bars[0].close >= bars[0].open;

    for (i, b) in bars.iter().enumerate() {
        if merged.is_empty() {
            merged.push(MergedKline {
                idx: 0,
                start: i,
                end: i,
                date_start: b.date,
                date_end: b.date,
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
            });
            continue;
        }

        let top = merged.last_mut().expect("non-empty");
        let contains =
            (b.high >= top.high && b.low <= top.low) || (b.high <= top.high && b.low >= top.low);

        if contains {
            let (new_high, new_low) = if direction_up {
                // 向上趋势取高高
                (b.high.max(top.high), b.low.max(top.low))
            } else {
                // 向下趋势取低低
                (b.high.min(top.high), b.low.min(top.low))
            };
            top.high = new_high;
            top.low = new_low;
            top.close = b.close;
            top.end = i;
            top.date_end = b.date;
            // 与 Python 完全一致（含其怪癖）：合并后 K 线为阳 -> 用当前 K 线阴阳，否则保持
            if top.close >= top.open {
                direction_up = b.close >= b.open;
            }
        } else {
            direction_up = b.close >= b.open;
            let new_idx = top.idx + 1;
            merged.push(MergedKline {
                idx: new_idx,
                start: i,
                end: i,
                date_start: b.date,
                date_end: b.date,
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
            });
        }
    }

    merged
}

/// 取分型/笔的日期辅助（供调试输出）。
pub fn bar_date(bars: &[Bar], raw_index: usize) -> Option<Date8> {
    bars.get(raw_index).map(|b| b.date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(date: u32, o: f64, h: f64, l: f64, c: f64) -> Bar {
        Bar {
            date,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 1.0,
            amount: 0.0,
        }
    }

    #[test]
    fn no_containment_keeps_all_bars() {
        let bars = vec![
            b(1, 10.0, 11.0, 9.0, 10.5),
            b(2, 10.5, 12.0, 10.0, 11.5),
            b(3, 11.5, 13.0, 11.0, 12.5),
        ];
        let m = merge_klines(&bars);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].start, 2);
        assert_eq!(m[2].end, 2);
    }

    #[test]
    fn up_trend_containment_takes_high_high_low_low() {
        // 第一根阳线 -> 向上；第二根被包含 -> 取高高
        let bars = vec![
            b(1, 10.0, 12.0, 9.0, 11.5),
            b(2, 11.0, 11.8, 9.5, 11.0),
            b(3, 12.0, 13.0, 11.5, 12.5),
        ];
        let m = merge_klines(&bars);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].high, 12.0);
        assert_eq!(m[0].low, 9.5); // max(9.0, 9.5)
        assert_eq!(m[0].end, 1);
    }

    #[test]
    fn down_trend_containment_takes_low_low() {
        // 第一根阴线 -> 向下；第二根被包含 -> 取低低
        let bars = vec![
            b(1, 12.0, 12.0, 9.0, 9.5),
            b(2, 10.5, 11.5, 9.5, 10.0),
            b(3, 9.5, 10.0, 8.0, 8.5),
        ];
        let m = merge_klines(&bars);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].high, 11.5); // min(12.0, 11.5)
        assert_eq!(m[0].low, 9.0);
    }
}
