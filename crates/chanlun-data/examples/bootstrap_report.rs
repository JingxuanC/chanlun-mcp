//! 旧仓导入冒烟 / 报告 JSON 输出（T-05 验收用）。
//!
//! ```bash
//! cargo run -p chanlun-data --example bootstrap_report -- <旧仓目录> <新仓目录>
//! ```
//!
//! 完整导入旧 Windows 仓（约 3.1GB / 5,235 只）也是同一条命令，报告里
//! `by_exchange` 会按 sh/sz/bj 分组给出导入数、bar 数与**末行日期**。

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("用法: bootstrap_report <旧仓目录> <新仓目录>");
        return ExitCode::from(2);
    }
    match chanlun_data::store::bootstrap_import(&args[0], &args[1]) {
        Ok(report) => {
            let json = serde_json::to_string_pretty(&report).unwrap_or_else(|err| {
                eprintln!("报告序列化失败: {err}");
                String::new()
            });
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("bootstrap 失败: {err}");
            ExitCode::FAILURE
        }
    }
}
