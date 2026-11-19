//! 笔构建（对齐 Python `find_bis`）。
//!
//! Python 算法（`code/chanlun.py:233-339`），逐行移植：
//!
//! 1. **refine**：按时间顺序过滤分型，同类型只保留更极端者（顶取更高、底取更低），保证顶底交替。
//!    注意同类型是**原地替换** `refined[-1]`（不是丢弃后一个/保留第一个）。
//! 2. **第一笔**：从 `refined[0]` 出发向后找第一个与起点反向、且 `idx - prev.idx >= min_bars_between_fractals`
//!    的分型作为终点。
//! 3. **递推**：
//!    - 若当前分型与当前笔末端同类型：更极端（顶更高/底更低）则**延伸**上一笔终点
//!      （更新 end_idx/end_raw/end_date/end_price，`is_confirmed = i < len-1`），否则忽略；
//!      **注意：延伸不检查最小 K 线间距**（Python 原样如此）。
//!    - 若反向且满足最小 K 线距：确认上一笔（`is_confirmed = true`）并开新笔。
//!    - 若反向但不满足最小 K 线距：跳过该分型。
//! 4. 最后一个分型可能只形成"未确认笔"——实盘扫描依赖它，回测里靠 `g+4` 重算确认来消除不确定性。
//!
//! 数据映射：`Bi.start_idx/end_idx` = 合并 K 线下标；`start_raw/end_raw` = 原始 bar 下标；
//! `start_price` = 起点分型高点(顶)/低点(底)；`end_price` = 终点低点(底)/高点(顶)。

use crate::types::{AnalyzerConfig, Bi, Direction, Fractal, FractalKind, MergedKline};

/// 用两个分型构造一笔（对齐 Python 内部 `_make_bi`）。
///
/// 起点类型决定方向与起点价，终点类型决定终点价（底分型取 low、顶分型取 high）。
fn make_bi(a: &Fractal, b: &Fractal) -> Bi {
    let direction = if a.kind == FractalKind::Top {
        Direction::Down
    } else {
        Direction::Up
    };
    let start_price = match a.kind {
        FractalKind::Top => a.high,
        FractalKind::Bottom => a.low,
    };
    let end_price = match b.kind {
        FractalKind::Bottom => b.low,
        _ => b.high,
    };
    Bi {
        start_idx: a.idx,
        end_idx: b.idx,
        start_raw: a.raw_index,
        end_raw: b.raw_index,
        start_date: a.date,
        end_date: b.date,
        start_price,
        end_price,
        direction,
        momentum: 0.0,
        is_confirmed: false,
    }
}

/// 在分型序列上构造笔（新笔定义）。
///
/// `_merged` 仅为对齐 Python `find_bis(merged, fractals)` 的签名而保留：Python 实现体
/// 也完全没用到 `merged`（只用分型，分型本身已内嵌合并 K 线的高/低点）。
pub fn find_bis(_merged: &[MergedKline], fractals: &[Fractal], cfg: &AnalyzerConfig) -> Vec<Bi> {
    if fractals.is_empty() {
        return Vec::new();
    }
    // Python 在 __init__ 里做 max(3, int(...))；此处同样兜底（配置来自 JSON 时也安全）。
    let min_bars = cfg.min_bars_between_fractals.max(3);

    // 1) 过滤：合并同类型（保留更极端者），保证顶底交替。
    let mut refined: Vec<Fractal> = Vec::with_capacity(fractals.len());
    for f in fractals {
        if let Some(last) = refined.last_mut() {
            if f.kind == last.kind {
                match f.kind {
                    FractalKind::Top => {
                        if f.high >= last.high {
                            *last = f.clone();
                        }
                    }
                    FractalKind::Bottom => {
                        if f.low <= last.low {
                            *last = f.clone();
                        }
                    }
                }
                continue;
            }
        }
        refined.push(f.clone());
    }

    let mut bis: Vec<Bi> = Vec::new();
    let mut prev = refined[0].clone();

    // 2) 找第一笔：起点向后第一个反向且间距足够的分型。
    let mut i = 0usize;
    while i < refined.len() {
        let f = &refined[i];
        if f.idx.saturating_sub(prev.idx) >= min_bars && f.kind != prev.kind {
            bis.push(make_bi(&prev, f));
            prev = f.clone();
            i += 1;
            break;
        }
        i += 1;
    }

    // 3) 后续递推。
    while i < refined.len() {
        let f = &refined[i];
        if f.kind == prev.kind {
            // 同类型：更极端则延伸上一笔终点（延伸不检查最小间距，与 Python 一致）。
            let extend = match f.kind {
                FractalKind::Top => f.high >= prev.high,
                FractalKind::Bottom => f.low <= prev.low,
            };
            if extend {
                if let Some(last) = bis.last_mut() {
                    last.end_idx = f.idx;
                    last.end_raw = f.raw_index;
                    last.end_date = f.date;
                    last.end_price = match f.kind {
                        FractalKind::Top => f.high,
                        FractalKind::Bottom => f.low,
                    };
                    last.is_confirmed = i < refined.len() - 1;
                }
                prev = f.clone();
            }
            i += 1;
            continue;
        }
        // 反向：间距足够才确认上一笔并开新笔。
        if f.idx.saturating_sub(prev.idx) >= min_bars {
            if let Some(last) = bis.last_mut() {
                last.is_confirmed = true;
            }
            bis.push(make_bi(&prev, f));
            prev = f.clone();
        }
        i += 1;
    }

    bis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fractal::find_fractals;
    use crate::merge::merge_klines;
    use crate::types::Bar;

    fn bar(date: u32, high: f64, low: f64) -> Bar {
        Bar {
            date,
            open: (high + low) / 2.0,
            high,
            low,
            close: (high + low) / 2.0,
            volume: 1.0,
            amount: 0.0,
        }
    }

    #[test]
    fn empty_fractals_gives_no_bi() {
        let cfg = AnalyzerConfig::default();
        assert!(find_bis(&[], &[], &cfg).is_empty());
    }

    #[test]
    fn builds_unconfirmed_bi_between_alternating_fractals() {
        // 严格分型：底(1) -> 顶(5) -> 底(9)（合并 K 线与原始 bar 一一对应）。
        let highs = [
            11.0, 10.0, 11.0, 12.0, 13.0, 15.0, 14.0, 13.0, 12.0, 11.0, 12.0,
        ];
        let lows = [
            10.0, 9.0, 10.0, 11.0, 12.0, 13.5, 12.5, 11.5, 10.5, 9.5, 10.5,
        ];
        let bars: Vec<Bar> = (0..11)
            .map(|i| {
                let mut b = bar(20240101 + i as u32, highs[i], lows[i]);
                b.open = highs[i] - 0.1;
                b.close = highs[i] - 0.1;
                b
            })
            .collect();
        let merged = merge_klines(&bars);
        assert_eq!(merged.len(), bars.len(), "本用例不应发生包含合并");
        let fractals = find_fractals(&merged);
        assert_eq!(fractals.len(), 3, "底-顶-底: {fractals:?}");
        let cfg = AnalyzerConfig::default();
        let bis = find_bis(&merged, &fractals, &cfg);
        assert_eq!(bis.len(), 2);
        assert_eq!(bis[0].direction, Direction::Up);
        assert!(bis[0].is_confirmed);
        assert_eq!(bis[0].start_idx, fractals[0].idx);
        assert_eq!(bis[0].end_idx, fractals[1].idx);
        assert_eq!(bis[1].direction, Direction::Down);
        assert!(!bis[1].is_confirmed);
        assert!(bis[1].start_date < bis[1].end_date);
        assert!(bis[1].end_price < bis[1].start_price);
    }
}
