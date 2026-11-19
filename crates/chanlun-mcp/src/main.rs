//! chanlun-mcp 可执行入口。
//!
//! 子命令：
//! - `serve`     : 以 stdio 启动 MCP 服务器（给 Claude/DSH 的 mcpServers 用）
//! - `worker`    : 内部子进程，执行一个 job（回测/审计），写 status/result
//! - `bootstrap` : 一次性历史数据导入（旧 Windows 仓 / CSV → 数据仓）
//! - `doctor`    : 数据健康检查（按 sh/sz 分组末行日期，文档 §3 红线）

#![allow(dead_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod home;
mod jobs;
mod prompts;
mod resources;
mod server;
mod tools;

#[derive(Parser)]
#[command(
    name = "chanlun-mcp",
    version,
    about = "缠论二买策略 MCP 服务（stdio）+ 异步回测 job"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 以 stdio 传输启动 MCP 服务器
    Serve {
        /// 数据仓目录（默认 $CHANLUN_HOME/data/60min）
        #[arg(long, env = "CHANLUN_DATA")]
        data: Option<PathBuf>,
        /// job 目录（默认 $CHANLUN_HOME/jobs）
        #[arg(long, env = "CHANLUN_JOBS")]
        jobs: Option<PathBuf>,
        /// 最大并发回测 job 数（预留）
        #[arg(long, default_value_t = 2)]
        max_jobs: usize,
    },
    /// 内部子进程：执行一个 job
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        job_id: String,
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        jobs_dir: PathBuf,
    },
    /// 一次性历史数据导入
    Bootstrap {
        /// 源数据目录（旧 Windows 数据仓：sh_600000_60min.csv 形式）
        #[arg(long)]
        from: PathBuf,
        /// 目标数据仓
        #[arg(long, env = "CHANLUN_DATA")]
        to: Option<PathBuf>,
    },
    /// 数据健康检查
    Doctor {
        #[arg(long, env = "CHANLUN_DATA")]
        data: Option<PathBuf>,
        /// 输出 JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn default_home() -> PathBuf {
    home::chanlun_home()
}

fn default_data_dir() -> PathBuf {
    std::env::var("CHANLUN_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_home().join("data").join("60min"))
}

fn main() -> Result<()> {
    // 重要：stdio MCP 的 stdout 是协议通道，日志必须走 stderr。
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            data,
            jobs,
            max_jobs,
        } => server::serve(data, jobs, max_jobs),
        Command::Worker {
            job_id,
            spec,
            jobs_dir,
        } => jobs::run_worker(&job_id, &spec, &jobs_dir),
        Command::Bootstrap { from, to } => {
            let to = to.unwrap_or_else(default_data_dir);
            let report = chanlun_data::store::bootstrap_import(&from, &to)
                .map_err(|e| anyhow::anyhow!("bootstrap 失败: {e}"))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Command::Doctor { data, json } => {
            let dir = data.unwrap_or_else(default_data_dir);
            let store =
                chanlun_data::store::Store::open(dir.clone(), chanlun_data::StoreFormat::Csv)
                    .map_err(|e| anyhow::anyhow!("打开数据仓 {} 失败: {e}", dir.display()))?;
            let report = chanlun_data::health::check(&store)
                .map_err(|e| anyhow::anyhow!("健康检查失败: {e}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("data_root: {}", report.data_root);
                println!("total_symbols: {}", report.total_symbols);
                for (ex, h) in &report.by_exchange {
                    println!(
                        "[{ex}] symbols={} last_date={} stale={} examples={:?}",
                        h.total_symbols, h.last_date, h.stale_count, h.stale_examples
                    );
                }
                println!("ok: {}", report.ok);
                for note in &report.notes {
                    println!("note: {note}");
                }
            }
            Ok(())
        }
    }
}
