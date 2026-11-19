//! 本地序列仓：读/写单票 60min 序列、遍历全市场、旧仓导入、增量合并。
//!
//! 兼容两种 CSV 形态：
//!
//! 1. **旧 Windows 数据仓**（bootstrap 源）：`<root>/sh_600000_60min.csv`，表头
//!    `date,time,open,high,low,close,volume,amount`，UTF-8 **带 BOM**；
//!    - `date` = `YYYY-MM-DD`（也接受 `YYYYMMDD` / `YYYY/MM/DD`）；
//!    - `time` = eltdx 32 位格式 `{date8}{date8}{HHMM}000000000000`
//!      （baostock 原始形态 `YYYYMMDDHHMMSS` / 纯 `HHMM` 也能解析）；
//!    - 每只票约 5,774 根（2020-01-02 起），每天 4 根但 `Bar.date` 只保留日期，
//!      **同一天多根 bar 靠文件内行序区分**（`load_series` 用稳定排序，不会打乱日内顺序）。
//! 2. **新导出形态**（本仓写出的规范格式）：`date,open,high,low,close,volume,amount`，
//!    `date` = `YYYYMMDD`，无 `time` 列；一天 4 行日期相同、行序即时间序。
//!
//! 口径约定：
//! - `volume` 统一为**股**（旧仓 baostock 即股；东财/腾讯返回"手"，在 `sources` 层 ×100）；
//! - `amount` 单位为**元**；缺失填 0.0；
//! - 价格口径由上游决定（`bootstrap` 与东财增量都是前复权），本层不做复权换算。
//!
//! 为什么 `bootstrap_import` 默认读旧仓：公开源的 60min 历史只有 1–1.5 年，
//! 2020+ 历史只能从旧 Windows 仓导入（见 `docs/data-sources.md`）。

use crate::{DataError, Result, StoreFormat, split_symbol};
use chanlun_core::{Bar, BarSeries};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 规范 CSV 表头。
pub const CANONICAL_HEADER: &str = "date,open,high,low,close,volume,amount";
/// 序列文件名中间段：`<exchange>_<code>_60min.csv`。
pub const SERIES_SUFFIX: &str = "60min";
/// 读文件尾部的窗口大小（取末行日期用）。
const TAIL_BYTES: u64 = 8 * 1024;
/// 导入失败明细最多保留条数。
const MAX_FAILURE_EXAMPLES: usize = 200;
/// 递归遍历源目录的最大深度。
const MAX_WALK_DEPTH: usize = 8;

const DATE_ALIASES: &[&str] = &["date", "datetime", "trade_date", "day", "time"];
const TIME_ALIASES: &[&str] = &["time"];
const OPEN_ALIASES: &[&str] = &["open"];
const HIGH_ALIASES: &[&str] = &["high"];
const LOW_ALIASES: &[&str] = &["low"];
const CLOSE_ALIASES: &[&str] = &["close"];
const VOLUME_ALIASES: &[&str] = &["volume", "vol"];
const AMOUNT_ALIASES: &[&str] = &["amount", "turnover"];

/// 序列仓。
#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
    pub format: StoreFormat,
}

/// 仓内一个序列文件（交易所 + 代码 + 路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEntry {
    pub exchange: String,
    pub symbol: String,
    pub path: PathBuf,
}

/// CSV 解析结果（`rows` 与 `skipped_rows` 用于导入报告）。
#[derive(Debug, Clone, Default)]
pub struct ParsedCsv {
    pub bars: Vec<Bar>,
    pub rows: usize,
    pub skipped_rows: usize,
    /// 文件是否带可用 `time` 列（旧仓有，新导出形态没有）。
    pub has_time: bool,
    /// 被去重的重复行数（旧仓按 `(date, HHMM)` 去重）。
    pub duplicate_rows: usize,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>, format: StoreFormat) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, format })
    }

    /// 列出仓内全部标的代码（不含交易所前缀），供全市场扫描/回测。
    pub fn list_symbols(&self) -> Result<Vec<String>> {
        let mut out: Vec<String> = self.list_entries()?.into_iter().map(|e| e.symbol).collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// 列出仓内全部序列文件（含交易所前缀，供 health / 增量更新使用）。
    pub fn list_entries(&self) -> Result<Vec<StoreEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let Some((exchange, symbol)) = parse_store_filename(&name) else {
                continue;
            };
            out.push(StoreEntry {
                exchange,
                symbol,
                path: entry.path(),
            });
        }
        out.sort_by(|a, b| {
            a.exchange
                .cmp(&b.exchange)
                .then_with(|| a.symbol.cmp(&b.symbol))
        });
        out.dedup_by(|a, b| a.exchange == b.exchange && a.symbol == b.symbol);
        Ok(out)
    }

    /// 单票文件路径（约定：`<exchange>_<code>_60min.csv|parquet`）。
    ///
    /// `symbol` 接受 `600000` / `sh600000` / `sh.600000`。
    pub fn path_for(&self, symbol: &str) -> PathBuf {
        let (exchange, code) =
            split_symbol(symbol).unwrap_or_else(|| ("sz".to_string(), symbol.trim().to_string()));
        self.path_for_exchange(&exchange, &code)
    }

    /// 指定义交易所的单票路径（旧仓导入时保留源文件前缀，不要靠首字母猜）。
    pub fn path_for_exchange(&self, exchange: &str, symbol: &str) -> PathBuf {
        let ext = match self.format {
            StoreFormat::Csv => "csv",
            StoreFormat::Parquet => "parquet",
        };
        self.root
            .join(format!("{exchange}_{symbol}_{SERIES_SUFFIX}.{ext}"))
    }

    /// 实际存在的序列文件：先按配置格式找，再回退到另一种格式。
    pub fn resolve_path(&self, symbol: &str) -> Option<PathBuf> {
        let (exchange, code) = split_symbol(symbol)?;
        let primary = self.path_for_exchange(&exchange, &code);
        if primary.is_file() {
            return Some(primary);
        }
        let alt_ext = match self.format {
            StoreFormat::Csv => "parquet",
            StoreFormat::Parquet => "csv",
        };
        let alt = self
            .root
            .join(format!("{exchange}_{code}_{SERIES_SUFFIX}.{alt_ext}"));
        alt.is_file().then_some(alt)
    }

    /// 读取单票序列（按时间升序；同一天多根 bar 保持文件行序）。
    pub fn load_series(&self, symbol: &str) -> Result<BarSeries> {
        let (exchange, code) = split_symbol(symbol)
            .ok_or_else(|| DataError::Parse(format!("非法标的代码：{symbol}")))?;
        let path = self.resolve_path(&code).ok_or_else(|| {
            DataError::Incomplete(format!("数据仓缺少 {exchange}.{code} 的序列文件"))
        })?;
        let bars = self.read_bars(&path)?;
        Ok(BarSeries { symbol: code, bars })
    }

    /// 读任意序列文件（按时间升序，同一天保持行序）。
    pub fn read_bars(&self, path: &Path) -> Result<Vec<Bar>> {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("parquet") => Err(DataError::Unimplemented(
                "Parquet 序列读取尚未实现（当前默认 CSV；后续用 arrow/parquet 元数据）",
            )),
            _ => Ok(parse_csv_file(path)?.bars),
        }
    }

    /// 写单票序列（原子替换；按日期稳定排序）。
    pub fn write_series(&self, symbol: &str, bars: &[Bar]) -> Result<PathBuf> {
        let (exchange, code) = split_symbol(symbol)
            .ok_or_else(|| DataError::Parse(format!("非法标的代码：{symbol}")))?;
        self.write_series_to(&exchange, &code, bars)
    }

    /// 指定义交易所写序列（旧仓导入保留源前缀）。
    pub fn write_series_to(&self, exchange: &str, symbol: &str, bars: &[Bar]) -> Result<PathBuf> {
        if self.format == StoreFormat::Parquet {
            return Err(DataError::Unimplemented(
                "Parquet 序列写入尚未实现（bootstrap 阶段固定 CSV）",
            ));
        }
        let path = self.path_for_exchange(exchange, symbol);
        write_csv_file(&path, bars)?;
        Ok(path)
    }

    /// 单票末行日期（只读文件头 + 尾部，不加载全量）。
    pub fn last_bar_date(&self, symbol: &str) -> Result<Option<u32>> {
        let path = self
            .resolve_path(symbol)
            .ok_or_else(|| DataError::Incomplete(format!("数据仓缺少 {symbol} 的序列文件")))?;
        last_bar_date(&path)
    }

    /// 把增量源抓到的 bar 合并进已有序列。
    ///
    /// 对齐规则：以 `(date, 日内序号)` 为 bar 身份，**抓取到的 bar 覆盖同身份旧 bar**。
    /// 只抓到半天（例如源窗口从某天 11:30 才开始）时无法判断缺的是哪几根，
    /// 因此：若抓取到**第一天**的 bar 数少于已有文件当天的 bar 数，就整段保留旧文件当天
    /// （`kept_first_day = true`），从次日起合并，避免把旧 bar 错位覆盖。
    ///
    /// 注意：不做复权口径校验。若上游改了复权基准（分红/换源），先跑
    /// `validate::compare` 的锚定收盘比，再决定是否重做 bootstrap。
    pub fn merge_incremental(&self, symbol: &str, fetched: &[Bar]) -> Result<MergeReport> {
        if fetched.is_empty() {
            return Err(DataError::Incomplete(format!(
                "{symbol}: 增量源未返回任何 bar"
            )));
        }
        let (exchange, code) = split_symbol(symbol)
            .ok_or_else(|| DataError::Parse(format!("非法标的代码：{symbol}")))?;
        let path = self.path_for_exchange(&exchange, &code);
        let existing = if path.is_file() {
            self.read_bars(&path)?
        } else {
            Vec::new()
        };
        let mut fetched_sorted = fetched.to_vec();
        fetched_sorted.sort_by_key(|bar| bar.date); // 稳定排序

        // 第一天完整性保护（见函数文档）。
        let first_fetched_date = fetched_sorted[0].date;
        let existing_first_count = existing
            .iter()
            .take_while(|bar| bar.date == first_fetched_date)
            .count();
        let fetched_first_count = fetched_sorted
            .iter()
            .take_while(|bar| bar.date == first_fetched_date)
            .count();
        let kept_first_day = existing_first_count > fetched_first_count;
        if kept_first_day {
            fetched_sorted.retain(|bar| bar.date != first_fetched_date);
        }

        let mut merged: BTreeMap<(u32, usize), Bar> = BTreeMap::new();
        for (key, bar) in keyed_bars(&existing) {
            merged.insert(key, bar);
        }
        let existed_bars = existing.len();
        let mut added = 0usize;
        let mut replaced = 0usize;
        for (key, bar) in keyed_bars(&fetched_sorted) {
            match merged.get(&key) {
                Some(old) => {
                    if *old != bar {
                        replaced += 1;
                        merged.insert(key, bar);
                    }
                }
                None => {
                    added += 1;
                    merged.insert(key, bar);
                }
            }
        }
        let bars: Vec<Bar> = merged.into_values().collect();
        let changed = added > 0 || replaced > 0;
        if changed {
            self.write_series_to(&exchange, &code, &bars)?;
        }
        Ok(MergeReport {
            symbol: code,
            existed_bars,
            fetched_bars: fetched.len(),
            added,
            replaced,
            total_bars: bars.len(),
            last_date: bars.last().map(|b| b.date).unwrap_or(0),
            changed,
            kept_first_day,
        })
    }
}

/// 增量合并结果。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MergeReport {
    pub symbol: String,
    /// 合并前已有 bar 数（文件不存在为 0）
    pub existed_bars: usize,
    pub fetched_bars: usize,
    /// 新增 bar 数
    pub added: usize,
    /// 被新值覆盖的 bar 数（数值有变化的）
    pub replaced: usize,
    pub total_bars: usize,
    pub last_date: u32,
    pub changed: bool,
    /// 抓取到的第一天不完整，已整段保留旧文件当天（见 `merge_incremental` 文档）
    pub kept_first_day: bool,
}

/// 旧仓导入（一次性 bootstrap）的报告。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportReport {
    pub from_dir: String,
    pub to_dir: String,
    /// 扫描到的候选 CSV 文件数
    pub files_scanned: usize,
    /// 成功导入（新写或覆盖更新）
    pub imported: usize,
    /// 跳过（目标已是最新 / 源文件无 bar）
    pub skipped: usize,
    /// 失败（文件名不合法 / 解析/写盘错误）
    pub failed: usize,
    /// 写入的 bar 总数
    pub bars_written: usize,
    /// 导入序列的最早日期（YYYYMMDD，0 = 无）
    pub first_date: u32,
    /// 导入序列的末行日期（YYYYMMDD，0 = 无）
    pub last_date: u32,
    /// **按交易所分组**（红线：不许只看汇总"N 只更新"）
    pub by_exchange: BTreeMap<String, ExchangeImportReport>,
    /// 跳过原因计数
    pub skip_reasons: BTreeMap<String, usize>,
    /// 失败明细（最多 200 条，全量计数见 `failed`）
    pub failure_examples: Vec<ImportFailure>,
    pub elapsed_ms: u64,
}

/// 单个交易所的导入统计。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExchangeImportReport {
    pub files: usize,
    pub imported_symbols: usize,
    pub skipped_symbols: usize,
    pub failed_symbols: usize,
    pub bars_written: usize,
    /// 该交易所已导入序列的最早日期
    pub first_date: u32,
    /// **该交易所末行日期**（导入 + 已存在跳过的文件一起统计）
    pub last_date: u32,
}

/// 导入失败明细。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFailure {
    pub file: String,
    pub error: String,
}

impl ImportReport {
    fn touch_exchange(&mut self, exchange: &str) -> &mut ExchangeImportReport {
        self.by_exchange.entry(exchange.to_string()).or_default()
    }
}

/// 从旧 Windows 数据仓把单票 CSV 导入成新仓（可重复执行，幂等）。
///
/// 行为：
/// - 递归遍历 `from_dir` 下所有 `*.csv`（跳过点文件/隐藏目录）；
/// - 解析旧仓 `date + time` 双列格式（32 位 eltdx time、`YYYYMMDDHHMMSS`、`HHMM` 都支持）；
/// - 按源文件名保留交易所前缀写出到 `to_dir/<exchange>_<code>_60min.csv`（原子替换）；
/// - 目标文件末行日期 >= 源文件末行日期的直接跳过，因此中断后重跑即可续传；
/// - 报告**按 sh/sz（/bj）分组**的导入数、bar 数与**末行日期**。
pub fn bootstrap_import(
    from_dir: impl AsRef<Path>,
    to_dir: impl AsRef<Path>,
) -> Result<ImportReport> {
    let from_dir = from_dir.as_ref();
    let to_dir = to_dir.as_ref();
    if !from_dir.is_dir() {
        return Err(DataError::Incomplete(format!(
            "源目录不存在或不是目录：{}",
            from_dir.display()
        )));
    }
    let store = Store::open(to_dir, StoreFormat::Csv)?;
    let files = collect_csv_files(from_dir)?;
    let started = Instant::now();
    let mut report = ImportReport {
        from_dir: from_dir.display().to_string(),
        to_dir: to_dir.display().to_string(),
        files_scanned: files.len(),
        ..Default::default()
    };

    for (i, path) in files.iter().enumerate() {
        let file_label = path
            .strip_prefix(from_dir)
            .unwrap_or(path)
            .display()
            .to_string();
        let Some((exchange, code)) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(parse_store_filename)
        else {
            report.failed += 1;
            report.touch_exchange("unknown").failed_symbols += 1;
            push_failure(
                &mut report,
                &file_label,
                "文件名不符合 <exchange>_<code>_60min.csv",
            );
            continue;
        };

        let outcome = import_one(&store, path, &exchange, &code);
        match outcome {
            Ok(ImportOutcome::Imported {
                bars,
                first_date,
                last_date,
            }) => {
                report.imported += 1;
                report.bars_written += bars;
                report.first_date = min_date(report.first_date, first_date);
                report.last_date = report.last_date.max(last_date);
                let ex = report.touch_exchange(&exchange);
                ex.files += 1;
                ex.imported_symbols += 1;
                ex.bars_written += bars;
                ex.first_date = min_date(ex.first_date, first_date);
                ex.last_date = ex.last_date.max(last_date);
            }
            Ok(ImportOutcome::Skipped { reason, last_date }) => {
                report.skipped += 1;
                *report.skip_reasons.entry(reason.to_string()).or_default() += 1;
                let ex = report.touch_exchange(&exchange);
                ex.files += 1;
                ex.skipped_symbols += 1;
                if let Some(d) = last_date {
                    ex.last_date = ex.last_date.max(d);
                    // 重跑时 imported=0，但仓里已有的末行日期仍要出现在汇总里。
                    report.last_date = report.last_date.max(d);
                }
            }
            Err(err) => {
                report.failed += 1;
                report.touch_exchange(&exchange).failed_symbols += 1;
                push_failure(&mut report, &file_label, &err.to_string());
            }
        }

        if (i + 1) % 500 == 0 {
            eprintln!(
                "[bootstrap] {}/{} 文件，已导入 {}，跳过 {}，失败 {}（{:.1}s）",
                i + 1,
                files.len(),
                report.imported,
                report.skipped,
                report.failed,
                started.elapsed().as_secs_f64()
            );
        }
    }

    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    for ex in report.by_exchange.values_mut() {
        ex.files = ex.imported_symbols + ex.skipped_symbols + ex.failed_symbols;
    }
    eprintln!(
        "[bootstrap] 完成：扫描 {}，导入 {}，跳过 {}，失败 {}，写入 {} 根 bar，{:.1}s",
        report.files_scanned,
        report.imported,
        report.skipped,
        report.failed,
        report.bars_written,
        report.elapsed_ms as f64 / 1000.0
    );
    Ok(report)
}

enum ImportOutcome {
    Imported {
        bars: usize,
        first_date: u32,
        last_date: u32,
    },
    Skipped {
        reason: &'static str,
        last_date: Option<u32>,
    },
}

fn import_one(store: &Store, path: &Path, exchange: &str, code: &str) -> Result<ImportOutcome> {
    let parsed = parse_csv_file(path)?;
    if parsed.bars.is_empty() {
        return Ok(ImportOutcome::Skipped {
            reason: "源文件无有效 bar",
            last_date: None,
        });
    }
    let first_date = parsed.bars.first().map(|b| b.date).unwrap_or(0);
    let last_date = parsed.bars.last().map(|b| b.date).unwrap_or(0);
    let target = store.path_for_exchange(exchange, code);
    if target.is_file()
        && let Some(dest_last) = last_bar_date(&target)?
        && dest_last >= last_date
    {
        return Ok(ImportOutcome::Skipped {
            reason: "目标已是最新",
            last_date: Some(dest_last),
        });
    }
    store.write_series_to(exchange, code, &parsed.bars)?;
    Ok(ImportOutcome::Imported {
        bars: parsed.bars.len(),
        first_date,
        last_date,
    })
}

fn push_failure(report: &mut ImportReport, file: &str, error: &str) {
    if report.failure_examples.len() < MAX_FAILURE_EXAMPLES {
        report.failure_examples.push(ImportFailure {
            file: file.to_string(),
            error: error.to_string(),
        });
    }
}

fn min_date(current: u32, candidate: u32) -> u32 {
    match (current, candidate) {
        (0, c) => c,
        (cur, 0) => cur,
        (cur, c) => cur.min(c),
    }
}

fn collect_csv_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_csv_inner(dir, 0, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_csv_inner(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if depth > MAX_WALK_DEPTH {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_csv_inner(&entry.path(), depth + 1, out)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .map(|e| e.eq_ignore_ascii_case("csv"))
                .unwrap_or(false)
        {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// 解析文件名中的 `(exchange, code)`；兼容 `sh_600000_60min.csv`、
/// `sh600000_60min.csv`、`600000_60min.csv`。
pub fn parse_store_filename(name: &str) -> Option<(String, String)> {
    let stem = name
        .strip_suffix(".csv")
        .or_else(|| name.strip_suffix(".CSV"))
        .or_else(|| name.strip_suffix(".parquet"))?;
    let stem = stem
        .strip_suffix(&format!("_{SERIES_SUFFIX}"))
        .unwrap_or(stem);
    let (first, rest) = stem.split_once('_')?;
    let (exchange, code) = match (first, rest) {
        ("sh", _) | ("sz", _) | ("bj", _) => (first, rest),
        _ => return None,
    };
    let code = code.trim_start_matches('_');
    if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
        Some((exchange.to_string(), code.to_string()))
    } else {
        None
    }
}

/// 将 `2026-11-16` / `20261116` / `202611161030` 统一成 `20261116`。
pub fn parse_date8(s: &str) -> Option<u32> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 8 {
        return None;
    }
    digits[..8].parse::<u32>().ok()
}

/// 从 `time` 列提取 HHMM（用于旧仓按时间排序/去重）：
/// 兼容 eltdx 32 位、`YYYYMMDDHHMMSS`、`YYYYMMDDHHMM`、`HHMM`、`HHMMSS`。
pub fn parse_time_hhmm(raw: &str) -> Option<u32> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let part = match digits.len() {
        4 => &digits[..4],
        6 => &digits[..4],
        12 | 14 => &digits[8..12],
        15..=19 => &digits[8..12],
        20..=40 => {
            // eltdx 32 位：{date8}{date8}{HHMM}000000000000
            if digits.len() == 32 {
                &digits[16..20]
            } else {
                &digits[8..12]
            }
        }
        _ => return None,
    };
    part.parse::<u32>().ok()
}

/// 解析单票 CSV（自动识别旧仓 / 新导出两种形态）。
pub fn parse_csv_file(path: &Path) -> Result<ParsedCsv> {
    let file = File::open(path)?;
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(BufReader::new(file));
    let headers: Vec<String> = rdr
        .headers()?
        .iter()
        .map(|h| h.trim_start_matches('\u{feff}').trim().to_ascii_lowercase())
        .collect();
    let date_col = column_index(&headers, DATE_ALIASES)
        .ok_or_else(|| DataError::Parse(format!("{}: 缺 date 列", path.display())))?;
    let time_col = column_index(&headers, TIME_ALIASES);
    let open_col = column_index(&headers, OPEN_ALIASES)
        .ok_or_else(|| DataError::Parse(format!("{}: 缺 open 列", path.display())))?;
    let high_col = column_index(&headers, HIGH_ALIASES)
        .ok_or_else(|| DataError::Parse(format!("{}: 缺 high 列", path.display())))?;
    let low_col = column_index(&headers, LOW_ALIASES)
        .ok_or_else(|| DataError::Parse(format!("{}: 缺 low 列", path.display())))?;
    let close_col = column_index(&headers, CLOSE_ALIASES)
        .ok_or_else(|| DataError::Parse(format!("{}: 缺 close 列", path.display())))?;
    let volume_col = column_index(&headers, VOLUME_ALIASES);
    let amount_col = column_index(&headers, AMOUNT_ALIASES);

    // (date, hhmm/None, 行号, bar)；行号做同一天缺 time 时的稳定排序键。
    let mut rows: Vec<(u32, Option<u32>, usize, Bar)> = Vec::new();
    let mut parsed = ParsedCsv::default();
    for (row_idx, record) in rdr.records().enumerate() {
        let record = match record {
            Ok(record) => record,
            Err(_) => {
                parsed.skipped_rows += 1;
                continue;
            }
        };
        parsed.rows += 1;
        let date = record.get(date_col).and_then(parse_date8).or_else(|| {
            time_col
                .and_then(|col| record.get(col))
                .and_then(parse_date8)
        });
        let Some(date) = date else {
            parsed.skipped_rows += 1;
            continue;
        };
        let getf = |idx: Option<usize>| -> f64 {
            idx.and_then(|i| record.get(i))
                .map(parse_number)
                .unwrap_or(0.0)
        };
        let open = getf(Some(open_col));
        let high = getf(Some(high_col));
        let low = getf(Some(low_col));
        let close = getf(Some(close_col));
        if open <= 0.0 && high <= 0.0 && low <= 0.0 && close <= 0.0 {
            parsed.skipped_rows += 1;
            continue;
        }
        let volume = getf(volume_col);
        let amount = getf(amount_col);
        let time_key = time_col
            .and_then(|col| record.get(col))
            .and_then(parse_time_hhmm);
        if time_key.is_some() {
            parsed.has_time = true;
        }
        rows.push((
            date,
            time_key,
            row_idx,
            Bar {
                date,
                open,
                high,
                low,
                close,
                volume,
                amount,
            },
        ));
    }

    // 按 (日期, HHMM) 稳定排序；缺 time 的行按文件行序排在同日末尾。
    rows.sort_by_key(|(date, hhmm, row_idx, _)| (*date, hhmm.unwrap_or(u32::MAX), *row_idx));
    let mut seen: HashSet<(u32, u32)> = HashSet::new();
    let mut bars = Vec::with_capacity(rows.len());
    for (date, time_key, _, bar) in rows {
        if let Some(hhmm) = time_key
            && !seen.insert((date, hhmm))
        {
            parsed.duplicate_rows += 1;
            continue;
        }
        bars.push(bar);
    }
    parsed.bars = bars;
    Ok(parsed)
}

fn column_index(headers: &[String], aliases: &[&str]) -> Option<usize> {
    headers
        .iter()
        .position(|header| aliases.iter().any(|alias| alias == header))
}

fn parse_number(raw: &str) -> f64 {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    trimmed.parse::<f64>().unwrap_or(0.0)
}

pub(crate) fn keyed_bars(bars: &[Bar]) -> Vec<((u32, usize), Bar)> {
    let mut ordinals: BTreeMap<u32, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(bars.len());
    for bar in bars {
        let ordinal = ordinals.entry(bar.date).or_insert(0);
        out.push(((bar.date, *ordinal), *bar));
        *ordinal += 1;
    }
    out
}

/// 原子写单票 CSV（先写 `.tmp` 再 rename）。
pub fn write_csv_file(path: &Path, bars: &[Bar]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut sorted: Vec<Bar> = bars.to_vec();
    sorted.sort_by_key(|bar| bar.date); // 稳定排序：同一天保持入参（时间）顺序
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(tmp);
    let write_result = (|| -> Result<()> {
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "{CANONICAL_HEADER}")?;
        for bar in &sorted {
            writeln!(
                writer,
                "{},{},{},{},{},{},{}",
                bar.date, bar.open, bar.high, bar.low, bar.close, bar.volume, bar.amount
            )?;
        }
        writer.flush()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// 只读文件头 + 尾部，取末行日期（不加载全量；health 全市场扫描用）。
pub fn last_bar_date(path: &Path) -> Result<Option<u32>> {
    let date_col = header_date_index(path)?;
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    let mut reader = file;
    let start = len.saturating_sub(TAIL_BYTES);
    reader.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    // start > 0 时第一行是被截断的半个行，必须丢弃。
    let mut lines = text.split('\n').filter(|line| !line.trim().is_empty());
    if start > 0 {
        lines.next();
    }
    for line in lines.rev() {
        let line = line.trim().trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(date) = line.split(',').nth(date_col).and_then(parse_date8) {
            return Ok(Some(date));
        }
    }
    // 尾部窗口里没有有效数据行（例如文件只有表头）：退回全文扫描一次。
    let all: Vec<String> = fs::read_to_string(path)?
        .lines()
        .map(|line| line.to_string())
        .collect();
    for line in all.iter().skip(1).rev() {
        if let Some(date) = line.split(',').nth(date_col).and_then(parse_date8) {
            return Ok(Some(date));
        }
    }
    Ok(None)
}

fn header_date_index(path: &Path) -> Result<usize> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut header = String::new();
    reader.read_line(&mut header)?;
    if header.trim().is_empty() {
        return Err(DataError::Parse(format!(
            "{}: 空文件（无表头）",
            path.display()
        )));
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(header.as_bytes());
    let record = rdr
        .records()
        .next()
        .ok_or_else(|| DataError::Parse(format!("{}: 表头为空", path.display())))?
        .map_err(|_| DataError::Parse(format!("{}: 表头无法解析", path.display())))?;
    let headers: Vec<String> = record
        .iter()
        .map(|h| h.trim_start_matches('\u{feff}').trim().to_ascii_lowercase())
        .collect();
    column_index(&headers, DATE_ALIASES)
        .ok_or_else(|| DataError::Parse(format!("{}: 缺 date 列", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 构造旧仓 32 位 eltdx time：`{date8}{date8}{HHMM}000000000000`。
    fn time32(date8: &str, hhmm: &str) -> String {
        format!("{date8}{date8}{hhmm}000000000000")
    }

    const OLD_HEADER: &str = "date,time,open,high,low,close,volume,amount";

    fn write_old_fixture(path: &Path, start_date: &str, close: f64) {
        let date8 = start_date.replace('-', "");
        let mut text = String::from("\u{feff}");
        text.push_str(OLD_HEADER);
        text.push('\n');
        for day in 0..2 {
            let date = if day == 0 {
                date8.clone()
            } else {
                next_day(&date8)
            };
            for (hhmm, delta) in [("0930", 0.0), ("1030", 0.1), ("1130", 0.2), ("1400", 0.3)] {
                let px = close + delta;
                text.push_str(&format!(
                    "{}-{}-{},{},{},{},{},{},{},{}\n",
                    &date[..4],
                    &date[4..6],
                    &date[6..8],
                    time32(&date, hhmm),
                    px,
                    px + 0.5,
                    px - 0.5,
                    px + 0.1,
                    1_000_000.0 + delta * 10.0,
                    10_000_000.0
                ));
            }
        }
        fs::write(path, text).unwrap();
    }

    fn next_day(date8: &str) -> String {
        // 仅测试用：13 号之后自增一天，够用（不跨月）。
        let day: u32 = date8[6..8].parse().unwrap();
        format!("{}{:02}", &date8[..6], day + 1)
    }

    #[test]
    fn parses_legacy_date_time_csv() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sh_600000_60min.csv");
        write_old_fixture(&path, "2020-01-02", 9.0);
        let parsed = parse_csv_file(&path).unwrap();
        assert_eq!(parsed.bars.len(), 8);
        assert!(parsed.has_time);
        assert_eq!(parsed.bars[0].date, 20200102);
        assert_eq!(parsed.bars[0].close, 9.1);
        assert_eq!(parsed.bars[4].date, 20200103);
        // 同一天 4 根 bar 的日内顺序必须保持（09:30 在 14:00 之前）。
        assert!(parsed.bars[0].close < parsed.bars[1].close);
        assert!(parsed.bars[1].close < parsed.bars[2].close);
        assert!(parsed.bars[2].close < parsed.bars[3].close);
        assert_eq!(parsed.bars[3].close, 9.4);
    }

    #[test]
    fn dedupes_legacy_rows_by_time() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sh_600000_60min.csv");
        let line = "2020-01-02,20200102202001020930000000000000,9.0,9.1,8.9,9.0,100,1000";
        fs::write(&path, format!("{OLD_HEADER}\n{line}\n{line}\n")).unwrap();
        let parsed = parse_csv_file(&path).unwrap();
        assert_eq!(parsed.bars.len(), 1);
        assert_eq!(parsed.duplicate_rows, 1);
    }

    #[test]
    fn parses_new_export_csv_without_time() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sz_000001_60min.csv");
        let mut text = String::from(CANONICAL_HEADER);
        text.push('\n');
        for (i, close) in [10.0, 10.1, 10.2, 10.3].iter().enumerate() {
            text.push_str(&format!(
                "20261116,{},{},{},{},1234567,12345678\n",
                10.0, 10.5, 9.5, close
            ));
            let _ = i;
        }
        text.push_str("20261117,10.3,10.4,10.2,10.35,1200000,12000000\n");
        fs::write(&path, text).unwrap();
        let parsed = parse_csv_file(&path).unwrap();
        assert!(!parsed.has_time);
        assert_eq!(parsed.bars.len(), 5);
        assert_eq!(parsed.bars[0].date, 20261116);
        assert_eq!(parsed.bars[4].date, 20261117);
        assert_eq!(parsed.bars[3].close, 10.3);
        // 同一天 4 根 bar 全部保留。
        assert_eq!(parsed.bars.iter().filter(|b| b.date == 20261116).count(), 4);
    }

    #[test]
    fn accepts_datetime_column_and_skips_bad_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sh_600000_60min.csv");
        fs::write(
            &path,
            "datetime,open,high,low,close,volume,amount\n\
             2026-11-16 10:30:00,10,11,9,10.5,100,1000\n\
             bad-row,1,2,3,4,5,6\n\
             2026-11-16 11:30:00,10.5,11,10,10.8,200,2000\n",
        )
        .unwrap();
        let parsed = parse_csv_file(&path).unwrap();
        assert_eq!(parsed.bars.len(), 2);
        assert_eq!(parsed.skipped_rows, 1);
        assert_eq!(parsed.bars[1].close, 10.8);
    }

    #[test]
    fn parses_dates_and_times() {
        assert_eq!(parse_date8("2026-11-16"), Some(20261116));
        assert_eq!(parse_date8("20261116"), Some(20261116));
        assert_eq!(parse_date8("20261116103000"), Some(20261116));
        assert_eq!(
            parse_time_hhmm("20200102202001020930000000000000"),
            Some(930)
        );
        assert_eq!(parse_time_hhmm("20200102093000"), Some(930));
        assert_eq!(parse_time_hhmm("0930"), Some(930));
        assert_eq!(parse_time_hhmm("100000"), Some(1000));
        assert_eq!(parse_time_hhmm("20200102093000000"), Some(930));
        assert_eq!(parse_time_hhmm(""), None);
    }

    #[test]
    fn derives_symbols_from_filenames() {
        assert_eq!(
            parse_store_filename("sh_600000_60min.csv"),
            Some(("sh".to_string(), "600000".to_string()))
        );
        assert_eq!(
            parse_store_filename("sz_000001_60min.csv"),
            Some(("sz".to_string(), "000001".to_string()))
        );
        assert_eq!(
            parse_store_filename("bj_830799_60min.csv"),
            Some(("bj".to_string(), "830799".to_string()))
        );
        assert_eq!(parse_store_filename("sh600000_60min.csv"), None);
        assert_eq!(parse_store_filename("readme.txt"), None);
    }

    fn write_repo(dir: &Path) {
        write_old_fixture(&dir.join("sh_600000_60min.csv"), "2020-01-02", 9.0);
        write_old_fixture(&dir.join("sz_000001_60min.csv"), "2020-01-02", 10.0);
        // 缺 close 列的坏文件（表头与行都缺 close）
        fs::write(
            dir.join("sh_600001_60min.csv"),
            format!(
                "date,time,open,high,low,volume,amount\n\
                 2020-01-02,{},9.0,9.1,8.9,100,1000\n",
                time32("20200102", "0930")
            ),
        )
        .unwrap();
        // 非 CSV 文件应被忽略
        fs::write(dir.join("notes.txt"), "ignore me").unwrap();
    }

    #[test]
    fn bootstrap_import_reports_and_is_idempotent() {
        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        write_repo(from.path());

        let report = bootstrap_import(from.path(), to.path()).unwrap();
        assert_eq!(report.files_scanned, 3);
        assert_eq!(report.imported, 2);
        assert_eq!(report.failed, 1);
        assert_eq!(report.bars_written, 16);
        assert_eq!(report.last_date, 20200103);
        assert_eq!(report.first_date, 20200102);
        assert_eq!(report.failure_examples.len(), 1);

        let sh = report.by_exchange.get("sh").unwrap();
        assert_eq!(sh.imported_symbols, 1);
        assert_eq!(sh.failed_symbols, 1);
        assert_eq!(sh.last_date, 20200103);
        let sz = report.by_exchange.get("sz").unwrap();
        assert_eq!(sz.imported_symbols, 1);
        assert_eq!(sz.last_date, 20200103);

        // 目标仓可正常 list/load
        let store = Store::open(to.path(), StoreFormat::Csv).unwrap();
        let mut symbols = store.list_symbols().unwrap();
        symbols.sort();
        assert_eq!(symbols, vec!["000001".to_string(), "600000".to_string()]);
        let series = store.load_series("600000").unwrap();
        assert_eq!(series.bars.len(), 8);
        assert_eq!(series.bars.last().unwrap().date, 20200103);

        // 重跑：已导入的跳过，坏文件仍失败
        let rerun = bootstrap_import(from.path(), to.path()).unwrap();
        assert_eq!(rerun.imported, 0);
        assert_eq!(rerun.skipped, 2);
        assert_eq!(rerun.failed, 1);
        assert_eq!(
            rerun.skip_reasons.get("目标已是最新").copied().unwrap_or(0),
            2
        );
    }

    #[test]
    fn merge_incremental_appends_and_replaces() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        let day1 = [
            ("0930", 10.0),
            ("1030", 10.1),
            ("1130", 10.2),
            ("1400", 10.3),
        ];
        let existing: Vec<Bar> = day1
            .iter()
            .map(|(_, c)| Bar {
                date: 20261116,
                open: *c,
                high: c + 0.5,
                low: c - 0.5,
                close: *c,
                volume: 100.0,
                amount: 1000.0,
            })
            .collect();
        store.write_series("600000", &existing).unwrap();

        // 抓到 11-16 全天（收盘价整体变 0.1）+ 11-17 全天
        let mut fetched = Vec::new();
        for close in [10.1, 10.2, 10.3, 10.4] {
            fetched.push(Bar {
                date: 20261116,
                open: close,
                high: close + 0.5,
                low: close - 0.5,
                close,
                volume: 200.0,
                amount: 2000.0,
            });
        }
        for close in [10.5, 10.6, 10.7, 10.8] {
            fetched.push(Bar {
                date: 20261117,
                open: close,
                high: close + 0.5,
                low: close - 0.5,
                close,
                volume: 300.0,
                amount: 3000.0,
            });
        }
        let report = store.merge_incremental("600000", &fetched).unwrap();
        assert_eq!(report.existed_bars, 4);
        assert_eq!(report.fetched_bars, 8);
        assert_eq!(report.replaced, 4);
        assert_eq!(report.added, 4);
        assert_eq!(report.total_bars, 8);
        assert_eq!(report.last_date, 20261117);
        assert!(report.changed);

        let series = store.load_series("600000").unwrap();
        assert_eq!(series.bars.len(), 8);
        assert_eq!(series.bars[0].close, 10.1);
        assert_eq!(series.bars[4].close, 10.5);
        assert_eq!(series.bars[7].close, 10.8);

        // 再合并完全相同的增量：不应有变化
        let again = store.merge_incremental("600000", &fetched).unwrap();
        assert!(!again.changed);
        assert_eq!(again.added, 0);
        assert_eq!(again.replaced, 0);
    }

    #[test]
    fn merge_incremental_keeps_earlier_bars_of_partial_day() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        let existing: Vec<Bar> = [10.0, 10.1, 10.2, 10.3]
            .iter()
            .map(|c| Bar {
                date: 20261116,
                open: *c,
                high: *c,
                low: *c,
                close: *c,
                volume: 1.0,
                amount: 1.0,
            })
            .collect();
        store.write_series("600000", &existing).unwrap();
        // 只抓到当天后两根（11:30 / 14:00）
        let fetched: Vec<Bar> = [10.25, 10.35]
            .iter()
            .map(|c| Bar {
                date: 20261116,
                open: *c,
                high: *c,
                low: *c,
                close: *c,
                volume: 2.0,
                amount: 2.0,
            })
            .collect();
        let report = store.merge_incremental("600000", &fetched).unwrap();
        // 抓取到的第一天只有 2 根 < 旧文件 4 根：无法判断缺哪几根，整段保留旧文件当天。
        assert!(report.kept_first_day);
        assert_eq!(report.added, 0);
        assert_eq!(report.replaced, 0);
        assert!(!report.changed);
        let series = store.load_series("600000").unwrap();
        assert_eq!(series.bars.len(), 4);
        assert_eq!(series.bars[0].close, 10.0);
        assert_eq!(series.bars[2].close, 10.2);
        assert_eq!(series.bars[3].close, 10.3);
    }

    #[test]
    fn last_bar_date_reads_tail_without_full_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sh_600000_60min.csv");
        let mut bars = Vec::new();
        for day in 1..=40u32 {
            for close in [10.0, 10.1, 10.2, 10.3] {
                bars.push(Bar {
                    date: 20260100 + day,
                    open: close,
                    high: close,
                    low: close,
                    close,
                    volume: 1.0,
                    amount: 1.0,
                });
            }
        }
        write_csv_file(&path, &bars).unwrap();
        assert_eq!(last_bar_date(&path).unwrap(), Some(20260140));
        // 只有表头
        let empty = dir.path().join("sz_000001_60min.csv");
        fs::write(&empty, format!("{CANONICAL_HEADER}\n")).unwrap();
        assert_eq!(last_bar_date(&empty).unwrap(), None);
    }

    #[test]
    fn write_series_is_atomic_and_sorted() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), StoreFormat::Csv).unwrap();
        let bars = vec![
            Bar {
                date: 20261117,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
                amount: 1.0,
            },
            Bar {
                date: 20261116,
                open: 2.0,
                high: 2.0,
                low: 2.0,
                close: 2.0,
                volume: 2.0,
                amount: 2.0,
            },
        ];
        let path = store.write_series("000001", &bars).unwrap();
        assert!(path.ends_with("sz_000001_60min.csv"));
        let loaded = store.load_series("000001").unwrap();
        assert_eq!(loaded.bars[0].date, 20261116);
        assert_eq!(loaded.bars[1].date, 20261117);
        // 临时文件不残留
        assert!(!path.with_extension("csv.tmp").exists());
    }
}
