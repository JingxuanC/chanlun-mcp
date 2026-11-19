//! 一买 / 二买识别（对齐 Python `find_buy_signals`）。
//!
//! ## 一买（底背驰）
//! 遍历每段下跌笔 `i`：
//! - `divergence_mode = adjacent`：进入段 = `bis[i-2]`（必须是下跌笔，即相邻两段下跌笔中间隔一上涨），
//!   后段创新低 `leave_low < enter_low`（`require_new_low`），后段力度衰竭
//!   `momentum(leave) < momentum(enter) && momentum(enter) > 0` -> B1。
//! - `divergence_mode = pivot`：进入段由 [`pivot_entry_for_leave`] 决定（围绕同一中枢的进入段 vs 离开段），
//!   Python 见 `code/chanlun.py:456-504`。
//! - 信号 bar = leave 的底分型 `end_raw`；`price = low = bi_low(leave)`。
//!
//! ## 二买（策略入场信号）
//! 对每个 B1：取其后第 1 笔为反弹（`bounce`，up）、第 2 笔为回调（`pullback`，down）：
//! - 回调低点不破一买低点（容差 `tolerance_pct`）：`pull_low >= first_low * (1 - tolerance_pct)`；
//! - 回调力度 < 反弹力度（`use_momentum_weak_for_b2`）：`pullback.momentum < bounce.momentum`；
//! - 满足 -> 在 `pullback` 的底分型处标 B2。
//!
//! 去重：按触发笔 `end_idx`（Python `buy_point_index`，本结构映射为 `bi_idx`）只保留最早一次，
//! 再按信号 bar 原始下标（Python `index`）排序。
//!
//! `zhongshu_idx` 映射说明：Python 信号里只有可读字符串 `zhongshu_info`
//! （adjacent 模式来自 `_find_zhongshu_info` 的"包含该笔的中枢 / 最近中枢"，
//! pivot 模式直接是配对中枢）。`BuySignal.zhongshu_idx` 是 Python `zhongshu_info`
//! 指代中枢在 `zhongshus` 数组中的下标：pivot -> 配对的 `zs0`；adjacent -> `_find_zhongshu_info`
//! 选择的那个中枢（先找包含，再找最近）；二买沿用其前置一买的值。

use crate::types::{
    AnalyzerConfig, Bi, BuySignal, Direction, DivergenceMode, SignalKind, Zhongshu,
};
use crate::zhongshu::{bi_high, bi_low};

/// 对齐 Python `_find_zhongshu_info` 的中枢选择：先找"包含该笔下标"的中枢，
/// 否则取时间上最近（`min(|bi_idx-start|, |bi_idx-end|)` 最小，严格 `<` 保留先出现者）。
fn find_zhongshu_index(bi_idx: usize, zhongshus: &[Zhongshu]) -> Option<usize> {
    for (k, zs) in zhongshus.iter().enumerate() {
        if zs.start_idx <= bi_idx && bi_idx <= zs.end_idx {
            return Some(k);
        }
    }
    let mut best: Option<usize> = None;
    let mut best_dist = usize::MAX;
    for (k, zs) in zhongshus.iter().enumerate() {
        let d_start = bi_idx.abs_diff(zs.start_idx);
        let d_end = bi_idx.abs_diff(zs.end_idx);
        let d = d_start.min(d_end);
        if d < best_dist {
            best_dist = d;
            best = Some(k);
        }
    }
    best
}

/// 严格缠论背驰配对：为离开段 `leave_idx` 找到"围绕同一中枢"的进入段。
///
/// 返回 `(进入段笔下标, 中枢下标)`；不构成严格结构返回 `None`。
/// 对应 Python `_pivot_entry_for_leave`：
/// - leave 必须是某中枢结束后**第一笔**跌破 `ZD`（向下） / 升破 `ZG`（向上）的同向笔；
/// - 进入段 = 中枢起点之前第一个同向笔（最多回退 2 步，笔方向交替）；
/// - 多个中枢满足时取最近（时间上最后）的一个。
pub(crate) fn pivot_entry_for_leave(
    bis: &[Bi],
    zhongshus: &[Zhongshu],
    leave_idx: usize,
) -> Option<(usize, usize)> {
    if leave_idx >= bis.len() {
        return None;
    }
    let leave = &bis[leave_idx];
    let mut best: Option<(usize, usize)> = None;
    for (zsi, zs) in zhongshus.iter().enumerate() {
        if zs.end_idx >= leave_idx {
            continue; // 中枢必须在离开段之前结束
        }
        if leave.direction == Direction::Down {
            // leave 必须是该中枢结束后第一笔跌破 ZD 的下跌笔。
            let mut is_first_break = true;
            for j in (zs.end_idx + 1)..leave_idx {
                if bis[j].direction == Direction::Down && bi_low(&bis[j]) < zs.zd {
                    is_first_break = false;
                    break;
                }
            }
            if !is_first_break {
                continue;
            }
            if bi_low(leave) >= zs.zd {
                continue; // leave 本身未跌破 ZD
            }
        } else {
            let mut is_first_break = true;
            for j in (zs.end_idx + 1)..leave_idx {
                if bis[j].direction == Direction::Up && bi_high(&bis[j]) > zs.zg {
                    is_first_break = false;
                    break;
                }
            }
            if !is_first_break {
                continue;
            }
            if bi_high(leave) <= zs.zg {
                continue; // leave 本身未升破 ZG
            }
        }
        // 进入段：中枢起点之前第一个同向笔（最多回退 2 步，笔方向交替）。
        // Python: for k in range(zs.start_idx - 1, max(-1, zs.start_idx - 3), -1)
        // 等价于 k = start-1, start-2（且 k >= 0）。
        for k in (zs.start_idx.saturating_sub(2)..zs.start_idx).rev() {
            if bis[k].direction == leave.direction {
                best = Some((k, zsi)); // 按时间序遍历，最后满足者 = 最近中枢
                break;
            }
        }
    }
    best
}

/// 一买/二买扫描（对应 Python `find_buy_signals` 前半段：仅 B1 的收集与去重）。
fn find_b1_signals(bis: &[Bi], zhongshus: &[Zhongshu], cfg: &AnalyzerConfig) -> Vec<BuySignal> {
    let mut b1_signals: Vec<BuySignal> = Vec::new();
    let n = bis.len();
    for i in 0..n {
        let leave = &bis[i];
        if leave.direction != Direction::Down {
            continue;
        }
        let (enter_idx, zs_idx) = if cfg.divergence_mode == DivergenceMode::Pivot {
            match pivot_entry_for_leave(bis, zhongshus, i) {
                Some(pair) => (pair.0, Some(pair.1)),
                None => continue,
            }
        } else {
            // 进入段 = 前一段下跌笔（隔一段上涨）。
            if i < 2 || bis[i - 2].direction != Direction::Down {
                continue;
            }
            (i - 2, find_zhongshu_index(i, zhongshus))
        };
        let enter = &bis[enter_idx];
        let m1 = enter.momentum;
        let m2 = leave.momentum;
        let enter_low = bi_low(enter);
        let leave_low = bi_low(leave);
        // 创新低。
        if cfg.require_new_low && !(leave_low < enter_low) {
            continue;
        }
        // 力度减弱。
        if !(m2 < m1 && m1 > 0.0) {
            continue;
        }
        b1_signals.push(BuySignal {
            kind: SignalKind::B1,
            bi_idx: i,
            raw_index: leave.end_raw,
            date: leave.end_date,
            price: leave_low,
            low: leave_low,
            first_buy_bi_idx: Some(i),
            zhongshu_idx: zs_idx,
        });
    }
    // Python `_dedupe_signals`：按 buy_point_index（= 触发笔 end_idx）去重保留最早，再按 index 排序。
    dedupe_and_sort(b1_signals)
}

/// 按 `bi_idx`（唯一触发笔）去重保留最早，再按原始 bar 下标稳定排序。
fn dedupe_and_sort(mut signals: Vec<BuySignal>) -> Vec<BuySignal> {
    let mut seen: Vec<usize> = Vec::with_capacity(signals.len());
    signals.retain(|s| {
        if seen.contains(&s.bi_idx) {
            false
        } else {
            seen.push(s.bi_idx);
            true
        }
    });
    signals.sort_by_key(|s| s.raw_index);
    signals
}

/// 识别一买与二买。返回 `(b1, b2)`，均按信号时间升序。
pub fn find_buy_signals(
    bis: &[Bi],
    zhongshus: &[Zhongshu],
    cfg: &AnalyzerConfig,
) -> (Vec<BuySignal>, Vec<BuySignal>) {
    let b1_signals = find_b1_signals(bis, zhongshus, cfg);
    let n = bis.len();
    let mut b2_signals: Vec<BuySignal> = Vec::new();
    for b1 in &b1_signals {
        // 一买笔 = 离开段下跌笔（Python 通过 bis[i].end_idx == b1.bi_idx 反查；本结构直接记录 bis 下标）。
        let down_idx = b1.bi_idx;
        // 其后：上涨反弹笔 + 回调下跌笔。
        if down_idx + 2 >= n {
            continue;
        }
        let bounce = &bis[down_idx + 1];
        let pullback = &bis[down_idx + 2];
        if !(bounce.direction == Direction::Up && pullback.direction == Direction::Down) {
            continue;
        }
        let pull_low = bi_low(pullback);
        let first_low = b1.low;
        // 回调低点不破一买低点（容差）。
        if pull_low < first_low * (1.0 - cfg.tolerance_pct) {
            continue;
        }
        // 可选：回调笔动能衰竭（回调力度 < 反弹力度）。
        if cfg.use_momentum_weak_for_b2 && pullback.momentum >= bounce.momentum {
            continue;
        }
        b2_signals.push(BuySignal {
            kind: SignalKind::B2,
            bi_idx: down_idx + 2,
            raw_index: pullback.end_raw,
            date: pullback.end_date,
            price: pull_low,
            low: pull_low,
            first_buy_bi_idx: Some(down_idx),
            zhongshu_idx: b1.zhongshu_idx,
        });
    }
    (b1_signals, dedupe_and_sort(b2_signals))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Direction, SignalKind};

    /// 构造一笔；`start_idx/end_idx` 按数组位置填充，`raw_index` 取 4 倍便于区分。
    fn bi(i: usize, start: f64, end: f64, momentum: f64, is_confirmed: bool) -> Bi {
        let direction = if end < start {
            Direction::Down
        } else {
            Direction::Up
        };
        Bi {
            start_idx: i,
            end_idx: i,
            start_raw: i * 4,
            end_raw: i * 4 + 4,
            start_date: 20240101,
            end_date: 20240105,
            start_price: start,
            end_price: end,
            direction,
            momentum,
            is_confirmed,
        }
    }

    #[test]
    fn adjacent_bottom_divergence_yields_b1_then_b2() {
        // 0: up 10->12, 1: down 12->9(m=5), 2: up 9->11, 3: down 11->8(m=2, 创新低且力度弱) => B1
        // 4: up 8->10, 5: down 10->8.2(m=1 < bounce m=3, 低点 >= 8*0.995)               => B2
        let bis = vec![
            bi(0, 10.0, 12.0, 4.0, true),
            bi(1, 12.0, 9.0, 5.0, true),
            bi(2, 9.0, 11.0, 3.0, true),
            bi(3, 11.0, 8.0, 2.0, true),
            bi(4, 8.0, 10.0, 3.0, true),
            bi(5, 10.0, 8.2, 1.0, false),
        ];
        let cfg = AnalyzerConfig::default();
        let (b1, b2) = find_buy_signals(&bis, &[], &cfg);
        assert_eq!(b1.len(), 1);
        assert_eq!(b1[0].kind, SignalKind::B1);
        assert_eq!(b1[0].bi_idx, 3);
        assert_eq!(b1[0].raw_index, 3 * 4 + 4);
        assert!((b1[0].price - 8.0).abs() < 1e-12);
        assert_eq!(b1[0].first_buy_bi_idx, Some(3));
        assert_eq!(b2.len(), 1);
        assert_eq!(b2[0].kind, SignalKind::B2);
        assert_eq!(b2[0].bi_idx, 5);
        assert_eq!(b2[0].first_buy_bi_idx, Some(3));
        assert_eq!(b2[0].zhongshu_idx, None);
    }

    #[test]
    fn b2_rejected_when_pullback_breaks_first_low() {
        // 回调低点 7.5 < 一买低点 8.0 * (1-0.005) = 7.96 -> 拒绝
        let bis = vec![
            bi(0, 10.0, 12.0, 4.0, true),
            bi(1, 12.0, 9.0, 5.0, true),
            bi(2, 9.0, 11.0, 3.0, true),
            bi(3, 11.0, 8.0, 2.0, true),
            bi(4, 8.0, 10.0, 3.0, true),
            bi(5, 10.0, 7.5, 1.0, false),
        ];
        let cfg = AnalyzerConfig::default();
        let (b1, b2) = find_buy_signals(&bis, &[], &cfg);
        // 回调笔本身也创新低且力度更弱，因此又是一个 B1（adjacent 模式的固有行为）；
        // 但它的低点跌破一买低点容差，B2 被拒绝。
        assert_eq!(b1.len(), 2);
        assert!(b2.is_empty());
    }
}
