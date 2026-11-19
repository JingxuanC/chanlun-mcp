//! 数据健康检查（对应文档 §3 的"红线"）。
//!
//! **红线判据**：不许看汇总的"N 只已更新"，必须按交易所分组统计每只票的末行日期。
//! 2026-11-17 事故：深市 2,952 只停在 10-16，净值虚高。MCP 的 `data_health` 工具
//! 必须能直接回答：`sh` / `sz` 各自最新日期、落后多少只、最差的是谁。
//!
//! 性能：5,235 只全市场，若逐文件 `load_series` 会加载约 3,000 万根 bar（分钟级）。
//! 本实现只读**文件头**（定位 date 列）+ **文件尾 8KB**（取末行日期），
//! 全市场扫描应在秒级；Parquet 后续可用 row-group metadata 做同样的事。

use crate::Result;
use crate::store::{self, Store};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 落后比例红线（>= 10% 即整体不健康）。
pub const STALE_RED_LINE: f64 = 0.10;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExchangeHealth {
    pub total_symbols: usize,
    /// 该交易所最新末行日期（多数票的日期；少数停牌/退市票会被算作 stale）
    pub last_date: u32,
    /// 末行日期 < last_date 的票数
    pub stale_count: usize,
    /// 落后明细（最多 20 只，`code:date`）
    pub stale_examples: Vec<String>,
    // —— T-06 补充字段 ——
    /// 该交易所末行日期的**最大值**（若个别票领先多数票，看这里排错）
    pub newest_date: u32,
    /// 该交易所最早末行日期（最差的票）
    pub oldest_date: u32,
    /// 落后比例（stale_count / total_symbols）
    pub stale_ratio: f64,
    /// 该交易所是否健康（有数据且落后比例 < 10%）
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthReport {
    pub data_root: String,
    pub total_symbols: usize,
    /// key: "sh" | "sz"（北交所若存在则为 "bj"）
    pub by_exchange: BTreeMap<String, ExchangeHealth>,
    /// 整体判据：各交易所最新日期一致、落后占比 < 10%、无损坏文件
    pub ok: bool,
    pub notes: Vec<String>,
    // —— T-06 补充字段 ——
    /// 全仓最新末行日期
    pub newest_date: u32,
    /// 无法读取（空文件/坏表头/IO 错误）的序列文件数
    pub unreadable: usize,
}

/// 扫描数据仓，生成按交易所分组的健康报告（只读文件头 + 尾部）。
pub fn check(store: &Store) -> Result<HealthReport> {
    let entries = store.list_entries()?;
    let mut report = HealthReport {
        data_root: store.root.display().to_string(),
        total_symbols: entries.len(),
        ..Default::default()
    };
    if entries.is_empty() {
        report.ok = false;
        report.notes.push(format!(
            "数据仓为空或没有序列文件：{}",
            store.root.display()
        ));
        return Ok(report);
    }

    // (exchange, symbol) -> 末行日期
    let mut buckets: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
    let mut unreadable_examples: Vec<String> = Vec::new();
    for entry in &entries {
        match store::last_bar_date(&entry.path) {
            Ok(Some(last_date)) => buckets
                .entry(entry.exchange.clone())
                .or_default()
                .push((entry.symbol.clone(), last_date)),
            Ok(None) => {
                report.unreadable += 1;
                if unreadable_examples.len() < 10 {
                    unreadable_examples.push(format!("{}: 无有效数据行", entry.path.display()));
                }
            }
            Err(err) => {
                report.unreadable += 1;
                if unreadable_examples.len() < 10 {
                    unreadable_examples.push(format!("{}: {err}", entry.path.display()));
                }
            }
        }
    }

    let mut stale_total = 0usize;
    let mut counted_total = 0usize;
    for (exchange, items) in &buckets {
        // 取出现频次最高的日期作为该交易所"最新日期"（容忍个别停牌/退市票）；
        // 频次相同时取较新的日期（BTreeMap 升序 + max_by_key 取最后最大值）。
        let mut freq: BTreeMap<u32, usize> = BTreeMap::new();
        for (_, date) in items {
            *freq.entry(*date).or_default() += 1;
        }
        let last_date = freq
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(date, _)| *date)
            .unwrap_or(0);
        let mut stale: Vec<&(String, u32)> =
            items.iter().filter(|(_, date)| *date < last_date).collect();
        stale.sort_by_key(|(_, date)| *date);
        let stale_count = stale.len();
        let total = items.len();
        let stale_ratio = if total == 0 {
            1.0
        } else {
            stale_count as f64 / total as f64
        };
        let oldest_date = items.iter().map(|(_, date)| *date).min().unwrap_or(0);
        let newest_date = items.iter().map(|(_, date)| *date).max().unwrap_or(0);
        let stale_examples: Vec<String> = stale
            .iter()
            .take(20)
            .map(|(symbol, date)| format!("{symbol}:{date}"))
            .collect();
        stale_total += stale_count;
        counted_total += total;
        report.newest_date = report.newest_date.max(newest_date);
        report.by_exchange.insert(
            exchange.clone(),
            ExchangeHealth {
                total_symbols: total,
                last_date,
                stale_count,
                stale_examples,
                newest_date,
                oldest_date,
                stale_ratio,
                ok: last_date > 0 && stale_ratio < STALE_RED_LINE,
            },
        );
    }

    // 判据 1：各交易所最新日期一致（允许 0 表示空仓）
    let dates: Vec<u32> = report
        .by_exchange
        .values()
        .map(|exchange| exchange.last_date)
        .filter(|date| *date > 0)
        .collect();
    let consistent = dates.windows(2).all(|window| window[0] == window[1]);
    let overall_stale_ratio = if counted_total == 0 {
        1.0
    } else {
        stale_total as f64 / counted_total as f64
    };
    report.ok = consistent && overall_stale_ratio < STALE_RED_LINE && report.unreadable == 0;

    if !consistent {
        report.notes.push(format!(
            "红线告警：各交易所最新日期不一致（{}），当天卡片与报告应作废重跑",
            report
                .by_exchange
                .iter()
                .map(|(exchange, health)| format!("{exchange}={}", health.last_date))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if overall_stale_ratio >= STALE_RED_LINE {
        report.notes.push(format!(
            "红线告警：{:.1}% 标的落后于所属交易所最新日期（红线 {:.0}%）",
            overall_stale_ratio * 100.0,
            STALE_RED_LINE * 100.0
        ));
    }
    if report.unreadable > 0 {
        report.notes.push(format!(
            "{} 个序列文件无法读取（空/坏表头/Parquet 未实现）",
            report.unreadable
        ));
        report.notes.extend(unreadable_examples);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreFormat;
    use chanlun_core::Bar;
    use tempfile::TempDir;

    fn bars(dates: &[u32]) -> Vec<Bar> {
        dates
            .iter()
            .flat_map(|date| {
                (0..4).map(|i| Bar {
                    date: *date,
                    open: 10.0 + i as f64 * 0.1,
                    high: 11.0,
                    low: 9.0,
                    close: 10.0 + i as f64 * 0.1,
                    volume: 100.0,
                    amount: 1000.0,
                })
            })
            .collect()
    }

    fn write(store: &Store, exchange: &str, symbol: &str, dates: &[u32]) {
        store
            .write_series_to(exchange, symbol, &bars(dates))
            .unwrap();
    }

    #[test]
    fn healthy_repo_is_ok() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        write(&store, "sh", "600000", &[20261116, 20261117]);
        write(&store, "sh", "600001", &[20261116, 20261117]);
        write(&store, "sz", "000001", &[20261116, 20261117]);
        write(&store, "sz", "000002", &[20261116, 20261117]);
        let report = check(&store).unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(report.total_symbols, 4);
        assert_eq!(report.newest_date, 20261117);
        assert_eq!(report.by_exchange["sh"].last_date, 20261117);
        assert_eq!(report.by_exchange["sh"].newest_date, 20261117);
        assert_eq!(report.by_exchange["sh"].stale_count, 0);
        assert_eq!(report.by_exchange["sz"].last_date, 20261117);
        assert!(report.by_exchange.values().all(|health| health.ok));
    }

    #[test]
    fn red_line_flags_stale_exchange_and_lists_worst() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        write(&store, "sh", "600000", &[20261116, 20261117]);
        write(&store, "sz", "000001", &[20261116, 20261117]);
        write(&store, "sz", "000002", &[20261116, 20261117]);
        write(&store, "sz", "000003", &[20261116, 20261117]);
        // 深市 1/4 的票停在 10-16（模拟 2026-11-17 事故的缩微版）
        write(&store, "sz", "000004", &[20261016]);
        let report = check(&store).unwrap();
        assert!(!report.ok);
        let sz = &report.by_exchange["sz"];
        assert_eq!(sz.last_date, 20261117); // 多数票日期
        assert_eq!(sz.newest_date, 20261117);
        assert_eq!(sz.stale_count, 1);
        assert_eq!(sz.oldest_date, 20261016);
        assert!((sz.stale_ratio - 0.25).abs() < 1e-12);
        assert_eq!(sz.stale_examples.len(), 1);
        assert!(
            sz.stale_examples
                .iter()
                .any(|e| e.starts_with("000004:20261016"))
        );
        assert!(report.notes.iter().any(|note| note.contains("红线告警")));
    }

    #[test]
    fn whole_exchange_frozen_is_caught_by_date_mismatch() {
        // 2026-11-17 事故形态：深市全部停在 10-16，沪市正常。
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        write(&store, "sh", "600000", &[20261116, 20261117]);
        write(&store, "sh", "600001", &[20261116, 20261117]);
        write(&store, "sz", "000001", &[20261016]);
        write(&store, "sz", "000002", &[20261016]);
        let report = check(&store).unwrap();
        assert!(!report.ok);
        assert_eq!(report.by_exchange["sh"].last_date, 20261117);
        assert_eq!(report.by_exchange["sz"].last_date, 20261016);
        // 深市"多数票"日期自己就是 10-16，所以组内 stale=0；
        // 红线靠"各交易所日期不一致"命中，比汇总"N 只更新"更可靠。
        assert_eq!(report.by_exchange["sz"].stale_count, 0);
        assert!(report.notes.iter().any(|note| note.contains("不一致")));
    }

    #[test]
    fn exchange_date_mismatch_breaks_ok() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        write(&store, "sh", "600000", &[20261117]);
        write(&store, "sz", "000001", &[20261116]);
        let report = check(&store).unwrap();
        assert!(!report.ok);
        assert!(report.notes[0].contains("不一致"));
        assert_ne!(
            report.by_exchange["sh"].last_date,
            report.by_exchange["sz"].last_date
        );
    }

    #[test]
    fn groups_beijing_exchange_and_handles_empty_repo() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        let empty = check(&store).unwrap();
        assert!(!empty.ok);
        assert!(empty.notes[0].contains("数据仓为空"));

        write(&store, "bj", "830799", &[20261117]);
        let report = check(&store).unwrap();
        assert_eq!(report.by_exchange.len(), 1);
        assert_eq!(report.by_exchange["bj"].last_date, 20261117);
        assert!(report.ok, "{report:?}");
    }

    #[test]
    fn unreadable_files_are_counted_and_break_ok() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        write(&store, "sh", "600000", &[20261117]);
        std::fs::write(dir.path().join("sz_000001_60min.csv"), "").unwrap();
        let report = check(&store).unwrap();
        assert_eq!(report.unreadable, 1);
        assert!(!report.ok);
        assert_eq!(report.by_exchange["sh"].total_symbols, 1);
        assert!(report.notes.iter().any(|note| note.contains("无法读取")));
    }
}
