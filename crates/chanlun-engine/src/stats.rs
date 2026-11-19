//! 绩效统计（逐字段对齐 Python `code/_gen_doc_stats.py` + `_gen_doc_stats2.py`）。
//!
//! 口径要点：
//!
//! - `years = (末日期 - 首日期).days / 365.25`（**日历天数**，不是交易日/244；
//!   注意 Python 回测脚本自己打印的「年化」用交易日/244，两套口径不同，这里跟 stats.json 走）；
//! - `cagr = (nav_end/nav0)^(1/years) - 1`；
//! - 最大回撤 `max_dd = min(nav/cummax(nav)-1)`（负数）；
//! - 只统计**真实成交**：`reason != "估值平仓（非成交）"`；
//! - 胜率/均值/中位数/盈亏比/利润因子按单笔 `return_pct`（已按 Python 规则 round 到 2dp）；
//! - `profit_factor = Σ(return_pct×qty×buy_price)_{盈利} / |Σ(...)_{亏损}|`；
//! - 分年收益 = 年末净值 / 上年末净值 − 1（首年用本年内首日净值；与 stats.json 一致）；
//! - 前 5 大回撤段按 `_gen_doc_stats2.py` 的逐段算法（段内 min < -2% 才入选）；
//! - **前 5% 交易利润占比**：按单笔利润 `return_pct×qty×buy_price` 降序取
//!   `ceil(n×5%)` 笔，占全部交易利润之和（含亏损，即净利润）的比例。
//!   与文档 review-notes 的 74.3% / 前 1% 27% 同口径（已用基线 CSV 复核）。

use std::collections::{BTreeMap, BTreeSet};

use chanlun_core::Date8;

use crate::{
    DrawdownSegment, ExitStat, NavPoint, Stats, Trade, VALUATION_REASON, YearlyStat, py_round,
};

/// 两个 `YYYYMMDD` 之间的日历天数（`b - a`，可负）。
///
/// 用 Howard Hinnant 的 `days_from_civil` 算法，避免引入 chrono 依赖。
pub fn days_between(a: Date8, b: Date8) -> i64 {
    days_from_civil(b) - days_from_civil(a)
}

fn days_from_civil(date: Date8) -> i64 {
    let y = (date / 10_000) as i64;
    let m = ((date / 100) % 100) as i64;
    let d = (date % 100) as i64;
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// 样本标准差（ddof=1，Pandas 默认口径）。
fn std_dev(xs: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n - 1) as f64;
    var.sqrt()
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// 单笔利润（与 `_gen_doc_stats.py` gross_win/gross_lose 同口径，单位是「%×元」，
/// 仅用于比值，不做会计金额解释）。
fn trade_profit(t: &Trade) -> f64 {
    t.return_pct * t.qty as f64 * t.buy_price
}

/// 计算绩效统计。`nav` 为按日净值序列（升序），`trades` 为全部成交（含估值平仓）。
pub fn compute_stats(trades: &[Trade], nav: &[NavPoint]) -> Stats {
    let mut s = Stats::default();

    // ---- 净值类 ----
    if !nav.is_empty() {
        let v: Vec<f64> = nav.iter().map(|p| p.nav).collect();
        s.days = v.len();
        s.nav0 = v[0];
        s.nav_end = *v.last().unwrap();
        s.total_ret = s.nav_end / s.nav0 - 1.0;
        s.years = days_between(nav[0].date, nav[nav.len() - 1].date) as f64 / 365.25;
        s.cagr = if s.years > 0.0 {
            (s.nav_end / s.nav0).powf(1.0 / s.years) - 1.0
        } else {
            s.total_ret
        };

        // 回撤序列
        let mut dd: Vec<f64> = Vec::with_capacity(v.len());
        let mut running_max = v[0];
        for &x in &v {
            running_max = running_max.max(x);
            dd.push(if running_max > 0.0 {
                x / running_max - 1.0
            } else {
                0.0
            });
        }
        // numpy argmin 取第一个最小值
        let mut i_dd = 0usize;
        for (i, &d) in dd.iter().enumerate() {
            if d < dd[i_dd] {
                i_dd = i;
            }
        }
        s.max_dd = dd[i_dd];
        // 向前找峰值（第一个最大值）
        let mut peak_i = 0usize;
        for i in 1..=i_dd {
            if v[i] > v[peak_i] {
                peak_i = i;
            }
        }
        s.dd_peak_date = nav[peak_i].date;
        s.dd_trough_date = nav[i_dd].date;

        // 日收益序列（首日 0，与 pandas pct_change().fillna(0) 一致）
        let mut rets: Vec<f64> = Vec::with_capacity(v.len());
        rets.push(0.0);
        for i in 1..v.len() {
            rets.push(if v[i - 1] != 0.0 {
                v[i] / v[i - 1] - 1.0
            } else {
                0.0
            });
        }
        let ret_mean = mean(&rets);
        let ret_std = std_dev(&rets);
        s.sharpe = if ret_std > 0.0 {
            ret_mean / ret_std * 252.0_f64.sqrt()
        } else {
            0.0
        };
        let neg: Vec<f64> = rets.iter().copied().filter(|r| *r < 0.0).collect();
        let downside = std_dev(&neg);
        s.sortino = if downside > 0.0 {
            ret_mean / downside * 252.0_f64.sqrt()
        } else {
            0.0
        };
        s.calmar = if s.max_dd != 0.0 {
            s.cagr / s.max_dd.abs()
        } else {
            0.0
        };

        // 分年净值（year -> 本年内首/末净值），收益以上年末为基准
        let mut yearly_nav: BTreeMap<u32, (f64, f64)> = BTreeMap::new();
        for p in nav {
            let year = p.date / 10_000;
            yearly_nav
                .entry(year)
                .and_modify(|e| e.1 = p.nav)
                .or_insert((p.nav, p.nav));
        }

        // 月度胜率：月末净值序列的 pct_change > 0 占比
        let mut month_last: BTreeMap<u32, f64> = BTreeMap::new();
        for p in nav {
            month_last.insert(p.date / 100, p.nav);
        }
        let month_vals: Vec<f64> = month_last.values().copied().collect();
        let mut month_rets: Vec<f64> = Vec::new();
        for i in 1..month_vals.len() {
            if month_vals[i - 1] != 0.0 {
                month_rets.push(month_vals[i] / month_vals[i - 1] - 1.0);
            }
        }
        s.month_win = if month_rets.is_empty() {
            0.0
        } else {
            month_rets.iter().filter(|r| **r > 0.0).count() as f64 / month_rets.len() as f64
        };

        // ---- 分年收益 ----
        let mut yearly: Vec<YearlyStat> = Vec::new();
        let mut prev_last: Option<f64> = None;
        for (year, (first, last)) in &yearly_nav {
            let start = prev_last.unwrap_or(*first);
            let ret = if start != 0.0 {
                last / start - 1.0
            } else {
                0.0
            };
            yearly.push(YearlyStat {
                year: *year,
                ret,
                nav_end: *last,
                trades: 0, // 下面按真实成交回填
            });
            prev_last = Some(*last);
        }

        // ---- 前 5 大回撤段（_gen_doc_stats2.py）----
        s.top_drawdowns = top_drawdowns(nav, &v, &dd);

        // 先存起来，等真实成交统计完再回填 trades
        let mut yearly = std::mem::take(&mut yearly);

        // ---- 真实成交 ----
        let real: Vec<&Trade> = trades
            .iter()
            .filter(|t| t.reason != VALUATION_REASON)
            .collect();
        s.n_mark = trades.len() - real.len();
        s.n_trades = real.len();
        s.n_codes = real
            .iter()
            .map(|t| t.code.as_str())
            .collect::<BTreeSet<_>>()
            .len();

        if !real.is_empty() {
            let rets: Vec<f64> = real.iter().map(|t| t.return_pct).collect();
            let wins: Vec<f64> = rets.iter().copied().filter(|r| *r > 0.0).collect();
            let loses: Vec<f64> = rets.iter().copied().filter(|r| *r <= 0.0).collect();

            s.win_rate = wins.len() as f64 / rets.len() as f64;
            s.avg_ret = mean(&rets);
            s.expectancy = s.avg_ret;
            s.median_ret = median(&rets);
            s.avg_win = mean(&wins);
            s.avg_lose = mean(&loses);
            s.pl_ratio = if s.avg_lose != 0.0 {
                (s.avg_win / s.avg_lose).abs()
            } else {
                0.0
            };

            let gross_win: f64 = real
                .iter()
                .filter(|t| t.return_pct > 0.0)
                .map(|t| trade_profit(t))
                .sum();
            let gross_lose: f64 = real
                .iter()
                .filter(|t| t.return_pct <= 0.0)
                .map(|t| trade_profit(t))
                .sum();
            s.profit_factor = if gross_lose != 0.0 {
                gross_win / gross_lose.abs()
            } else {
                0.0
            };

            // best / worst（pandas idxmax/idxmin 取第一个出现）
            let mut best_i = 0usize;
            let mut worst_i = 0usize;
            for (i, t) in real.iter().enumerate() {
                if t.return_pct > real[best_i].return_pct {
                    best_i = i;
                }
                if t.return_pct < real[worst_i].return_pct {
                    worst_i = i;
                }
            }
            s.best = real[best_i].return_pct;
            s.best_code = real[best_i].code.clone();
            s.best_range = format!("{}→{}", real[best_i].buy_date, real[best_i].sell_date);
            s.worst = real[worst_i].return_pct;
            s.worst_code = real[worst_i].code.clone();

            let holds: Vec<f64> = real.iter().map(|t| t.hold_days as f64).collect();
            s.hold_avg = mean(&holds);
            s.hold_med = median(&holds);

            // 利润集中度：前 5% / 前 1% 交易利润占比
            let profits: Vec<f64> = real.iter().map(|t| trade_profit(t)).collect();
            s.top5pct_profit_share = top_profit_share(&profits, 0.05);
            s.top1pct_profit_share = top_profit_share(&profits, 0.01);

            // 离场原因分类统计
            let mut exits: BTreeMap<String, (usize, f64, usize)> = BTreeMap::new();
            for t in &real {
                let cls = classify_exit(&t.reason);
                let e = exits.entry(cls).or_insert((0, 0.0, 0));
                e.0 += 1;
                e.1 += t.return_pct;
                if t.return_pct > 0.0 {
                    e.2 += 1;
                }
            }
            s.exits = exits
                .into_iter()
                .map(|(reason, (n, sum, n_win))| ExitStat {
                    reason,
                    n,
                    avg_ret: sum / n as f64,
                    win_rate: n_win as f64 / n as f64,
                })
                .collect();

            // 回填分年成交笔数
            for y in &mut yearly {
                y.trades = real
                    .iter()
                    .filter(|t| t.buy_date / 10_000 == y.year)
                    .count();
            }
        }

        s.n_years = yearly.len();
        s.years_pos = yearly.iter().filter(|y| y.ret > 0.0).count();
        s.yearly = yearly;
    } else {
        // 空净值序列：只做成交侧统计（与 Python 会报错不同，这里保持 API 稳定）
        s.n_mark = trades
            .iter()
            .filter(|t| t.reason == VALUATION_REASON)
            .count();
        s.n_trades = trades.len() - s.n_mark;
    }

    s
}

fn top_profit_share(profits: &[f64], pct: f64) -> f64 {
    if profits.is_empty() {
        return 0.0;
    }
    let mut v = profits.to_vec();
    v.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let k = ((v.len() as f64 * pct).ceil() as usize).clamp(1, v.len());
    let total: f64 = v.iter().sum();
    if total == 0.0 {
        return 0.0;
    }
    v[..k].iter().sum::<f64>() / total
}

fn classify_exit(reason: &str) -> String {
    if reason.starts_with("跌破中枢ZD") {
        "跌破中枢ZD".to_string()
    } else if reason.contains("顶背驰") {
        "顶背驰".to_string()
    } else {
        reason.to_string()
    }
}

/// 全期前 5 大回撤段（峰到谷），逐段算法见 `_gen_doc_stats2.py`。
fn top_drawdowns(nav: &[NavPoint], v: &[f64], dd: &[f64]) -> Vec<DrawdownSegment> {
    let n = nav.len();
    if n == 0 {
        return Vec::new();
    }
    let mut segs: Vec<(usize, f64)> = Vec::new();
    let mut start = 0usize;
    for j in 1..n {
        // 创新高：cummax[j-1] 即 j 之前的运行最大值
        let prev_cummax = v[..j].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if v[j] >= prev_cummax {
            if j - 1 > start {
                let seg = &dd[start..j];
                if let Some((k, mn)) = min_with_index(seg) {
                    if mn < -0.02 {
                        segs.push((start + k, mn));
                    }
                }
            }
            start = j;
        }
    }
    if start < n {
        let seg = &dd[start..];
        if let Some((k, mn)) = min_with_index(seg) {
            if mn < -0.02 {
                segs.push((start + k, mn));
            }
        }
    }
    segs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    segs.truncate(5);

    segs.into_iter()
        .map(|(ti, d)| {
            // 向前找峰值（第一个最大值）
            let mut pi = 0usize;
            for i in 1..=ti {
                if v[i] > v[pi] {
                    pi = i;
                }
            }
            DrawdownSegment {
                peak: nav[pi].date,
                trough: nav[ti].date,
                dd: d,
                days: ti - pi,
            }
        })
        .collect()
}

fn min_with_index(xs: &[f64]) -> Option<(usize, f64)> {
    let mut out: Option<(usize, f64)> = None;
    for (i, &x) in xs.iter().enumerate() {
        match out {
            None => out = Some((i, x)),
            Some((_, m)) if x < m => out = Some((i, x)),
            _ => {}
        }
    }
    out
}

/// 便捷：按 Python `round(x, ndigits)` 语义四舍五入（half-even，作用于真实二进制值）。
pub fn round_like_python(x: f64, ndigits: usize) -> f64 {
    py_round(x, ndigits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_day_count() {
        assert_eq!(days_between(20230101, 20240101), 365);
        assert_eq!(days_between(20240101, 20250101), 366); // 2024 闰年
        assert_eq!(days_between(20240101, 20240101), 0);
        assert_eq!(days_between(20240102, 20240101), -1);
        assert_eq!(days_between(20200228, 20200301), 2); // 2020 闰年 2 月 29 日
    }

    fn trade(
        code: &str,
        buy: Date8,
        sell: Date8,
        ret: f64,
        qty: i64,
        px: f64,
        reason: &str,
    ) -> Trade {
        Trade {
            code: code.to_string(),
            buy_date: buy,
            sell_date: sell,
            buy_price: px,
            sell_price: px * (1.0 + ret / 100.0),
            qty,
            return_pct: ret,
            reason: reason.to_string(),
            hold_days: days_between(buy, sell).max(0) as u32,
        }
    }

    #[test]
    fn stats_formula_matches_doc_caliber() {
        // 净值：1.0 -> 1.2 -> 0.9（20230101 / 20230701 / 20240101）
        let nav = vec![
            NavPoint {
                date: 20230101,
                nav: 1.0,
            },
            NavPoint {
                date: 20230701,
                nav: 1.2,
            },
            NavPoint {
                date: 20240101,
                nav: 0.9,
            },
        ];
        // 4 笔真实成交；每笔 qty*price = 10000 元，利润 = ret%*10000
        let trades = vec![
            trade("600000", 20230201, 20230210, 10.0, 1000, 10.0, "跌破MA13"),
            trade("600001", 20230301, 20230310, -5.0, 1000, 10.0, "止损"),
            trade("600002", 20230801, 20230810, 20.0, 1000, 10.0, "跌破MA13"),
            trade("600003", 20240101, 20240102, -2.0, 1000, 10.0, "跌破MA13"),
        ];
        let s = compute_stats(&trades, &nav);

        assert_eq!(s.days, 3);
        assert_eq!(s.n_trades, 4);
        assert_eq!(s.n_mark, 0);
        assert_eq!(s.n_codes, 4);
        // years = 365/365.25
        assert!((s.years - 365.0 / 365.25).abs() < 1e-12);
        assert!((s.total_ret - (0.9 - 1.0)).abs() < 1e-12);
        let expected_cagr = 0.9_f64.powf(1.0 / s.years) - 1.0;
        assert!((s.cagr - expected_cagr).abs() < 1e-12);
        assert!((s.max_dd - (0.9 / 1.2 - 1.0)).abs() < 1e-12);
        assert_eq!(s.dd_peak_date, 20230701);
        assert_eq!(s.dd_trough_date, 20240101);

        // 单笔统计
        assert!((s.win_rate - 0.5).abs() < 1e-12);
        assert!((s.avg_ret - 5.75).abs() < 1e-12);
        assert!((s.median_ret - 4.0).abs() < 1e-12);
        assert!((s.avg_win - 15.0).abs() < 1e-12);
        assert!((s.avg_lose - (-3.5)).abs() < 1e-12);
        assert!((s.pl_ratio - 15.0 / 3.5).abs() < 1e-12);
        // gross_win = (10+20)*100 = 3000；gross_lose = -(5+2)*100 = -700
        assert!((s.profit_factor - 3000.0 / 700.0).abs() < 1e-12);
        assert!((s.hold_avg - 7.0).abs() < 1e-12);
        assert!((s.hold_med - 9.0).abs() < 1e-12);
        assert!((s.expectancy - s.avg_ret).abs() < 1e-12);
        // best/worst
        assert!((s.best - 20.0).abs() < 1e-12);
        assert_eq!(s.best_code, "600002");
        assert_eq!(s.best_range, "20230801→20230810");
        assert!((s.worst - (-5.0)).abs() < 1e-12);
        assert_eq!(s.worst_code, "600001");

        // 分年：2023 = 1.2/1.0-1 = 20%（首年用年内首日）；2024 = 0.9/1.2-1 = -25%
        assert_eq!(s.n_years, 2);
        assert_eq!(s.yearly[0].year, 2023);
        assert!((s.yearly[0].ret - 0.2).abs() < 1e-12);
        assert_eq!(s.yearly[0].trades, 3);
        assert_eq!(s.yearly[1].year, 2024);
        assert!((s.yearly[1].ret - (0.9 / 1.2 - 1.0)).abs() < 1e-12);
        assert_eq!(s.yearly[1].trades, 1);
        assert_eq!(s.years_pos, 1);

        // 月度胜率：月末净值 [1.0,1.2,0.9] -> 收益 [0.2,-0.25] -> 0.5
        assert!((s.month_win - 0.5).abs() < 1e-12);

        // 前 5%：k = ceil(4*0.05) = 1 -> 最大利润 2000 / 净利润 2300
        assert!((s.top5pct_profit_share - 2000.0 / 2300.0).abs() < 1e-12);
        assert!((s.top1pct_profit_share - 2000.0 / 2300.0).abs() < 1e-12);

        // 离场原因分类
        assert_eq!(s.exits.len(), 2);
        let ma13 = s.exits.iter().find(|e| e.reason == "跌破MA13").unwrap();
        assert_eq!(ma13.n, 3);
        assert!((ma13.avg_ret - (10.0 + 20.0 - 2.0) / 3.0).abs() < 1e-12);
        let stop = s.exits.iter().find(|e| e.reason == "止损").unwrap();
        assert_eq!(stop.n, 1);
        assert!((stop.win_rate - 0.0).abs() < 1e-12);

        // 前 5 大回撤段：只有一段 -25%（peak 20230701 -> trough 20240101）
        assert_eq!(s.top_drawdowns.len(), 1);
        let d = &s.top_drawdowns[0];
        assert_eq!(d.peak, 20230701);
        assert_eq!(d.trough, 20240101);
        assert!((d.dd - (0.9 / 1.2 - 1.0)).abs() < 1e-12);
        assert_eq!(d.days, 1);
    }

    #[test]
    fn valuation_trades_excluded_from_train_stats() {
        // 净值 100 -> 110；一笔真实成交 + 一笔估值平仓
        let nav = vec![
            NavPoint {
                date: 20240101,
                nav: 100.0,
            },
            NavPoint {
                date: 20240102,
                nav: 110.0,
            },
        ];
        let trades = vec![
            trade("600000", 20240101, 20240102, 10.0, 100, 10.0, "跌破MA13"),
            trade(
                "600001",
                20240101,
                20240102,
                5.0,
                100,
                10.0,
                VALUATION_REASON,
            ),
        ];
        let s = compute_stats(&trades, &nav);
        assert_eq!(s.n_trades, 1);
        assert_eq!(s.n_mark, 1);
        assert_eq!(s.n_codes, 1);
        assert!((s.win_rate - 1.0).abs() < 1e-12);
        // 估值平仓不参与离场原因统计
        assert_eq!(s.exits.len(), 1);
        assert_eq!(s.exits[0].reason, "跌破MA13");
    }

    #[test]
    fn top_drawdown_segments_pick_top5_and_threshold() {
        // 造 3 段 >2% 回撤 + 一段 1% 回撤；只取前 5、且 < -2% 才入选
        let vals = [1.0, 0.95, 1.0, 0.90, 1.0, 0.80, 1.05, 1.0, 1.10, 1.09, 1.2];
        let mut nav = Vec::new();
        let mut date = 20240101;
        for v in vals {
            nav.push(NavPoint { date, nav: v });
            date = next_day(date);
        }
        let v: Vec<f64> = nav.iter().map(|p| p.nav).collect();
        let mut running_max = v[0];
        let dd: Vec<f64> = v
            .iter()
            .map(|&x| {
                running_max = running_max.max(x);
                x / running_max - 1.0
            })
            .collect();
        let segs = top_drawdowns(&nav, &v, &dd);
        assert!(!segs.is_empty());
        assert!(segs.iter().all(|s| s.dd < -0.02));
        // 最深的 -20% 段排第一
        assert!((segs[0].dd - (0.8 / 1.0 - 1.0)).abs() < 1e-12);
        assert!(segs.len() <= 5);
    }

    fn next_day(date: u32) -> u32 {
        // 测试内简单递增（跨月处理：只在 01..28 范围内使用）
        date + 1
    }

    #[test]
    fn empty_inputs_are_stable() {
        let s = compute_stats(&[], &[]);
        assert_eq!(s.n_trades, 0);
        assert_eq!(s.nav_end, 0.0);
        assert_eq!(s.years, 0.0);
    }
}
