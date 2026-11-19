//! # chanlun-data
//!
//! 60 分钟行情仓 + 公开数据源适配 + 多源交叉校验 + 数据健康检查。
//!
//! ## 数据形态
//!
//! 历史（2020-01-02 起）由一次性 bootstrap 生成（baostock 或从现有 Windows 数据仓导入），
//! 落盘为**单票一个文件**的序列仓：`<root>/<exchange>_<symbol>_60min.csv|parquet`。
//! 日常增量由公开源补最近若干根（东财/新浪/腾讯），并用多源交叉校验兜底。
//!
//! ## 为什么历史必须 bootstrap（实测结论，见 docs/data-sources.md）
//!
//! | 源 | 60min 可回溯 | 含成交额 | 前复权 |
//! |---|---|---|---|
//! | 东方财富 klt=60 | ~124 根（约 1 个月） | ✅ | ✅ fqt=1（前复权；fqt=2 是后复权） |
//! | 新浪 scale=60 | ~1500 根（约 1.5 年） | ✅ | ❌ 不复权 |
//! | 腾讯 m60 | ~640 根（约 5 个月） | ❌ | ❌ 不复权 |
//! | baostock | 2020-01-02 起（7 年） | ✅ | ✅ adjustflag=2 |
//!
//! 免费公开源都拿不到 2020 年起的 60min 历史，因此：
//! - **回测历史**：bootstrap（baostock / Windows 导入）；Rust 运行时零 Python 依赖。
//! - **每日增量 + 多源校验**：东财（最近 1 个月足够补每日 4 根）为主，新浪/腾讯交叉验证。

pub mod health;
pub mod sources;
pub mod store;
pub mod validate;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 数据层错误。
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("CSV 解析错误：{0}")]
    Csv(#[from] csv::Error),
    #[error("HTTP 错误：{0}")]
    Http(String),
    #[error("响应解析错误：{0}")]
    Parse(String),
    #[error("数据不完整：{0}")]
    Incomplete(String),
    #[error("尚未实现：{0}")]
    Unimplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, DataError>;

/// 数据仓配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataConfig {
    /// 数据仓根目录，例：`~/.chanlun/data`
    pub root: PathBuf,
    /// 序列格式（bootstrap 阶段固定 csv，稳定后可切 parquet）
    pub format: StoreFormat,
    /// 增量更新时使用的源（按优先级）
    pub incremental_sources: Vec<String>,
    /// 多源交叉校验容差（bps），0.01% = 1 bps
    pub cross_validate_tolerance_bps: f64,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            root: default_root(),
            format: StoreFormat::Csv,
            incremental_sources: vec!["eastmoney".into(), "sina".into(), "tencent".into()],
            cross_validate_tolerance_bps: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreFormat {
    Csv,
    Parquet,
}

/// 由 6 位代码推断交易所前缀。
///
/// 沪市：`6xxxxx`（含科创板 688）、`9xxxxx`（B 股）；
/// 北交所：`4xxxxx` / `8xxxxx`（2021 年后才有 60min 数据，且公开源覆盖不稳定）；
/// 其余（`0xxxxx` / `2xxxxx` / `3xxxxx`）归深市。
pub fn exchange_for_code(code: &str) -> &'static str {
    match code.chars().next() {
        Some('6') | Some('9') => "sh",
        Some('4') | Some('8') => "bj",
        _ => "sz",
    }
}

/// 规范化标的写法：`600000` / `sh600000` / `SH.600000` / `sz_000001`
/// → `("sh", "600000")`。非法输入返回 `None`。
///
/// 交易所前缀只用于定位文件/拼 URL；6 位代码本身在全市场唯一。
pub fn split_symbol(spec: &str) -> Option<(String, String)> {
    let lower = spec.trim().to_ascii_lowercase();
    let (explicit, rest) = match lower.strip_prefix("sh") {
        Some(rest) => (Some("sh"), rest),
        None => match lower.strip_prefix("sz") {
            Some(rest) => (Some("sz"), rest),
            None => match lower.strip_prefix("bj") {
                Some(rest) => (Some("bj"), rest),
                None => (None, lower.as_str()),
            },
        },
    };
    let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 6 {
        return None;
    }
    let code = digits[digits.len() - 6..].to_string();
    let exchange = explicit
        .map(str::to_string)
        .unwrap_or_else(|| exchange_for_code(&code).to_string());
    Some((exchange, code))
}

pub fn default_root() -> PathBuf {
    std::env::var("CHANLUN_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("chanlun"))
        .join("data")
        .join("60min")
}

/// 从环境变量/参数解析数据仓配置（MCP 层与 CLI 共用）。
pub fn load_config() -> DataConfig {
    let mut cfg = DataConfig::default();
    if let Ok(home) = std::env::var("CHANLUN_HOME") {
        cfg.root = PathBuf::from(home).join("data").join("60min");
    }
    cfg
}
