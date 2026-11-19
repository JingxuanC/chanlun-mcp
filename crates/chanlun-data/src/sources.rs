//! 公开数据源适配（60min）。
//!
//! 统一接口：`fetch_60min(symbol) -> Result<Vec<Bar>>`（按时间升序，价格口径由 `Adjust` 标明）。
//! 实测可回溯深度与口径差异见 `docs/data-sources.md`；这里**不做复权换算**，复权由
//! `store`/`validate` 层按"不复权 + 因子表/锚定收盘比"统一处理。
//!
//! 实测要点（2026-11-17 本机复测，HTTP 直连）：
//! | 源 | URL 关键参数 | 深度 | 成交额 | volume 单位 | 复权 |
//! |---|---|---|---|---|---|
//! | 东方财富 | `klt=60&fqt=1` | ~124 根 | ✅ f57 元 | 手（×100 转股） | 前复权 |
//! | 新浪 | `scale=60&datalen=1500` | ~1500 根 | ✅ amount 元 | 股 | 不复权 |
//! | 腾讯 | `m60,,640` | ~640 根 | ❌（字段 `{}`） | 手（×100 转股） | 不复权 |
//!
//! 注意：东财 `fqt=2` 实测是**后复权**（600000 收盘被放大到 ~100），
//! 与历史仓（baostock `adjustflag=2` 前复权）对齐必须用 `fqt=1`。
//!
//! `Bar.volume` 统一为**股**、`amount` 为**元**（与旧仓 baostock 口径一致）。

use crate::{DataError, Result, split_symbol};
use chanlun_core::Bar;
use serde_json::Value;
use std::time::Duration;

/// 浏览器 UA（公开接口对默认 UA 不友好；实测带 UA 稳定返回）。
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// 默认 HTTP 超时（秒）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// 东财 kline 端点。
pub const EASTMONEY_ENDPOINT: &str = "https://push2his.eastmoney.com/api/qt/stock/kline/get";
/// 东财复权参数：1 = 前复权（与历史仓口径一致；2 是后复权，勿用）。
pub const EASTMONEY_FQT: u32 = 1;
/// 东财单次最多返回根数（服务端实际只留最近约 124 根）。
pub const EASTMONEY_LMT: usize = 500;

/// 新浪 kline 端点。
pub const SINA_ENDPOINT: &str =
    "https://quotes.sina.cn/cn/api/json_v2.php/CN_MarketDataService.getKLineData";
/// 新浪 datalen 上限约 1500（2000 返回 null）。
pub const SINA_MAX_DATALEN: usize = 1500;

/// 腾讯 kline 端点。
pub const TENCENT_ENDPOINT: &str = "https://ifzq.gtimg.cn/appstock/app/kline/mkline";
/// 腾讯 m60 大约 640 根。
pub const TENCENT_MAX_BARS: usize = 640;

/// 价格口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjust {
    /// 前复权（以东财抓取时刻为基准）
    Qfq,
    /// 不复权
    None,
}

/// 60min bar 数据源。
pub trait BarSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn adjust(&self) -> Adjust;
    /// 该源 60min 历史大约能回溯多少根（用于健康检查/告警）
    fn approx_depth_bars(&self) -> usize;
    fn fetch_60min(&self, symbol: &str) -> Result<Vec<Bar>>;
}

/// 东方财富：`push2his.eastmoney.com/api/qt/stock/kline/get?klt=60&fqt=1`
///
/// 实测：~124 根（约 1 个月），含成交额，前复权。适合**每日增量**。
pub struct EastmoneySource {
    pub timeout_secs: u64,
}

impl Default for EastmoneySource {
    fn default() -> Self {
        Self {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl BarSource for EastmoneySource {
    fn name(&self) -> &'static str {
        "eastmoney"
    }
    fn adjust(&self) -> Adjust {
        Adjust::Qfq
    }
    fn approx_depth_bars(&self) -> usize {
        124
    }
    fn fetch_60min(&self, symbol: &str) -> Result<Vec<Bar>> {
        let url = eastmoney_url(symbol, EASTMONEY_LMT)?;
        let body = fetch_text(
            &url,
            self.timeout_secs,
            "eastmoney",
            "https://quote.eastmoney.com/",
        )?;
        parse_eastmoney(&body)
    }
}

/// 新浪：`quotes.sina.cn/.../getKLineData?scale=60&datalen=N`
///
/// 实测：`datalen=1500` 可回溯约 1.5 年，含成交额，**不复权**。适合增量 + 主源交叉校验。
pub struct SinaSource {
    pub datalen: usize,
}

impl Default for SinaSource {
    fn default() -> Self {
        Self {
            datalen: SINA_MAX_DATALEN,
        }
    }
}

impl BarSource for SinaSource {
    fn name(&self) -> &'static str {
        "sina"
    }
    fn adjust(&self) -> Adjust {
        Adjust::None
    }
    fn approx_depth_bars(&self) -> usize {
        SINA_MAX_DATALEN
    }
    fn fetch_60min(&self, symbol: &str) -> Result<Vec<Bar>> {
        let url = sina_url(symbol, self.datalen)?;
        let body = fetch_text(
            &url,
            DEFAULT_TIMEOUT_SECS,
            "sina",
            "https://finance.sina.com.cn/",
        )?;
        parse_sina(&body)
    }
}

/// 腾讯：`ifzq.gtimg.cn/appstock/app/kline/mkline?param=sh600000,m60,,640`
///
/// 实测：~640 根（约 5 个月），**无成交额**、不复权。仅作第三源校验，不用于口径基准。
pub struct TencentSource {
    pub bars: usize,
}

impl Default for TencentSource {
    fn default() -> Self {
        Self {
            bars: TENCENT_MAX_BARS,
        }
    }
}

impl BarSource for TencentSource {
    fn name(&self) -> &'static str {
        "tencent"
    }
    fn adjust(&self) -> Adjust {
        Adjust::None
    }
    fn approx_depth_bars(&self) -> usize {
        TENCENT_MAX_BARS
    }
    fn fetch_60min(&self, symbol: &str) -> Result<Vec<Bar>> {
        let url = tencent_url(symbol, self.bars)?;
        let body = fetch_text(&url, DEFAULT_TIMEOUT_SECS, "tencent", "https://gu.qq.com/")?;
        parse_tencent(&body)
    }
}

/// 当前可用的公开 60min 源（按优先级：东财主源 → 新浪备源 → 腾讯第三源）。
pub fn default_sources() -> Vec<Box<dyn BarSource>> {
    vec![
        Box::new(EastmoneySource::default()),
        Box::new(SinaSource::default()),
        Box::new(TencentSource::default()),
    ]
}

/// 按名字取源（对应 `DataConfig.incremental_sources`）。
pub fn source_by_name(name: &str) -> Option<Box<dyn BarSource>> {
    match name.trim().to_ascii_lowercase().as_str() {
        "eastmoney" | "em" | "东方财富" => Some(Box::new(EastmoneySource::default())),
        "sina" | "新浪" => Some(Box::new(SinaSource::default())),
        "tencent" | "qq" | "腾讯" => Some(Box::new(TencentSource::default())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// URL 构造（纯函数，便于单测；不联网）
// ---------------------------------------------------------------------------

fn normalized(symbol: &str) -> Result<(String, String)> {
    split_symbol(symbol).ok_or_else(|| DataError::Parse(format!("非法标的代码：{symbol}")))
}

/// 东财 secid：沪市 `1.<code>`，深市/北交所 `0.<code>`。
fn eastmoney_secid(symbol: &str) -> Result<String> {
    let (exchange, code) = normalized(symbol)?;
    let market = match exchange.as_str() {
        "sh" => "1",
        // 深市、北交所都是 0；北交所在公开源上覆盖不稳定，保留前缀便于排错。
        _ => "0",
    };
    Ok(format!("{market}.{code}"))
}

pub fn eastmoney_url(symbol: &str, lmt: usize) -> Result<String> {
    let secid = eastmoney_secid(symbol)?;
    Ok(format!(
        "{EASTMONEY_ENDPOINT}?secid={secid}\
         &fields1=f1,f2,f3,f4,f5,f6\
         &fields2=f51,f52,f53,f54,f55,f56,f57\
         &klt=60&fqt={EASTMONEY_FQT}&beg=0&end=20500101&lmt={lmt}"
    ))
}

pub fn sina_url(symbol: &str, datalen: usize) -> Result<String> {
    let (exchange, code) = normalized(symbol)?;
    Ok(format!(
        "{SINA_ENDPOINT}?symbol={exchange}{code}&scale=60&ma=no&datalen={datalen}"
    ))
}

pub fn tencent_url(symbol: &str, bars: usize) -> Result<String> {
    let (exchange, code) = normalized(symbol)?;
    Ok(format!(
        "{TENCENT_ENDPOINT}?param={exchange}{code},m60,,{bars}"
    ))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn fetch_text(url: &str, timeout_secs: u64, source: &'static str, referer: &str) -> Result<String> {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("Referer", referer)
        .call()
        .map_err(|err| classify_fetch_error(source, err))?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|err| classify_fetch_error(source, err))
}

/// 错误分类：HTTP 状态码 / 超时 / 网络/协议错误，都带上源名与 URL 便于排错。
fn classify_fetch_error(source: &'static str, err: ureq::Error) -> DataError {
    match err {
        ureq::Error::StatusCode(code) => {
            DataError::Http(format!("{source}: HTTP {code}（限流/参数错误/接口变更）"))
        }
        ureq::Error::Timeout(timeout) => {
            DataError::Http(format!("{source}: 请求超时（{timeout}）"))
        }
        ureq::Error::BadUri(uri) => DataError::Http(format!("{source}: URL 非法（{uri}）")),
        other => DataError::Http(format!("{source}: {other}")),
    }
}

// ---------------------------------------------------------------------------
// 响应解析（纯函数：离屏 JSON 单测覆盖，可离线）
// ---------------------------------------------------------------------------

fn parse_f64(raw: &str, ctx: &str) -> Result<f64> {
    raw.trim()
        .parse::<f64>()
        .map_err(|_| DataError::Parse(format!("{ctx}: 数值非法 {raw:?}")))
}

fn json_number(value: &Value, ctx: &str) -> Result<f64> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 数值越界 {n}"))),
        Value::String(s) => parse_f64(s, ctx),
        other => Err(DataError::Parse(format!("{ctx}: 期望数值，实际 {other}"))),
    }
}

fn object_number(item: &Value, key: &str) -> Result<f64> {
    let value = item
        .get(key)
        .ok_or_else(|| DataError::Parse(format!("缺少字段 {key}")))?;
    json_number(value, key)
}

/// 丢弃明显无效的 bar（停牌/空行），但保留正常 bar。
fn push_bar(bars: &mut Vec<Bar>, bar: Bar) -> Result<()> {
    let finite = [
        bar.open, bar.high, bar.low, bar.close, bar.volume, bar.amount,
    ]
    .iter()
    .all(|v| v.is_finite());
    if !finite {
        return Err(DataError::Parse(format!(
            "{}: bar 含非有限数值 {bar:?}",
            bar.date
        )));
    }
    if bar.close <= 0.0 || bar.high <= 0.0 || bar.low <= 0.0 {
        // 停牌/未成交 bar：源可能返回 0，直接丢弃。
        return Ok(());
    }
    if bar.high + 1e-9 < bar.low {
        return Err(DataError::Parse(format!(
            "{}: high {} < low {}",
            bar.date, bar.high, bar.low
        )));
    }
    bars.push(bar);
    Ok(())
}

fn sort_bars(bars: &mut [Bar]) {
    bars.sort_by_key(|bar| bar.date); // 稳定排序：同一天保持源返回的日内顺序
}

fn ensure_not_empty(source: &'static str, bars: Vec<Bar>) -> Result<Vec<Bar>> {
    if bars.is_empty() {
        Err(DataError::Incomplete(format!(
            "{source}: 返回 0 根有效 bar（代码不存在/停牌/接口变更？）"
        )))
    } else {
        Ok(bars)
    }
}

/// 解析东财 `data.klines`：`"2026-09-29 10:30,open,close,high,low,volume,amount"`。
///
/// 注意字段顺序是 **open, close, high, low**（不是 OHLC）。
/// `volume` 单位"手" → ×100 转成股（与旧仓 baostock 一致）。
pub fn parse_eastmoney(body: &str) -> Result<Vec<Bar>> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| DataError::Parse(format!("eastmoney JSON 解析失败：{err}")))?;
    let rc = root.get("rc").and_then(Value::as_i64).unwrap_or(-1);
    if rc != 0 {
        let msg = root.get("data").and_then(|d| d.get("name")).cloned();
        return Err(DataError::Parse(format!(
            "eastmoney rc={rc}（data={msg:?}）"
        )));
    }
    let klines = root
        .pointer("/data/klines")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            DataError::Incomplete("eastmoney: data.klines 缺失（代码不存在/停牌？）".into())
        })?;
    let mut bars = Vec::with_capacity(klines.len());
    for (i, row) in klines.iter().enumerate() {
        let ctx = format!("eastmoney klines[{i}]");
        let line = row
            .as_str()
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 期望字符串，实际 {row}")))?;
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 7 {
            return Err(DataError::Parse(format!(
                "{ctx}: 字段不足（期望 7 个）：{line:?}"
            )));
        }
        let date = crate::store::parse_date8(fields[0])
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 日期非法 {:?}", fields[0])))?;
        let bar = Bar {
            date,
            open: parse_f64(fields[1], &ctx)?,
            close: parse_f64(fields[2], &ctx)?,
            high: parse_f64(fields[3], &ctx)?,
            low: parse_f64(fields[4], &ctx)?,
            volume: parse_f64(fields[5], &ctx)? * 100.0,
            amount: parse_f64(fields[6], &ctx)?,
        };
        push_bar(&mut bars, bar)?;
    }
    sort_bars(&mut bars);
    ensure_not_empty("eastmoney", bars)
}

/// 解析新浪 `[{"day":"2025-05-06 10:30:00","open":"...","high":...,"amount":"..."}]`。
///
/// 新浪 `symbol` 无效时返回字面量 `null`。`volume` 单位已是股。
pub fn parse_sina(body: &str) -> Result<Vec<Bar>> {
    let trimmed = body.trim();
    if trimmed == "null" || trimmed.is_empty() {
        return Err(DataError::Incomplete(
            "sina: 返回 null/空（代码无效或 datalen 超过上限）".into(),
        ));
    }
    let root: Value = serde_json::from_str(trimmed)
        .map_err(|err| DataError::Parse(format!("sina JSON 解析失败：{err}")))?;
    let rows = root
        .as_array()
        .ok_or_else(|| DataError::Parse("sina: 响应不是数组（接口变更/限流页面？）".into()))?;
    let mut bars = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let ctx = format!("sina[{i}]");
        let day = row
            .get("day")
            .and_then(Value::as_str)
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 缺 day 字段")))?;
        let date = crate::store::parse_date8(day)
            .ok_or_else(|| DataError::Parse(format!("{ctx}: day 非法 {day:?}")))?;
        let bar = Bar {
            date,
            open: object_number(row, "open").map_err(|e| annotate(&ctx, e))?,
            high: object_number(row, "high").map_err(|e| annotate(&ctx, e))?,
            low: object_number(row, "low").map_err(|e| annotate(&ctx, e))?,
            close: object_number(row, "close").map_err(|e| annotate(&ctx, e))?,
            volume: object_number(row, "volume").map_err(|e| annotate(&ctx, e))?,
            amount: row
                .get("amount")
                .map(|v| json_number(v, "amount"))
                .transpose()
                .map_err(|e| annotate(&ctx, e))?
                .unwrap_or(0.0),
        };
        push_bar(&mut bars, bar)?;
    }
    sort_bars(&mut bars);
    ensure_not_empty("sina", bars)
}

/// 解析腾讯 `data.<sh600000>.m60`：`["202611171030","open","close","high","low","volume",{...},...]`。
///
/// `volume` 单位"手" → ×100 转成股；`amount` 为 `{}`（无成交额）→ 0.0。
pub fn parse_tencent(body: &str) -> Result<Vec<Bar>> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| DataError::Parse(format!("tencent JSON 解析失败：{err}")))?;
    let code = root.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let msg = root.get("msg").and_then(Value::as_str).unwrap_or("unknown");
        return Err(DataError::Parse(format!("tencent code={code}（{msg}）")));
    }
    let data = root
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| DataError::Incomplete("tencent: data 缺失（代码不存在？）".into()))?;
    let rows = data
        .values()
        .find_map(|value| value.get("m60").and_then(Value::as_array))
        .ok_or_else(|| {
            DataError::Incomplete("tencent: data.*.m60 缺失（该代码无 60min 数据）".into())
        })?;
    let mut bars = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let ctx = format!("tencent m60[{i}]");
        let fields = row
            .as_array()
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 期望数组，实际 {row}")))?;
        if fields.len() < 6 {
            return Err(DataError::Parse(format!(
                "{ctx}: 字段不足（期望 >= 6）：{row}"
            )));
        }
        let time = fields[0]
            .as_str()
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 时间字段不是字符串")))?;
        let date = crate::store::parse_date8(time)
            .ok_or_else(|| DataError::Parse(format!("{ctx}: 时间非法 {time:?}")))?;
        // 实测顺序：时间, open, close, high, low, volume, amount({}), ...
        let bar = Bar {
            date,
            open: json_number(&fields[1], &ctx)?,
            close: json_number(&fields[2], &ctx)?,
            high: json_number(&fields[3], &ctx)?,
            low: json_number(&fields[4], &ctx)?,
            volume: json_number(&fields[5], &ctx)? * 100.0,
            amount: fields
                .get(6)
                .map(|v| match v {
                    Value::Number(n) => n.as_f64().unwrap_or(0.0),
                    _ => 0.0, // 实测为 {}，即无成交额
                })
                .unwrap_or(0.0),
        };
        push_bar(&mut bars, bar)?;
    }
    sort_bars(&mut bars);
    ensure_not_empty("tencent", bars)
}

fn annotate(ctx: &str, err: DataError) -> DataError {
    match err {
        DataError::Parse(msg) => DataError::Parse(format!("{ctx}: {msg}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实响应片段（2026-11-17 抓取，截断为 2 根）。
    const EM_BODY: &str = r#"{"rc":0,"rt":17,"svr":181669690,"lt":1,"full":0,"dlmkts":"","dsc":"0","data":{"code":"600000","market":1,"name":"浦发银行","decimal":2,"dktotal":6429,"preKPrice":9.16,"klines":["2026-11-17 14:00,10.75,10.83,10.88,10.74,162232,175842288.00","2026-11-17 15:00,10.83,10.86,10.88,10.80,248833,269818731.00"]}}"#;

    /// 真实响应片段（新浪，截断为 2 根）。
    const SINA_BODY: &str = r#"[{"day":"2026-11-17 14:00:00","open":"10.750","high":"10.880","low":"10.740","close":"10.830","volume":"16223209","amount":"175842288.7109"},{"day":"2026-11-17 15:00:00","open":"10.820","high":"10.880","low":"10.800","close":"10.860","volume":"24883345","amount":"269818731.1423"}]"#;

    /// 真实响应片段（腾讯，截断为 2 根；amount 字段为 `{}`）。
    const TENCENT_BODY: &str = r#"{"code":0,"msg":"","data":{"sh600000":{"qt":{"sh600000":["1","浦发银行","600000"]},"m60":[["202611171400","10.75","10.83","10.88","10.74","162232.00",{},"4.87"],["202611171500","10.83","10.86","10.88","10.80","248833.00",{},"7.47"]],"prec":"10.78"}}}"#;

    #[test]
    fn parses_eastmoney_real_fragment() {
        let bars = parse_eastmoney(EM_BODY).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].date, 20261117);
        // 字段顺序 open, close, high, low：open=10.75 close=10.86
        assert_eq!(bars[0].open, 10.75);
        assert_eq!(bars[0].close, 10.83);
        assert_eq!(bars[0].high, 10.88);
        assert_eq!(bars[0].low, 10.74);
        // volume 手 -> 股
        assert_eq!(bars[0].volume, 162_232.0 * 100.0);
        assert_eq!(bars[0].amount, 175_842_288.0);
        // 日内顺序保持
        assert!(bars[0].date <= bars[1].date);
    }

    #[test]
    fn eastmoney_errors_are_classified() {
        // rc != 0
        let err = parse_eastmoney(r#"{"rc":102,"data":{}}"#).unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
        // data=null（代码不存在）
        let err = parse_eastmoney(r#"{"rc":0,"data":null}"#).unwrap_err();
        assert!(matches!(err, DataError::Incomplete(_)), "{err}");
        // 空 klines
        let err = parse_eastmoney(r#"{"rc":0,"data":{"klines":[]}}"#).unwrap_err();
        assert!(matches!(err, DataError::Incomplete(_)), "{err}");
        // 非法 JSON
        let err = parse_eastmoney("<html>blocked</html>").unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
        // 字段不足
        let err =
            parse_eastmoney(r#"{"rc":0,"data":{"klines":["2026-11-17 15:00,1,2"]}}"#).unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
    }

    #[test]
    fn skips_zero_price_bars_from_eastmoney() {
        let body = r#"{"rc":0,"data":{"klines":["2026-11-17 14:00,0,0,0,0,0,0","2026-11-17 15:00,10.83,10.86,10.88,10.80,248833,269818731.00"]}}"#;
        let bars = parse_eastmoney(body).unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].close, 10.86);
    }

    #[test]
    fn parses_sina_real_fragment() {
        let bars = parse_sina(SINA_BODY).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].date, 20261117);
        assert_eq!(bars[0].close, 10.83);
        assert_eq!(bars[0].volume, 16_223_209.0); // 股，不再换算
        assert_eq!(bars[0].amount, 175_842_288.710_9);
        assert_eq!(bars[1].close, 10.86);
    }

    #[test]
    fn sina_errors_are_classified() {
        let err = parse_sina("null").unwrap_err();
        assert!(matches!(err, DataError::Incomplete(_)), "{err}");
        let err = parse_sina("").unwrap_err();
        assert!(matches!(err, DataError::Incomplete(_)), "{err}");
        let err = parse_sina("<!doctype html>").unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
        // 缺 close 字段
        let err = parse_sina(
            r#"[{"day":"2026-11-17 15:00:00","open":"1","high":"1","low":"1","volume":"1"}]"#,
        )
        .unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
    }

    #[test]
    fn parses_tencent_real_fragment() {
        let bars = parse_tencent(TENCENT_BODY).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].date, 20261117);
        assert_eq!(bars[0].open, 10.75);
        assert_eq!(bars[0].close, 10.83);
        assert_eq!(bars[0].high, 10.88);
        assert_eq!(bars[0].low, 10.74);
        assert_eq!(bars[0].volume, 162_232.0 * 100.0); // 手 -> 股
        assert_eq!(bars[0].amount, 0.0); // 腾讯 m60 无成交额
    }

    #[test]
    fn tencent_errors_are_classified() {
        let err = parse_tencent(r#"{"code":-1,"msg":"bad params"}"#).unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
        let err = parse_tencent(r#"{"code":0,"data":{}}"#).unwrap_err();
        assert!(matches!(err, DataError::Incomplete(_)), "{err}");
        let err = parse_tencent(r#"{"code":0,"data":{"sh600000":{"m60":[]}}}"#).unwrap_err();
        assert!(matches!(err, DataError::Incomplete(_)), "{err}");
        let err = parse_tencent("not-json").unwrap_err();
        assert!(matches!(err, DataError::Parse(_)), "{err}");
    }

    #[test]
    fn builds_urls_for_sh_sz() {
        let url = eastmoney_url("600000", 500).unwrap();
        assert!(url.contains("secid=1.600000"), "{url}");
        assert!(url.contains("fqt=1"), "{url}");
        assert!(url.contains("klt=60"), "{url}");
        assert!(url.contains("lmt=500"), "{url}");
        let url = eastmoney_url("sz000001", 500).unwrap();
        assert!(url.contains("secid=0.000001"), "{url}");
        let url = sina_url("sh.600000", 1500).unwrap();
        assert!(url.contains("symbol=sh600000"), "{url}");
        assert!(url.contains("scale=60"), "{url}");
        assert!(url.contains("datalen=1500"), "{url}");
        let url = tencent_url("000001", 640).unwrap();
        assert!(url.contains("param=sz000001,m60,,640"), "{url}");
        assert!(eastmoney_url("abc", 10).is_err());
    }

    #[test]
    fn sources_have_metadata() {
        let sources = default_sources();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0].name(), "eastmoney");
        assert_eq!(sources[0].adjust(), Adjust::Qfq);
        assert_eq!(sources[1].adjust(), Adjust::None);
        assert_eq!(sources[2].approx_depth_bars(), 640);
        assert!(source_by_name("sina").is_some());
        assert!(source_by_name("unknown").is_none());
    }

    // —— 真实联网测试（默认 ignore；本机网络可用时手动跑一次）——
    //
    // `cargo test -p chanlun-data -- --ignored network`
    #[test]
    #[ignore = "联网测试：cargo test -p chanlun-data -- --ignored"]
    fn network_eastmoney_fetch() {
        let bars = EastmoneySource::default().fetch_60min("sh600000").unwrap();
        assert!(!bars.is_empty());
        assert!(bars.windows(2).all(|w| w[0].date <= w[1].date));
        assert!(bars.iter().all(|b| b.close > 0.0));
        eprintln!("东财 sh600000: {} 根，末根 {:?}", bars.len(), bars.last());
    }

    #[test]
    #[ignore = "联网测试：cargo test -p chanlun-data -- --ignored"]
    fn network_sina_fetch() {
        let bars = SinaSource::default().fetch_60min("sh600000").unwrap();
        assert!(
            bars.len() > 1000,
            "新浪应能回溯 >1000 根，实际 {}",
            bars.len()
        );
        assert!(bars.iter().all(|b| b.close > 0.0 && b.amount > 0.0));
        eprintln!("新浪 sh600000: {} 根，末根 {:?}", bars.len(), bars.last());
    }

    #[test]
    #[ignore = "联网测试：cargo test -p chanlun-data -- --ignored"]
    fn network_tencent_fetch() {
        let bars = TencentSource::default().fetch_60min("sh600000").unwrap();
        assert!(bars.len() > 500, "腾讯应返回约 640 根，实际 {}", bars.len());
        assert!(bars.iter().all(|b| b.close > 0.0 && b.amount == 0.0));
        eprintln!("腾讯 sh600000: {} 根，末根 {:?}", bars.len(), bars.last());
    }

    /// T-06 验收口径：同一标的、同一段窗口，三源收盘价误差 < 5 bps。
    ///
    /// 注意东财前复权、新浪/腾讯不复权，`validate::compare` 用锚定收盘比消除
    /// 常数复权因子后比较；重叠窗口内若发生分红会真报警（需换因子表）。
    #[test]
    #[ignore = "联网测试：cargo test -p chanlun-data -- --ignored"]
    fn network_cross_validate_three_sources() {
        let eastmoney = EastmoneySource::default().fetch_60min("sh600000").unwrap();
        let sina = SinaSource::default().fetch_60min("sh600000").unwrap();
        let tencent = TencentSource::default().fetch_60min("sh600000").unwrap();
        let tencent_recent = &tencent[tencent.len().saturating_sub(124)..];
        let summary = crate::validate::validate_sources(
            "600000",
            &[
                ("eastmoney", &eastmoney),
                ("sina", &sina),
                ("tencent", tencent_recent),
            ],
            5.0,
        );
        assert!(summary.ok, "三源交叉校验未通过：{:?}", summary.notes);
        // 注意：原始 bps 可能 > 5（1 分钱舍入在 10 元股价上是 ~10 bps），
        // `compare` 用"1 个最小价位"兜底；这里断言每份报告都通过。
        assert!(summary.reports.iter().all(|report| report.ok));
        eprintln!(
            "三源校验通过：max_diff={:.2}bps，报告={:?}",
            summary.max_close_diff_bps,
            summary
                .reports
                .iter()
                .map(|report| (
                    report.sources.join(" vs "),
                    report.close_ratio,
                    report.max_close_diff_bps,
                    report.common_bars
                ))
                .collect::<Vec<_>>()
        );
    }
}
