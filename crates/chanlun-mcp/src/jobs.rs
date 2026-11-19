//! 异步 job 框架。
//!
//! MCP 客户端普遍 ~60s 超时，而全市场回测/扫描是分钟级，因此：
//! - 长任务 `run_backtest` 立即返回 `job_id`，客户端用 `get_job` 轮询；
//! - job 由 `serve` 以**子进程**（`current_exe worker`）拉起：隔离 panic/内存，
//!   不阻塞 MCP 主循环；
//! - 目录布局：
//! ```text
//! $CHANLUN_HOME/jobs/<job_id>/
//!   spec.json      # 入参（kind + params）
//!   status.json    # 状态机：queued/running/done/failed/cancelled
//!   result.json    # 结果（stats + trades 摘要）
//!   log.txt        # stdout/stderr（人读）
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
use std::path::{Path, PathBuf};
use crate::home;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Backtest,
    Audit,
    Scan,
}

impl JobKind {
    fn prefix(self) -> &'static str {
        match self {
            JobKind::Backtest => "bt",
            JobKind::Audit => "audit",
            JobKind::Scan => "scan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub kind: JobKind,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub created_at_unix: u64,
    pub started_at_unix: Option<u64>,
    pub finished_at_unix: Option<u64>,
    pub pid: Option<u32>,
    pub error: Option<String>,
    pub progress: Option<String>,
}

pub fn job_dir(jobs_dir: &Path, job_id: &str) -> PathBuf {
    jobs_dir.join(job_id)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_job_id(kind: JobKind) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", kind.prefix(), now_unix(), nanos)
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn write_status(jobs_dir: &Path, st: &JobStatus) -> Result<()> {
    let path = job_dir(jobs_dir, &st.id).join("status.json");
    write_json_atomic(&path, &serde_json::to_value(st)?)
}

/// 提交一个 job：建目录、写 spec/status、拉起 worker 子进程。
pub fn submit(jobs_dir: &Path, kind: JobKind, params: Value) -> Result<String> {
    std::fs::create_dir_all(jobs_dir)?;
    let job_id = new_job_id(kind);
    let dir = job_dir(jobs_dir, &job_id);
    std::fs::create_dir_all(&dir)?;

    let spec = JobSpec { kind, params };
    std::fs::write(dir.join("spec.json"), serde_json::to_vec_pretty(&spec)?)?;

    let mut status = JobStatus {
        id: job_id.clone(),
        kind,
        state: JobState::Queued,
        created_at_unix: now_unix(),
        started_at_unix: None,
        finished_at_unix: None,
        pid: None,
        error: None,
        progress: None,
    };
    write_status(jobs_dir, &status)?;

    let exe = std::env::current_exe().context("无法定位当前可执行文件")?;
    let log = File::create(dir.join("log.txt"))?;
    let log_err = log.try_clone()?;
    let child = std::process::Command::new(exe)
        .arg("worker")
        .arg("--job-id")
        .arg(&job_id)
        .arg("--spec")
        .arg(dir.join("spec.json"))
        .arg("--jobs-dir")
        .arg(jobs_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .spawn()
        .context("拉起 worker 失败")?;
    status.pid = Some(child.id());
    write_status(jobs_dir, &status)?;
    eprintln!(
        "[job] submitted {} kind={:?} pid={}",
        job_id,
        kind,
        child.id()
    );
    Ok(job_id)
}

pub fn read_status(jobs_dir: &Path, job_id: &str) -> Result<JobStatus> {
    let path = job_dir(jobs_dir, job_id).join("status.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("job {job_id} 不存在（{}）", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

pub fn read_result(jobs_dir: &Path, job_id: &str) -> Result<Value> {
    let path = job_dir(jobs_dir, job_id).join("result.json");
    let text = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text)?)
}

/// 取消一个 job（SIGTERM 子进程并标记状态）。
pub fn cancel(jobs_dir: &Path, job_id: &str) -> Result<()> {
    let mut st = read_status(jobs_dir, job_id)?;
    if matches!(
        st.state,
        JobState::Done | JobState::Failed | JobState::Cancelled
    ) {
        return Ok(());
    }
    if let Some(pid) = st.pid {
        // macOS/Linux：直接 kill；Windows 下 worker 生命周期随父进程结束
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    st.state = JobState::Cancelled;
    st.finished_at_unix = Some(now_unix());
    write_status(jobs_dir, &st)?;
    Ok(())
}

/// worker 子进程入口：执行 job 并写 result/status。
pub fn run_worker(job_id: &str, spec_path: &Path, jobs_dir: &Path) -> Result<()> {
    let spec_text = std::fs::read_to_string(spec_path)?;
    let spec: JobSpec = serde_json::from_str(&spec_text)?;

    let mut st = read_status(jobs_dir, job_id).unwrap_or(JobStatus {
        id: job_id.to_string(),
        kind: spec.kind,
        state: JobState::Running,
        created_at_unix: now_unix(),
        started_at_unix: None,
        finished_at_unix: None,
        pid: Some(std::process::id()),
        error: None,
        progress: None,
    });
    st.state = JobState::Running;
    st.started_at_unix = Some(now_unix());
    st.pid = Some(std::process::id());
    write_status(jobs_dir, &st)?;

    let outcome = execute(&spec, jobs_dir, job_id);
    match outcome {
        Ok(result) => {
            std::fs::write(
                job_dir(jobs_dir, job_id).join("result.json"),
                serde_json::to_vec_pretty(&result)?,
            )?;
            st.state = JobState::Done;
            st.finished_at_unix = Some(now_unix());
            st.progress = Some("done".into());
            write_status(jobs_dir, &st)?;
        }
        Err(e) => {
            st.state = JobState::Failed;
            st.finished_at_unix = Some(now_unix());
            st.error = Some(format!("{e:#}"));
            write_status(jobs_dir, &st)?;
            return Err(e);
        }
    }
    Ok(())
}

fn execute(spec: &JobSpec, jobs_dir: &Path, job_id: &str) -> Result<Value> {
    match spec.kind {
        JobKind::Backtest => run_backtest_job(spec, jobs_dir, job_id),
        JobKind::Scan => run_scan_job(spec, jobs_dir, job_id),
        other => anyhow::bail!("job 类型 {other:?} 尚未实现（T-08）"),
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct ScanParams {
    /// 目标交易日 YYYYMMDD；省略 = 用各票最后一根 bar 的日期做过滤
    #[serde(default)]
    as_of: Option<u32>,
    /// 只扫描前 N 只（调试/小样本）
    #[serde(default)]
    limit: Option<usize>,
}

/// 全市场当日二买扫描（Scan job）。
///
/// 与回测同口径：`ChanlunSignalSource`（g+4 截断确认、g+5 开盘买入、Phase B/C 过滤），
/// 再补上可成交性上下文（一字板/涨停开盘/相对流动性）与建议金额/止损位。
fn run_scan_job(spec: &JobSpec, jobs_dir: &Path, job_id: &str) -> Result<Value> {
    use chanlun_data::StoreFormat;
    use chanlun_data::store::Store;
    use chanlun_engine::signals::{ChanlunSignalSource, SignalSource};
    let p: ScanParams = serde_json::from_value(spec.params.clone()).unwrap_or_default();
    let cfg = chanlun_engine::BacktestConfig::default();
    let st = chanlun_engine::load_st_table(&cfg);

    let home = home::chanlun_home();
    let data_dir = std::env::var("CHANLUN_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join("data").join("60min"));
    let store = Store::open(data_dir.clone(), StoreFormat::Csv)
        .with_context(|| format!("打开数据仓失败 {}", data_dir.display()))?;
    let mut universe = store.list_symbols().context("列出标的失败")?;
    if let Some(l) = p.limit {
        universe.truncate(l);
    }

    let mut src = ChanlunSignalSource::new();
    let total = universe.len();
    let mut scanned = 0usize;
    let mut errors = 0usize;
    let mut signals: Vec<Value> = Vec::new();
    let mut target_date = p.as_of;

    for (i, code) in universe.iter().enumerate() {
        if i % 200 == 0 {
            if let Ok(mut stt) = read_status(jobs_dir, job_id) {
                stt.progress = Some(format!("scan {i}/{total}"));
                let _ = write_status(jobs_dir, &stt);
            }
        }
        let series = match store.load_series(code) {
            Ok(s) => s,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let mut bars = series.bars;
        if let Some(d) = p.as_of {
            bars.retain(|b| b.date <= d);
        }
        if bars.len() < 300 {
            continue;
        }
        let last_date = bars.last().map(|b| b.date).unwrap_or(0);
        let _ = last_date;
        let target = p.as_of;
        let scanned_sigs = match src.scan(code, &bars, &cfg) {
            Ok(v) => v,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        scanned += 1;
        for sig in scanned_sigs {
            // 只保留"最近 20 根 bar 内已确认"的信号（≈最近 5 个交易日）；
            // as_of 是数据截止日，不是信号日 —— 信号需要 g+4 确认 + g+5 买入，天然滞后。
            if sig.buy_bar + 20 < bars.len() {
                continue;
            }
            let _ = target;
            let buy_bar = sig.buy_bar;
            if buy_bar >= bars.len() {
                continue;
            }
            let bar = &bars[buy_bar];
            let prev_close = if buy_bar > 0 {
                bars[buy_bar - 1].close
            } else {
                bar.open
            };
            let st_flag = st.is_st(code, bar.date);
            let limit_pct = chanlun_engine::gates::board_limit(code, st_flag);
            let one_word = chanlun_engine::gates::is_one_word(bar);
            let gap = if prev_close > 0.0 {
                bar.open / prev_close - 1.0
            } else {
                0.0
            };
            let notional = cfg.capital * cfg.pos_pct;
            let liq_block = chanlun_engine::gates::exceeds_relative_liquidity(
                notional,
                bar.amount,
                cfg.liquidity_pct,
            );
            let cb = buy_bar.saturating_sub(1);
            let ma13 = {
                let start = cb.saturating_sub(12);
                let w = &bars[start..=cb];
                w.iter().map(|b| b.close).sum::<f64>() / w.len() as f64
            };
            let amt20 = {
                let start = cb.saturating_sub(19);
                let w = &bars[start..=cb];
                w.iter().map(|b| b.amount).sum::<f64>() / w.len() as f64
            };
            let mut tradability = Vec::new();
            if one_word {
                tradability.push("执行 bar 一字板（买不进）".to_string());
            } else if gap >= limit_pct - 0.002 {
                tradability.push(format!(
                    "开盘涨幅 {:.2}% ≥ 涨停幅 {:.0}%−0.2%（买不进）",
                    gap * 100.0,
                    limit_pct * 100.0
                ));
            }
            if liq_block {
                tradability.push(format!(
                    "单笔 {:.0} 元 > 执行 bar 成交额×{:.0}%",
                    notional,
                    cfg.liquidity_pct * 100.0
                ));
            }
            signals.push(json!({
                "code": code,
                "buy_date": sig.buy_date,
                "signal_bar_date": bar.date,
                "buy_price": sig.price,
                "exec_bar": {
                    "open": bar.open, "high": bar.high, "low": bar.low, "close": bar.close,
                    "amount": bar.amount,
                },
                "ma13_at_confirm": ma13,
                "ma13_deviation_pct": (bars[cb].close / ma13 - 1.0) * 100.0,
                "amount20": amt20,
                "suggested_notional": notional,
                "stop_loss_price": sig.price * (1.0 - cfg.stop_loss),
                "tradability": tradability,
                "tradable": tradability.is_empty(),
            }));
        }
    }
    // 未指定日期时：取全市场最新的可跟单信号日，过滤到同一天（等价"今天的跟单卡"）
    if p.as_of.is_none() {
        let latest = signals.iter().filter_map(|v| v["buy_date"].as_u64()).max();
        target_date = latest.map(|v| v as u32);
        if let Some(d) = target_date {
            signals.retain(|v| v["buy_date"].as_u64() == Some(d as u64));
        }
    } else {
        target_date = p.as_of;
    }
    signals.sort_by(|a, b| a["code"].as_str().cmp(&b["code"].as_str()));
    Ok(json!({
        "as_of": target_date,
        "symbols_total": total,
        "symbols_scanned": scanned,
        "errors": errors,
        "n_signals": signals.len(),
        "signals": signals,
    }))
}

fn run_backtest_job(spec: &JobSpec, jobs_dir: &Path, job_id: &str) -> Result<Value> {
    use chanlun_data::StoreFormat;
    use chanlun_data::store::Store;
    use chanlun_engine::BacktestConfig;

    let cfg: BacktestConfig = serde_json::from_value(spec.params.clone())
        .context("params 反序列化为 BacktestConfig 失败（字段见 docs/architecture.md）")?;

    let home = home::chanlun_home();
    let data_dir = std::env::var("CHANLUN_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join("data").join("60min"));

    let update_progress = |msg: &str| {
        if let Ok(mut st) = read_status(jobs_dir, job_id) {
            st.progress = Some(msg.to_string());
            let _ = write_status(jobs_dir, &st);
        }
    };

    update_progress("opening store");
    let store = Store::open(data_dir.clone(), StoreFormat::Csv)
        .with_context(|| format!("打开数据仓失败 {}", data_dir.display()))?;
    let universe = store.list_symbols().context("列出标的失败")?;
    update_progress(&format!("running backtest on {} symbols", universe.len()));

    let result =
        chanlun_engine::run(&store, &universe, &cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    update_progress("done");
    Ok(json!({
        "stats": result.stats,
        "n_trades": result.trades.len(),
        "n_blocked": result.blocked.len(),
        "trades_head": result.trades.iter().take(20).collect::<Vec<_>>(),
        "nav_tail": result.nav.iter().rev().take(5).collect::<Vec<_>>(),
    }))
}
