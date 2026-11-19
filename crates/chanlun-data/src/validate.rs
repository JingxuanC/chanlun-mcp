//! 多源交叉校验：同一标的、同一 bar，多个源之间对比收盘价 / bar 覆盖。
//!
//! 用途：
//! - 日常增量更新后，验证东财（前复权）/ 新浪 / 腾讯（不复权）是否一致；
//! - 数据事故（如只更新了一个交易所）时快速定位。
//!
//! ## 对齐口径（重要）
//!
//! `Bar.date` 只有 8 位日期，一天有 4 根 60min bar，所以 bar 身份用
//! **`(date, 日内序号)`**（按文件/源返回顺序编号），不能只用 `date` 建 map，
//! 否则一天 4 根会互相覆盖。
//!
//! ## 复权口径（重要）
//!
//! 历史仓 + 东财是前复权，新浪/腾讯是不复权，同名 bar 收盘价可以差一个
//! **常数缩放因子**（还可能在分红日附近分段不同）。因此不能直接比绝对收盘价，
//! 而是：
//! 1. 取重叠 bar 里**最后一根**作为锚点，算 `close_ratio = a.close / b.close`
//!    （前复权以最新价为基准，所以锚点比就是两条序列的口径比）；
//! 2. 逐 bar 计算 `a.close / close_ratio` 与 `b.close` 的差；
//! 3. 判定：绝对差 > max(容差 bps 对应的价格差, 1 个最小价位 0.01 元) 才告警。
//!
//! ## 最小价位（重要，实测踩过）
//!
//! A 股最小价位 0.01 元，各源 60min 收盘价存在 **1 分钱** 的口径差
//! （2026-11 实测：东财/腾讯一致，新浪在 124 根里有 9 根差 0.01；
//! 10 元股价上 1 分钱 = 10.4 bps，会击穿 5 bps 容差）。所以判定不能只看 bps，
//! 必须允许"1 个最小价位"的噪声，否则每次校验都误报。
//!
//! 这个做法对"常数复权因子"免疫，但对**重叠窗口内发生分红/换源重定基**敏感
//! （会真报警，需要重做 bootstrap 或改用因子表换算）。

use crate::store::keyed_bars;
use chanlun_core::Bar;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A 股最小价位（元）。
pub const PRICE_TICK: f64 = 0.01;
const MAX_ISSUES: usize = 50;
const MAX_MISSING_EXAMPLES: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationReport {
    pub symbol: String,
    pub sources: Vec<String>,
    /// 公共 bar 数量（按 `(date, 日内序号)` 对齐）
    pub common_bars: usize,
    /// 收盘价最大相对偏差（bps，已扣除锚点复权比；可能 > 容差但仍通过"1 价位"判定）
    pub max_close_diff_bps: f64,
    /// 是否通过
    pub ok: bool,
    /// 未通过时的明细（前 N 条）
    pub issues: Vec<String>,
    // —— 以下为 T-06 补充字段 ——
    /// 本次比较使用的容差（bps）
    pub tolerance_bps: f64,
    /// 锚点（重叠段最后一根）的 A/B 收盘比；≈1 表示同口径，≠1 表示复权差异
    pub close_ratio: f64,
    /// 收盘价最大绝对偏差（元，已扣除锚点复权比）
    pub max_abs_close_diff: f64,
    /// 参与判定的最小价位（元）
    pub price_tick: f64,
    /// 仅在 A 中出现的 bar 数（窗口深度差异，不算错误）
    pub bars_only_in_a: usize,
    /// 仅在 B 中出现的 bar 数
    pub bars_only_in_b: usize,
    /// 缺失 bar 示例（前 20 条，`date#日内序号`）
    pub missing_examples: Vec<String>,
}

/// 两条序列按 `(date, 日内序号)` 对齐后的重叠信息。
struct Overlap {
    a_map: BTreeMap<(u32, usize), Bar>,
    b_map: BTreeMap<(u32, usize), Bar>,
    common: Vec<(u32, usize)>,
    only_a: Vec<(u32, usize)>,
    only_b: Vec<(u32, usize)>,
}

fn overlap(a: &[Bar], b: &[Bar]) -> Overlap {
    let a_map: BTreeMap<(u32, usize), Bar> = keyed_bars(a).into_iter().collect();
    let b_map: BTreeMap<(u32, usize), Bar> = keyed_bars(b).into_iter().collect();
    let mut common = Vec::new();
    let mut only_a = Vec::new();
    let mut only_b = Vec::new();
    for key in a_map.keys() {
        if b_map.contains_key(key) {
            common.push(*key);
        } else {
            only_a.push(*key);
        }
    }
    for key in b_map.keys() {
        if !a_map.contains_key(key) {
            only_b.push(*key);
        }
    }
    Overlap {
        a_map,
        b_map,
        common,
        only_a,
        only_b,
    }
}

fn key_label(key: &(u32, usize)) -> String {
    format!("{}#{}", key.0, key.1)
}

fn missing_examples(keys: &[(u32, usize)]) -> Vec<String> {
    keys.iter()
        .take(MAX_MISSING_EXAMPLES)
        .map(key_label)
        .collect()
}

/// 对比两个源在同一标的上的重叠 bar（锚定末根收盘比，容差单位 bps）。
pub fn compare(
    symbol: &str,
    a_name: &str,
    a: &[Bar],
    b_name: &str,
    b: &[Bar],
    tolerance_bps: f64,
) -> ValidationReport {
    let Overlap {
        a_map,
        b_map,
        common,
        only_a,
        only_b,
    } = overlap(a, b);
    let mut report = ValidationReport {
        symbol: symbol.to_string(),
        sources: vec![a_name.to_string(), b_name.to_string()],
        tolerance_bps,
        price_tick: PRICE_TICK,
        bars_only_in_a: only_a.len(),
        bars_only_in_b: only_b.len(),
        missing_examples: {
            let mut examples = missing_examples(&only_a);
            examples.extend(missing_examples(&only_b));
            examples.truncate(MAX_MISSING_EXAMPLES);
            examples
        },
        ..Default::default()
    };
    report.common_bars = common.len();
    if common.is_empty() {
        report.issues.push(format!(
            "{symbol}: {a_name} 与 {b_name} 无重叠 bar，无法校验"
        ));
        report.ok = false;
        return report;
    }

    // 锚点：重叠段最后一根（前复权以最新价为基准，锚点比即口径比）。
    let anchor_key = *common.last().expect("common 非空");
    let anchor_a = a_map[&anchor_key];
    let anchor_b = b_map[&anchor_key];
    if anchor_b.close <= 0.0 || anchor_a.close <= 0.0 {
        report.issues.push(format!(
            "{symbol}: 锚点 {} 收盘价非正（{a_name}={}, {b_name}={}）",
            key_label(&anchor_key),
            anchor_a.close,
            anchor_b.close
        ));
        report.ok = false;
        return report;
    }
    report.close_ratio = anchor_a.close / anchor_b.close;
    let ratio = report.close_ratio;
    if !ratio.is_finite() || ratio <= 0.0 {
        report
            .issues
            .push(format!("{symbol}: 锚定收盘比非法（{ratio}）"));
        report.ok = false;
        return report;
    }

    let mut max_diff_bps = 0.0f64;
    let mut max_abs_diff = 0.0f64;
    for key in &common {
        let bar_a = a_map[key];
        let bar_b = b_map[key];
        if bar_a.close <= 0.0 || bar_b.close <= 0.0 {
            if report.issues.len() < MAX_ISSUES {
                report.issues.push(format!(
                    "{symbol} {}: 收盘价非正（{a_name}={}, {b_name}={}）",
                    key_label(key),
                    bar_a.close,
                    bar_b.close
                ));
            }
            continue;
        }
        let normalized_a = bar_a.close / ratio;
        let abs_diff = (normalized_a - bar_b.close).abs();
        let diff_bps = abs_diff / bar_b.close * 10_000.0;
        max_diff_bps = max_diff_bps.max(diff_bps);
        max_abs_diff = max_abs_diff.max(abs_diff);
        // 容差：bps 容差对应的价格差与 1 个最小价位取大（吸收各源 1 分钱舍入差）。
        let bps_allowance = bar_b.close * tolerance_bps / 10_000.0;
        let allowance = bps_allowance.max(PRICE_TICK * (1.0 + 1e-9));
        if abs_diff > allowance && report.issues.len() < MAX_ISSUES {
            report.issues.push(format!(
                "{symbol} {}: {a_name} close {:.4} vs {b_name} close {:.4}（锚定比 {ratio:.6}，绝对差 {abs_diff:.4} = {diff_bps:.2} bps，允许 {allowance:.4}）",
                key_label(key),
                bar_a.close,
                bar_b.close
            ));
        }
    }
    report.max_close_diff_bps = max_diff_bps;
    report.max_abs_close_diff = max_abs_diff;
    report.ok = report.issues.is_empty();
    report
}

/// 逐源与基准源（第一个）两两对比。少于 2 个源返回空报告。
pub fn compare_all(
    symbol: &str,
    sources: &[(&str, &[Bar])],
    tolerance_bps: f64,
) -> Vec<ValidationReport> {
    if sources.len() < 2 {
        return Vec::new();
    }
    let (base_name, base) = sources[0];
    sources[1..]
        .iter()
        .map(|(name, bars)| compare(symbol, base_name, base, name, bars, tolerance_bps))
        .collect()
}

/// 多源校验汇总（供 MCP `data_health`/增量更新报告直接序列化）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationSummary {
    pub symbol: String,
    pub reports: Vec<ValidationReport>,
    /// 全部两两对比通过
    pub ok: bool,
    /// 所有对比中的最大偏差（bps）
    pub max_close_diff_bps: f64,
    /// 告警/说明
    pub notes: Vec<String>,
}

pub fn validate_sources(
    symbol: &str,
    sources: &[(&str, &[Bar])],
    tolerance_bps: f64,
) -> ValidationSummary {
    let reports = compare_all(symbol, sources, tolerance_bps);
    let mut summary = ValidationSummary {
        symbol: symbol.to_string(),
        reports,
        ..Default::default()
    };
    if summary.reports.is_empty() {
        summary
            .notes
            .push(format!("{symbol}: 可用源不足 2 个，跳过交叉校验"));
        summary.ok = false;
        return summary;
    }
    summary.max_close_diff_bps = summary
        .reports
        .iter()
        .map(|report| report.max_close_diff_bps)
        .fold(0.0, f64::max);
    summary.ok = summary.reports.iter().all(|report| report.ok);
    for report in &summary.reports {
        if !report.ok {
            summary.notes.push(format!(
                "{} 校验未通过（最大偏差 {:.2} bps，{} 条 issue）",
                report.sources.join(" vs "),
                report.max_close_diff_bps,
                report.issues.len()
            ));
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(date: u32, close: f64) -> Bar {
        Bar {
            date,
            open: close,
            high: close,
            low: close,
            close,
            volume: 100.0,
            amount: 1000.0,
        }
    }

    fn day(date: u32, closes: [f64; 4]) -> Vec<Bar> {
        closes
            .iter()
            .map(|close| bar(date, *close))
            .map(|mut b| {
                b.date = date;
                b
            })
            .collect()
    }

    #[test]
    fn compares_same_bars_as_ok() {
        let a = [
            day(20261116, [10.0, 10.1, 10.2, 10.3]),
            day(20261117, [10.4, 10.5, 10.6, 10.7]),
        ]
        .concat();
        let report = compare("600000", "eastmoney", &a, "store", &a, 5.0);
        assert!(report.ok, "{report:?}");
        assert_eq!(report.common_bars, 8);
        assert_eq!(report.close_ratio, 1.0);
        assert_eq!(report.max_close_diff_bps, 0.0);
        assert!(report.issues.is_empty());
        assert_eq!(report.tolerance_bps, 5.0);
    }

    #[test]
    fn constant_adjustment_ratio_is_ok() {
        // 前复权 vs 不复权：同名 bar 差常数因子 10，不算异常。
        let a = day(20261116, [100.0, 101.0, 102.0, 103.0]);
        let b = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(report.ok, "{report:?}");
        assert!((report.close_ratio - 10.0).abs() < 1e-12);
        assert_eq!(report.max_close_diff_bps, 0.0);
    }

    #[test]
    fn flags_close_mismatch_beyond_tolerance() {
        let a = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let mut b = a.clone();
        b[2].close = 10.25; // 5 bps 之外
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(!report.ok);
        assert!(report.max_close_diff_bps > 5.0);
        assert!(!report.issues.is_empty());
        assert!(
            report.issues[0].contains("20261116#2"),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn within_tolerance_is_ok() {
        let a = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let mut b = a.clone();
        b[2].close = 10.2 * 1.0003; // 3 bps
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(report.ok, "{report:?}");
        assert!(report.max_close_diff_bps <= 5.0);
    }

    #[test]
    fn one_price_tick_rounding_is_ok_despite_5bps_tolerance() {
        // 实测：10 元股价上 1 分钱 = 10.4 bps，超过 5 bps 容差但不能算数据错误。
        let a = day(20261116, [9.65, 9.66, 9.64, 9.67]);
        let mut b = a.clone();
        b[0].close = 9.66;
        b[1].close = 9.65;
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(report.ok, "{report:?}");
        assert!(
            report.max_close_diff_bps > 5.0,
            "原始 bps 仍应 > 容差：{report:?}"
        );
        assert!(
            (report.max_abs_close_diff - 0.01).abs() < 1e-12,
            "{report:?}"
        );
        assert_eq!(report.price_tick, 0.01);
    }

    #[test]
    fn two_price_ticks_are_flagged() {
        let a = day(20261116, [9.65, 9.66, 9.64, 9.67]);
        let mut b = a.clone();
        b[0].close = 9.63; // 2 分钱
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(!report.ok, "{report:?}");
        assert!((report.max_abs_close_diff - 0.02).abs() < 1e-12);
    }

    #[test]
    fn counts_missing_bars_by_window() {
        let a = [
            day(20261116, [10.0, 10.1, 10.2, 10.3]),
            day(20261117, [10.4, 10.5, 10.6, 10.7]),
        ]
        .concat();
        let b = day(20261117, [10.4, 10.5, 10.6, 10.7]);
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(report.ok, "{report:?}");
        assert_eq!(report.bars_only_in_a, 4);
        assert_eq!(report.bars_only_in_b, 0);
        assert_eq!(report.missing_examples.len(), 4);
        assert_eq!(report.common_bars, 4);
    }

    #[test]
    fn no_overlap_is_not_ok() {
        let a = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let b = day(20261117, [10.0, 10.1, 10.2, 10.3]);
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert!(!report.ok);
        assert_eq!(report.common_bars, 0);
        assert!(report.issues[0].contains("无重叠"));
    }

    #[test]
    fn same_day_four_bars_do_not_collapse() {
        // 只用 date 建 map 会把 4 根压成 1 根；这里必须看到 4 个公共 bar。
        let a = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let b = day(20261116, [99.0, 99.0, 99.0, 99.0]); // 不连续，必报错
        let report = compare("600000", "eastmoney", &a, "sina", &b, 5.0);
        assert_eq!(report.common_bars, 4);
        assert!(!report.ok);
    }

    #[test]
    fn validate_sources_summarizes() {
        let a = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let b = a.clone();
        let c = day(20261116, [10.0, 10.1, 10.2, 10.3]);
        let summary = validate_sources("600000", &[("em", &a), ("sina", &b), ("tencent", &c)], 5.0);
        assert!(summary.ok, "{summary:?}");
        assert_eq!(summary.reports.len(), 2);
        assert_eq!(summary.max_close_diff_bps, 0.0);

        let mut bad = a.clone();
        bad[0].close = 11.0;
        let summary = validate_sources("600000", &[("em", &a), ("sina", &bad)], 5.0);
        assert!(!summary.ok);
        assert!(!summary.notes.is_empty());

        let summary = validate_sources("600000", &[("em", &a)], 5.0);
        assert!(!summary.ok);
        assert!(summary.notes[0].contains("不足 2 个"));
    }
}
