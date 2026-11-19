//! 分型识别（对齐 Python `find_fractals`）。
//!
//! 严格顶分型：中间合并 K 线的高点 > 左右高点，且低点 > 左右低点。
//! 严格底分型：中间合并 K 线的低点 < 左右低点，且高点 < 左右高点。
//! 注意：只扫描 `1..len-1`，序列两端不可能形成分型。

use crate::types::{Fractal, FractalKind, MergedKline};

/// 在合并 K 线上识别严格顶/底分型。
pub fn find_fractals(merged: &[MergedKline]) -> Vec<Fractal> {
    let mut out = Vec::new();
    if merged.len() < 3 {
        return out;
    }
    for i in 1..merged.len() - 1 {
        let (left, mid, right) = (&merged[i - 1], &merged[i], &merged[i + 1]);
        let is_top = mid.high > left.high
            && mid.high > right.high
            && mid.low > left.low
            && mid.low > right.low;
        if is_top {
            out.push(Fractal {
                idx: mid.idx,
                raw_index: mid.start,
                date: mid.date_start,
                high: mid.high,
                low: mid.low,
                kind: FractalKind::Top,
            });
            continue;
        }
        let is_bottom = mid.low < left.low
            && mid.low < right.low
            && mid.high < left.high
            && mid.high < right.high;
        if is_bottom {
            out.push(Fractal {
                idx: mid.idx,
                raw_index: mid.start,
                date: mid.date_start,
                high: mid.high,
                low: mid.low,
                kind: FractalKind::Bottom,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(idx: usize, high: f64, low: f64) -> MergedKline {
        MergedKline {
            idx,
            start: idx,
            end: idx,
            date_start: 20260101 + idx as u32,
            date_end: 20260101 + idx as u32,
            open: low,
            high,
            low,
            close: low,
        }
    }

    #[test]
    fn detects_top_and_bottom() {
        // 低 -> 中(顶) -> 低，再 高 -> 中(底) -> 高
        let merged = vec![
            k(0, 10.0, 9.0),
            k(1, 12.0, 11.0), // 顶分型
            k(2, 10.5, 9.5),
            k(3, 9.0, 8.0),
            k(4, 7.5, 6.5), // 底分型（低点最低）
            k(5, 9.0, 8.0),
        ];
        let f = find_fractals(&merged);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].kind, FractalKind::Top);
        assert_eq!(f[0].idx, 1);
        assert_eq!(f[1].kind, FractalKind::Bottom);
        assert_eq!(f[1].idx, 4);
    }

    #[test]
    fn ignores_series_without_strict_extremes() {
        let merged = vec![k(0, 10.0, 9.0), k(1, 10.0, 9.0), k(2, 10.0, 9.0)];
        assert!(find_fractals(&merged).is_empty());
    }
}
