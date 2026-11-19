//! 卖出判定（对齐 Python `latest_sell_advice` / `sell_advice_from_result`）。
//!
//! 两个 Python 函数逻辑逐行一致（后者只是复用已算好的 `analyze` 结果、不重复计算），
//! 本模块实现其共同逻辑：
//!
//! 判定只看**入场后已完成（is_confirmed）的笔**：
//! `after_bis = bis.filter(is_confirmed && (end_raw >= entry_raw || start_raw >= entry_raw))`。
//!
//! - **顶背驰**：`after_bis` 中最近两段上涨笔 `latest_up`、`prev_up`；
//!   `latest_up.high > prev_up.high && latest_up.momentum < prev_up.momentum && prev_up.momentum > 0`
//!   -> `top_divergence = true`，文案 `"顶背驰(上涨笔力度衰竭);"`。
//!   `divergence_mode = pivot` 时改用 `_pivot_entry_for_leave` 找同一中枢的进入段对比，
//!   文案 `"顶背驰(同中枢zs[a..b]离开段力度衰竭);"`。
//! - **跌破中枢 ZD**：取最后一个中枢的 `zd`（Python 里 `zs_zd` 可覆盖，Rust API 无此参数，
//!   等价于 Python 传 `zs_zd=None`；
//!   - `break_zd_confirm = false`：`after_bis` 中最后一段下跌笔的低点 `< zd`
//!     -> `"跌破中枢ZD(12.520);"`（3 位小数 + 分号；
//!   - `break_zd_confirm = true`（严格第三类卖点）：对每个跌破 ZD 的已完成下跌笔，
//!     找其后第一笔已完成上涨笔（回抽），回抽高点 `< zd` 才确认 -> `"三卖确认(破ZD(12.520)后回抽不回中枢);"`；
//!     回抽回到中枢内视为假跌破继续检查后续破位笔；回抽尚未形成则等待（不触发）。
//!     该分支结束时直接返回（与 Python 相同：不再走默认"跌破即触发"逻辑）。
//! - 多条命中时按 Python 顺序拼接：先顶背驰、再 ZD 相关文案。
//!
//! `entry_price` 在 Python 中同样未参与判定（仅文档用途）；`window_bars` 由调用方负责截窗
//! （engine 已把 `analysis` 截成窗口内的结果），本函数忽略该参数。

use crate::signals::pivot_entry_for_leave;
use crate::types::{Analysis, AnalyzerConfig, Direction, DivergenceMode, SellAdvice};
use crate::zhongshu::{bi_high, bi_low};

/// 基于已完成分析判断离场。
///
/// * `analysis` - 窗口内的完整分析结果（`bis[*].start_raw/end_raw` 是相对窗口起点的偏移）；
/// * `entry_price` - 持仓成本价（与 Python 一致：未参与判定）；
/// * `entry_raw` - 入场 bar 相对窗口起点的偏移（负数调用方已钳为 0）；
/// * `window_bars` - 调用方已截好的窗口长度（本函数忽略，保留签名以对齐契约）；
/// * `cfg` - 分析配置（`divergence_mode` / `break_zd_confirm` 决定分支）。
pub fn latest_sell_advice(
    analysis: &Analysis,
    _entry_price: f64,
    entry_raw: usize,
    _window_bars: usize,
    cfg: &AnalyzerConfig,
) -> SellAdvice {
    let mut info = SellAdvice::default();
    let bis = &analysis.bis;
    if bis.len() < 2 {
        return info;
    }

    // 入场后完成的笔（顺序与 bis 一致，下标指向 bis）。
    let after: Vec<usize> = (0..bis.len())
        .filter(|&k| {
            bis[k].is_confirmed && (bis[k].end_raw >= entry_raw || bis[k].start_raw >= entry_raw)
        })
        .collect();
    if after.is_empty() {
        return info;
    }

    // —— 顶背驰（入场后最近两段上涨笔）——
    let up_after: Vec<usize> = after
        .iter()
        .copied()
        .filter(|&k| bis[k].direction == Direction::Up)
        .collect();
    if up_after.len() >= 2 {
        let latest_up_idx = *up_after.last().expect("len>=2");
        let prev_up_idx = up_after[up_after.len() - 2];
        if cfg.divergence_mode == DivergenceMode::Pivot {
            // 严格缠论：最新上涨笔须为某中枢的"离开段"，与"进入段"（同中枢）比力度。
            if let Some((enter_idx, zs_idx)) =
                pivot_entry_for_leave(bis, &analysis.zhongshus, latest_up_idx)
            {
                let enter_up = &bis[enter_idx];
                let latest_up = &bis[latest_up_idx];
                if bi_high(latest_up) > bi_high(enter_up)
                    && latest_up.momentum < enter_up.momentum
                    && enter_up.momentum > 0.0
                {
                    info.top_divergence = true;
                    let zs = &analysis.zhongshus[zs_idx];
                    info.note.push_str(&format!(
                        "顶背驰(同中枢zs[{}..{}]离开段力度衰竭);",
                        zs.start_idx, zs.end_idx
                    ));
                }
            }
        } else {
            let latest_up = &bis[latest_up_idx];
            let prev_up = &bis[prev_up_idx];
            if bi_high(latest_up) > bi_high(prev_up)
                && latest_up.momentum < prev_up.momentum
                && prev_up.momentum > 0.0
            {
                info.top_divergence = true;
                info.note.push_str("顶背驰(上涨笔力度衰竭);");
            }
        }
    }

    // —— 跌破中枢 ZD ——
    let Some(zs_obj) = analysis.zhongshus.last() else {
        return info;
    };
    let zs_zd_val = zs_obj.zd;
    let down_after: Vec<usize> = after
        .iter()
        .copied()
        .filter(|&k| bis[k].direction == Direction::Down)
        .collect();

    if cfg.break_zd_confirm {
        // 严格第三类卖点（回抽确认）：任何时点触发数 <= 默认模式。
        let breaks: Vec<usize> = down_after
            .iter()
            .copied()
            .filter(|&k| bi_low(&bis[k]) < zs_zd_val)
            .collect();
        for bk in breaks {
            // 只取该破位笔之后第一次回抽（已完成上涨笔）。
            let pull = after.iter().copied().find(|&k| {
                bis[k].direction == Direction::Up
                    && bis[k].is_confirmed
                    && bis[k].start_raw > bis[bk].end_raw
            });
            let Some(pull) = pull else {
                continue; // 回抽尚未形成，等待确认
            };
            if bi_high(&bis[pull]) < zs_zd_val {
                info.break_zd = true;
                info.note
                    .push_str(&format!("三卖确认(破ZD({:.3})后回抽不回中枢);", zs_zd_val));
                break; // 回抽不回中枢，确认三卖
            }
            // 否则该破位是假跌破，继续检查后续破位笔。
        }
        return info;
    }

    // 默认：跌破即触发（V13.1 现有口径）。
    if let Some(&last_down) = down_after.last() {
        if bi_low(&bis[last_down]) < zs_zd_val {
            info.break_zd = true;
            info.note
                .push_str(&format!("跌破中枢ZD({:.3});", zs_zd_val));
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Analysis, Bi, Direction, Zhongshu};

    fn bi(i: usize, start: f64, end: f64, momentum: f64, confirmed: bool) -> Bi {
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
            is_confirmed: confirmed,
        }
    }

    fn zs(zd: f64, zg: f64) -> Zhongshu {
        Zhongshu {
            start_idx: 0,
            end_idx: 2,
            start_raw: 0,
            end_raw: 12,
            start_date: 20240101,
            end_date: 20240105,
            zg,
            zd,
        }
    }

    fn analysis(bis: Vec<Bi>, zhongshus: Vec<Zhongshu>) -> Analysis {
        Analysis {
            bis,
            zhongshus,
            ..Default::default()
        }
    }

    #[test]
    fn top_divergence_and_break_zd_notes() {
        // up 10->12(m=5), down 12->10, up 10->13(m=2) => 顶背驰（新高+力度弱）
        // 最后一笔 down 13->9 < ZD=10 <=> 跌破中枢
        let an = analysis(
            vec![
                bi(0, 10.0, 12.0, 5.0, true),
                bi(1, 12.0, 10.0, 2.0, true),
                bi(2, 10.0, 13.0, 2.0, true),
                bi(3, 13.0, 9.0, 3.0, true),
            ],
            vec![zs(10.0, 12.0)],
        );
        let cfg = AnalyzerConfig::default();
        let adv = latest_sell_advice(&an, 11.0, 0, 100, &cfg);
        assert!(adv.top_divergence);
        assert!(adv.break_zd);
        assert_eq!(adv.note, "顶背驰(上涨笔力度衰竭);跌破中枢ZD(10.000);");
    }

    #[test]
    fn entry_filter_excludes_bis_before_entry() {
        let an = analysis(
            vec![
                bi(0, 10.0, 12.0, 5.0, true),
                bi(1, 12.0, 10.0, 2.0, true),
                bi(2, 10.0, 13.0, 2.0, true),
                bi(3, 13.0, 9.0, 3.0, true),
            ],
            vec![zs(10.0, 12.0)],
        );
        let cfg = AnalyzerConfig::default();
        // entry_raw = 12（bi1 的 end_raw）之后，只有 bi2/bi3 参与 => 仅 1 段上涨笔，无顶背驰
        let adv = latest_sell_advice(&an, 11.0, 12, 100, &cfg);
        assert!(!adv.top_divergence);
        assert!(adv.break_zd);
    }

    #[test]
    fn break_zd_confirm_waits_for_pullback() {
        // 破位笔下跌至 9（<ZD 10），其后没有回抽上涨笔 => 等待，不触发
        let an = analysis(
            vec![bi(0, 10.0, 12.0, 5.0, true), bi(1, 12.0, 9.0, 3.0, true)],
            vec![zs(10.0, 12.0)],
        );
        let cfg = AnalyzerConfig {
            break_zd_confirm: true,
            ..AnalyzerConfig::default()
        };
        let adv = latest_sell_advice(&an, 11.0, 0, 100, &cfg);
        assert!(!adv.break_zd);
        assert!(adv.note.is_empty());

        // 加入回抽上涨笔（start_raw 在破位笔之后，高点 9.5 < ZD 10）=> 三卖确认
        let an2 = analysis(
            vec![
                bi(0, 10.0, 12.0, 5.0, true),
                bi(1, 12.0, 9.0, 3.0, true),
                bi(3, 9.0, 9.5, 1.0, true),
            ],
            vec![zs(10.0, 12.0)],
        );
        let adv2 = latest_sell_advice(&an2, 11.0, 0, 100, &cfg);
        assert!(adv2.break_zd);
        assert_eq!(adv2.note, "三卖确认(破ZD(10.000)后回抽不回中枢);");
    }
}
