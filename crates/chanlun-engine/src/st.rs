//! 时点 ST 标记表（对齐 Python `st_flags.json` + `is_st`）。
//!
//! Python 表格式：
//!
//! ```json
//! { "stocks": { "sh_600000": { "st": [[20200101, 20201231], ...] }, ... } }
//! ```
//!
//! - key = `("sh_" if code.startswith("6") else "sz_") + code`（北交所 8/4 开头也落 `sz_`，
//!   与 Python 行为一致）；
//! - 区间为闭区间 `YYYYMMDD`；
//! - 表缺失 / 解析失败 / 该票无记录 → 一律按**非 ST** 处理（Python 同样行为）。
//!
//! 注意：`board_limit` 的 ST 5% 涨跌停与 Phase D 的排除 ST 都依赖这张表；
//! 表本身是 point-in-time 的，缺失会把 ST 票按 10% 涨停幅处理，只会放宽闸门，不会误杀。

use std::collections::HashMap;
use std::path::Path;

use chanlun_core::Date8;

use crate::EngineError;

/// 时点 ST 表。
#[derive(Debug, Clone, Default)]
pub struct StTable {
    stocks: HashMap<String, Vec<(u32, u32)>>,
}

impl StTable {
    /// 空表（全部按非 ST）。
    pub fn empty() -> Self {
        Self::default()
    }

    /// 从 JSON 字符串解析（容忍缺字段；解析失败返回 `Err`，由调用方决定是否降级）。
    pub fn from_json_str(text: &str) -> Result<Self, EngineError> {
        let root: serde_json::Value = serde_json::from_str(text)?;
        let mut table = Self::default();
        let Some(stocks) = root.get("stocks").and_then(|v| v.as_object()) else {
            return Ok(table);
        };
        for (key, rec) in stocks {
            let Some(ranges) = rec.get("st").and_then(|v| v.as_array()) else {
                continue;
            };
            let mut out = Vec::with_capacity(ranges.len());
            for r in ranges {
                let Some(pair) = r.as_array() else { continue };
                if pair.len() < 2 {
                    continue;
                }
                let (Some(a), Some(b)) = (pair[0].as_u64(), pair[1].as_u64()) else {
                    continue;
                };
                out.push((a as u32, b as u32));
            }
            if !out.is_empty() {
                table.stocks.insert(key.clone(), out);
            }
        }
        Ok(table)
    }

    /// 从文件加载；读失败返回 `Err`（生产上 Python 是「加载失败 → 全部按非 ST」，
    /// 调用方可捕获后改用 [`StTable::empty`] 降级）。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, EngineError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json_str(&text)
    }

    /// Python 侧的 key 规则：6 开头 → sh_，其余 → sz_。
    pub fn key(code: &str) -> String {
        let prefix = if code.starts_with('6') { "sh_" } else { "sz_" };
        format!("{prefix}{code}")
    }

    /// 手工插入（测试/内存态使用）。
    pub fn insert(&mut self, code: &str, ranges: Vec<(u32, u32)>) {
        self.stocks.insert(Self::key(code), ranges);
    }

    /// 该票在 `date8` 当天是否 ST（表缺失 → false）。
    pub fn is_st(&self, code: &str, date8: Date8) -> bool {
        let Some(rec) = self.stocks.get(&Self::key(code)) else {
            return false;
        };
        let d = date8;
        rec.iter().any(|&(a, b)| a <= d && d <= b)
    }

    pub fn len(&self) -> usize {
        self.stocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
      "generated": "2026-11-17",
      "stocks": {
        "sh_600000": {"st": [[20200101, 20200301], [20210101, 20210131]]},
        "sz_000001": {"st": []},
        "sz_300750": {"st": [[20200601, 20200630]]}
      }
    }"#;

    #[test]
    fn parses_ranges_and_checks_dates() {
        let t = StTable::from_json_str(JSON).unwrap();
        assert_eq!(t.len(), 2, "空 st 记录的票被跳过");
        assert!(t.is_st("600000", 20200201));
        assert!(t.is_st("600000", 20200101));
        assert!(t.is_st("600000", 20200301));
        assert!(!t.is_st("600000", 20200302));
        assert!(!t.is_st("600000", 20201231));
        assert!(t.is_st("600000", 20210115));
        assert!(t.is_st("300750", 20200615));
        assert!(!t.is_st("300750", 20200701));
    }

    #[test]
    fn unknown_code_and_bad_json_are_non_st() {
        let t = StTable::from_json_str(JSON).unwrap();
        assert!(!t.is_st("000002", 20200101));
        assert!(!StTable::empty().is_st("600000", 20200101));
        assert!(StTable::from_json_str("not json").is_err());
        // 缺 stocks 字段 → 空表而非报错
        let t = StTable::from_json_str(r#"{"generated":"x"}"#).unwrap();
        assert!(t.is_empty());
    }

    #[test]
    fn key_prefix_matches_python() {
        assert_eq!(StTable::key("600000"), "sh_600000");
        assert_eq!(StTable::key("900901"), "sz_900901");
        assert_eq!(StTable::key("000001"), "sz_000001");
        assert_eq!(StTable::key("830799"), "sz_830799");
    }
}
