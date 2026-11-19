//! MCP 服务器（stdio，rmcp 3.x）。
//!
//! 设计见 docs/architecture.md：
//! - 同步工具读仓/读产物，秒回（data_health / get_performance / get_plan / analyze_stock / explain_signal）；
//! - 长任务（run_backtest / scan_signals）走 job 子进程，`get_job` 轮询；
//! - 所有响应带口径提示，防止模型把回测数字当实盘业绩。

use anyhow::Result;
use chanlun_core::{AnalyzerConfig, Direction, SignalKind};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::home;
use crate::jobs;

const CALIBER_WARNINGS: &[&str] = &[
    "前复权口径：收益率/胜率/回撤有效，但买入价与股数不是当时真实盘口，不可与券商成交单对账",
    "dynpos 复利口径容量不成立，实盘请按 legacy 或更小单笔规模设计",
    "回测未建模涨跌停排队与真实盘口深度，实盘可成交性需折让",
];

fn caliber(as_of: &str, funding_mode: &str) -> serde_json::Value {
    json!({
        "as_of": as_of,
        "funding_mode": funding_mode,
        "adjust": "qfq",
        "with_execution_gates": true,
        "warnings": CALIBER_WARNINGS,
    })
}

#[derive(Debug, Clone)]
pub struct ChanlunServer {
    data_dir: PathBuf,
    jobs_dir: PathBuf,
    artifacts_dir: PathBuf,
    #[allow(dead_code)]
    max_jobs: usize,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmptyParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ModeParams {
    /// legacy 或 dynpos；默认 legacy
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StockParams {
    /// 6 位代码，如 600000
    pub code: String,
    /// 截至日期 YYYYMMDD；默认仓内最后一根
    #[serde(default)]
    pub as_of: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScanToolParams {
    /// 目标交易日 YYYYMMDD；省略 = 各票最新 bar
    #[serde(default)]
    pub as_of: Option<String>,
    /// 只扫描前 N 只（调试用）
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobParams {
    pub job_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BacktestToolParams {
    /// legacy 或 dynpos
    #[serde(default)]
    pub funding_mode: Option<String>,
    /// 起始日期 YYYYMMDD（仅约束买入日）
    #[serde(default)]
    pub start_date: Option<String>,
    /// 结束日期 YYYYMMDD
    #[serde(default)]
    pub end_date: Option<String>,
    /// 其余 BacktestConfig 字段（JSON 合并，覆盖默认值）
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

impl ChanlunServer {
    pub fn new(
        data_dir: PathBuf,
        jobs_dir: PathBuf,
        artifacts_dir: PathBuf,
        max_jobs: usize,
    ) -> Self {
        Self {
            data_dir,
            jobs_dir,
            artifacts_dir,
            max_jobs,
            tool_router: Self::tool_router(),
        }
    }

    fn open_store(&self) -> Result<chanlun_data::store::Store, ErrorData> {
        chanlun_data::store::Store::open(self.data_dir.clone(), chanlun_data::StoreFormat::Csv)
            .map_err(|e| {
                ErrorData::internal_error(
                    format!("打开数据仓 {} 失败: {e}", self.data_dir.display()),
                    None,
                )
            })
    }

    fn read_artifact(&self, name: &str) -> Result<serde_json::Value, ErrorData> {
        let path = self.artifacts_dir.join(name);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            ErrorData::internal_error(
                format!(
                    "读不到产物 {}: {e}。请把 v1.0 文档包里的 {name} 放到 {}",
                    path.display(),
                    self.artifacts_dir.display()
                ),
                None,
            )
        })?;
        serde_json::from_str(&text)
            .map_err(|e| ErrorData::internal_error(format!("产物 {name} 不是合法 JSON: {e}"), None))
    }

    /// 读取单票 bar 序列，可选按 as_of 截断。
    fn load_bars(
        &self,
        code: &str,
        as_of: &Option<String>,
    ) -> Result<chanlun_core::BarSeries, ErrorData> {
        let store = self.open_store()?;
        let mut series = store
            .load_series(code)
            .map_err(|e| ErrorData::invalid_params(format!("读 {code} 失败: {e}"), None))?;
        if let Some(d) = as_of {
            let d: u32 = d
                .parse()
                .map_err(|_| ErrorData::invalid_params("as_of 需为 YYYYMMDD".to_string(), None))?;
            series.bars.retain(|b| b.date <= d);
        }
        if series.bars.is_empty() {
            return Err(ErrorData::invalid_params(
                format!("{code} 在指定日期前无 bar"),
                None,
            ));
        }
        Ok(series)
    }
}

#[tool_router(router = tool_router)]
impl ChanlunServer {
    /// 数据健康检查：按 sh/sz 分组统计每只票的末行日期（文档 §3 红线：不许只看汇总）。
    #[tool(
        name = "data_health",
        description = "数据健康检查：按 sh/sz 分组统计末行日期、落后数量与红线判据（ok/notes/by_exchange）。返回 JSON。"
    )]
    pub async fn data_health(&self, _p: Parameters<EmptyParams>) -> Result<String, ErrorData> {
        let store = self.open_store()?;
        let report = chanlun_data::health::check(&store)
            .map_err(|e| ErrorData::internal_error(format!("健康检查失败: {e}"), None))?;
        serde_json::to_string_pretty(&json!({
            "data_dir": self.data_dir.display().to_string(),
            "report": report,
            "warnings": CALIBER_WARNINGS,
        }))
        .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 绩效统计：读 v1.0 文档包复算产物 stats.json。
    #[tool(
        name = "get_performance",
        description = "读取回测绩效（legacy/dynpos）：净值、年化、回撤、胜率、盈亏比、分年、前 5% 交易利润占比。必须原样转述 caliber.warnings。"
    )]
    pub async fn get_performance(
        &self,
        Parameters(ModeParams { mode }): Parameters<ModeParams>,
    ) -> Result<String, ErrorData> {
        let mode = mode.unwrap_or_else(|| "legacy".into());
        let stats = self.read_artifact("stats.json")?;
        let one = stats
            .get(&mode)
            .ok_or_else(|| ErrorData::invalid_params(format!("stats.json 没有 {mode} 口径"), None))?
            .clone();
        let years = one.get("years").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let mut out = json!({ "caliber": caliber("20261116", &mode), "stats": one });
        out["annualization_basis"] = json!(format!(
            "年化基准：{years} 年（自然年 365.25/年）；引擎另有 244 交易日/年口径，引用时需注明"
        ));
        serde_json::to_string_pretty(&out)
            .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 跟单卡：读期末快照（持仓 / 明日卖 / 明日买候选 / 净值预算）。
    #[tool(
        name = "get_plan",
        description = "读取跟单卡快照（模拟盘持仓、明日开盘卖、明日开盘买候选、净值与单笔预算）。数据来自夜间跑批产物 snapshot json。"
    )]
    pub async fn get_plan(&self, _p: Parameters<EmptyParams>) -> Result<String, ErrorData> {
        let snap = self.read_artifact("paper_60min_ma13_dynpos_result_snapshot.json")?;
        let out = json!({
            "caliber": caliber(
                snap.get("final_day").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "dynpos_cap100w"
            ),
            "snapshot": snap,
        });
        serde_json::to_string_pretty(&out)
            .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 单票结构/离场距离。
    #[tool(
        name = "analyze_stock",
        description = "单票分析：最新收盘、MA13、距 5% 止损/MA13/中枢 ZD 距离，以及最近笔、最近中枢、最近一买/二买。结构由 chanlun-core 提供（已与 Python 逐信号对拍）。"
    )]
    pub async fn analyze_stock(
        &self,
        Parameters(StockParams { code, as_of }): Parameters<StockParams>,
    ) -> Result<String, ErrorData> {
        let series = self.load_bars(&code, &as_of)?;
        let bars = &series.bars;
        let last = bars.last().expect("non-empty");
        let ma13 = {
            let w = &bars[bars.len().saturating_sub(13)..];
            w.iter().map(|b| b.close).sum::<f64>() / w.len() as f64
        };
        let cfg = AnalyzerConfig::default();
        let analysis = chanlun_core::analyze(bars, &cfg);

        let structure = match &analysis {
            Ok(a) => {
                let recent_bis: Vec<serde_json::Value> = a
                    .bis
                    .iter()
                    .rev()
                    .take(5)
                    .map(|bi| {
                        json!({
                            "direction": match bi.direction { Direction::Up => "up", Direction::Down => "down" },
                            "start_date": bi.start_date,
                            "end_date": bi.end_date,
                            "start_price": bi.start_price,
                            "end_price": bi.end_price,
                            "momentum": bi.momentum,
                            "confirmed": bi.is_confirmed,
                        })
                    })
                    .collect();
                let latest_zs = a.zhongshus.last().map(|zs| {
                    json!({
                        "zg": zs.zg, "zd": zs.zd,
                        "start_date": zs.start_date, "end_date": zs.end_date,
                        "start_bi": zs.start_idx, "end_bi": zs.end_idx,
                    })
                });
                json!({
                    "structure_ready": true,
                    "recent_bis": recent_bis,
                    "latest_zhongshu": latest_zs,
                    "latest_b1": a.b1.last().map(|s| json!({"date": s.date, "low": s.low})),
                    "latest_b2": a.b2.last().map(|s| json!({"date": s.date, "low": s.low})),
                })
            }
            Err(e) => json!({"structure_ready": false, "error": format!("{e}")}),
        };

        let stop_loss = last.close * 0.95;
        let to_zd = analysis
            .ok()
            .as_ref()
            .and_then(|a| a.zhongshus.last())
            .map(|zs| (last.close / zs.zd - 1.0) * 100.0);
        let out = json!({
            "caliber": caliber(&last.date.to_string(), "n/a"),
            "code": code,
            "last_date": last.date,
            "last_close": last.close,
            "ma13": ma13,
            "ma13_deviation_pct": (last.close / ma13 - 1.0) * 100.0,
            "stop_loss_price": stop_loss,
            "to_stop_loss_pct": (last.close / stop_loss - 1.0) * 100.0,
            "to_zd_pct": to_zd,
            "bars": bars.len(),
            "structure": structure,
        });
        serde_json::to_string_pretty(&out)
            .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 信号解释：二买证据链。
    #[tool(
        name = "explain_signal",
        description = "解释二买信号证据链：一买低点、反弹笔/回调笔价格与力度、回调是否破前低、中枢位置、确认 bar 收盘与 MA13 乖离、20 根均成交额。as_of 省略时取最近一个二买。"
    )]
    pub async fn explain_signal(
        &self,
        Parameters(StockParams { code, as_of }): Parameters<StockParams>,
    ) -> Result<String, ErrorData> {
        let series = self.load_bars(&code, &as_of)?;
        let bars = &series.bars;
        if bars.len() < 60 {
            return Err(ErrorData::invalid_params(
                format!("{code} bar 太少（{}）", bars.len()),
                None,
            ));
        }
        let cfg = AnalyzerConfig::default();
        let analysis = chanlun_core::analyze(bars, &cfg)
            .map_err(|e| ErrorData::internal_error(format!("结构计算失败: {e}"), None))?;
        let target = analysis
            .b2
            .last()
            .ok_or_else(|| ErrorData::invalid_params(format!("{code} 没有二买信号"), None))?;

        let pullback = analysis.bis.get(target.bi_idx);
        let bounce = target
            .bi_idx
            .checked_sub(1)
            .and_then(|i| analysis.bis.get(i));
        let first_low = target
            .first_buy_bi_idx
            .and_then(|i| analysis.bis.get(i))
            .map(|b| b.start_price.min(b.end_price))
            .unwrap_or(target.low);
        let bar_idx = target.raw_index.min(bars.len() - 1);
        let ma13 = {
            let st = bar_idx.saturating_sub(12);
            let w = &bars[st..=bar_idx];
            w.iter().map(|b| b.close).sum::<f64>() / w.len() as f64
        };
        let amt20 = {
            let st = bar_idx.saturating_sub(19);
            let w = &bars[st..=bar_idx];
            w.iter().map(|b| b.amount).sum::<f64>() / w.len() as f64
        };
        let out = json!({
            "caliber": caliber(&bars[bar_idx].date.to_string(), "n/a"),
            "code": code,
            "signal": {
                "kind": match target.kind { SignalKind::B1 => "B1", SignalKind::B2 => "B2" },
                "date": target.date,
                "pullback_low": target.low,
                "b1_low": first_low,
                "pullback_holds_b1_low": target.low >= first_low * (1.0 - 0.005),
            },
            "evidence": {
                "bounce": bounce.map(|b| json!({"start": b.start_price, "end": b.end_price, "momentum": b.momentum})),
                "pullback": pullback.map(|b| json!({"start": b.start_price, "end": b.end_price, "momentum": b.momentum})),
                "momentum_weaker": match (bounce, pullback) {
                    (Some(bp), Some(pb)) => json!(pb.momentum < bp.momentum),
                    _ => json!(null),
                },
                "zhongshu": target.zhongshu_idx.and_then(|i| analysis.zhongshus.get(i))
                    .map(|zs| json!({"zg": zs.zg, "zd": zs.zd})),
                "confirm_bar_close": bars[bar_idx].close,
                "ma13_at_confirm": ma13,
                "ma13_deviation_pct": (bars[bar_idx].close / ma13 - 1.0) * 100.0,
                "amt20": amt20,
            },
            "note": "证据链按生产口径（Phase B：确认收盘 ≤ MA13×1.01；Phase C：amt20 < 5000 万）判定；未含 P1/P2 可成交性闸门。"
        });
        serde_json::to_string_pretty(&out)
            .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 全市场当日扫描（异步 job）。
    #[tool(
        name = "scan_signals",
        description = "提交全市场当日二买扫描异步任务（子进程 job），立即返回 job_id，用 get_job 轮询。参数：as_of(YYYYMMDD，省略=各票最新 bar)、limit(调试用，只扫前 N 只)。结果含每个信号的可成交性/建议金额/止损位。"
    )]
    pub async fn scan_signals(
        &self,
        Parameters(p): Parameters<ScanToolParams>,
    ) -> Result<String, ErrorData> {
        let mut params = json!({});
        if let Some(d) = p.as_of {
            params["as_of"] = json!(d.parse::<u32>().map_err(|_| {
                ErrorData::invalid_params("as_of 需为 YYYYMMDD".to_string(), None)
            })?);
        }
        if let Some(l) = p.limit {
            params["limit"] = json!(l);
        }
        let job_id = jobs::submit(&self.jobs_dir, jobs::JobKind::Scan, params)
            .map_err(|e| ErrorData::internal_error(format!("提交 scan job 失败: {e}"), None))?;
        serde_json::to_string_pretty(&json!({
            "job_id": job_id,
            "kind": "scan",
            "hint": "用 get_job(job_id) 轮询；全市场扫描是分钟级",
        }))
        .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 提交回测 job（异步）。
    #[tool(
        name = "run_backtest",
        description = "提交组合回测异步任务，立即返回 job_id；用 get_job 轮询状态/结果。参数：funding_mode(legacy|dynpos)、start_date/end_date(YYYYMMDD)、params(JSON 覆盖 BacktestConfig)。长任务，不要同步等待。"
    )]
    pub async fn run_backtest(
        &self,
        Parameters(p): Parameters<BacktestToolParams>,
    ) -> Result<String, ErrorData> {
        let mut params = p.params.unwrap_or_else(|| json!({}));
        if let Some(v) = p.funding_mode {
            // 对外统一用 legacy / dynpos 两个别名；engine 的 serde 变体名是 snake_case
            let normalized = match v.as_str() {
                "legacy" | "Legacy" => "legacy",
                "dynpos" | "dynpos_cap100w" | "DynPosCap100w" => "dyn_pos_cap100w",
                other => other,
            };
            params["funding_mode"] = json!(normalized);
        }
        if let Some(v) = p.start_date {
            params["start_date"] = json!(v.parse::<u32>().map_err(|_| {
                ErrorData::invalid_params("start_date 需为 YYYYMMDD".to_string(), None)
            })?);
        }
        if let Some(v) = p.end_date {
            params["end_date"] = json!(v.parse::<u32>().map_err(|_| {
                ErrorData::invalid_params("end_date 需为 YYYYMMDD".to_string(), None)
            })?);
        }
        let job_id = jobs::submit(&self.jobs_dir, jobs::JobKind::Backtest, params)
            .map_err(|e| ErrorData::internal_error(format!("提交 job 失败: {e}"), None))?;
        serde_json::to_string_pretty(&json!({
            "job_id": job_id,
            "hint": "用 get_job(job_id) 轮询；状态 queued/running/done/failed/cancelled",
        }))
        .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 查询 job 状态/结果。
    #[tool(
        name = "get_job",
        description = "查询回测/扫描 job 的状态与结果（state/progress/result/error）。"
    )]
    pub async fn get_job(
        &self,
        Parameters(JobParams { job_id }): Parameters<JobParams>,
    ) -> Result<String, ErrorData> {
        let status = jobs::read_status(&self.jobs_dir, &job_id)
            .map_err(|e| ErrorData::invalid_params(format!("读 job {job_id} 失败: {e}"), None))?;
        let result = jobs::read_result(&self.jobs_dir, &job_id).ok();
        serde_json::to_string_pretty(&json!({
            "job_id": job_id,
            "status": status,
            "result": result,
        }))
        .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }

    /// 取消 job。
    #[tool(
        name = "cancel_job",
        description = "取消一个 queued/running 的回测或扫描 job（kill 子进程并标记 cancelled）。"
    )]
    pub async fn cancel_job(
        &self,
        Parameters(JobParams { job_id }): Parameters<JobParams>,
    ) -> Result<String, ErrorData> {
        jobs::cancel(&self.jobs_dir, &job_id)
            .map_err(|e| ErrorData::invalid_params(format!("取消 job {job_id} 失败: {e}"), None))?;
        serde_json::to_string_pretty(&json!({"job_id": job_id, "state": "cancelled"}))
            .map_err(|e| ErrorData::internal_error(format!("序列化失败: {e}"), None))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChanlunServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("chanlun-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "A 股 60min 缠论二买策略数据服务。所有绩效为前复权回测口径，不可与券商成交单对账；dynpos 容量不成立。长任务用 run_backtest / scan_signals → get_job。",
            )
    }
}

/// 启动 stdio MCP 服务器（阻塞直到 stdin 关闭）。
pub async fn serve_async(
    data_dir: PathBuf,
    jobs_dir: PathBuf,
    artifacts_dir: PathBuf,
    max_jobs: usize,
) -> Result<()> {
    std::fs::create_dir_all(&jobs_dir)?;
    let server = ChanlunServer::new(data_dir, jobs_dir, artifacts_dir, max_jobs);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// 同步入口（CLI 用）。
pub fn serve(data: Option<PathBuf>, jobs: Option<PathBuf>, max_jobs: usize) -> Result<()> {
    let home = home::chanlun_home();
    let data_dir = data.unwrap_or_else(|| home.join("data").join("60min"));
    let jobs_dir = jobs.unwrap_or_else(|| home.join("jobs"));
    let artifacts_dir = home.join("artifacts");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve_async(data_dir, jobs_dir, artifacts_dir, max_jobs))
}
