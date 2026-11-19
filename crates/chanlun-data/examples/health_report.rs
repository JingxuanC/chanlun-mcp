//! 数据健康检查 JSON 输出（T-06 验收 / `doctor` 子命令的参考实现）。
//!
//! ```bash
//! cargo run -p chanlun-data --example health_report -- <数据仓目录>
//! ```
//!
//! 报告按交易所分组：`sh`/`sz` 各自最新日期、落后多少只、最差的是谁（文档 §3 红线）。

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        eprintln!("用法: health_report <数据仓目录>");
        return ExitCode::from(2);
    }
    let store = match chanlun_data::store::Store::open(&args[0], chanlun_data::StoreFormat::Csv) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("打开数据仓失败: {err}");
            return ExitCode::FAILURE;
        }
    };
    match chanlun_data::health::check(&store) {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("健康检查失败: {err}");
            ExitCode::FAILURE
        }
    }
}
