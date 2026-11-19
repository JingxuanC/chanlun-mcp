# 公开数据源实测（2026-11-17，本机 macOS）

目标：给 60min 策略找**可公开访问、无需 key、能覆盖 2020-01-02 起**的数据源。
结论先说：**免费公开源的 60min 历史只有 1–1.5 年，覆盖不到 2020；历史必须 bootstrap。**

## 实测结果

| 源 | 端点 | 60min 可回溯 | 成交额 | 复权 | 备注 |
|---|---|---|---|---|---|
| 东方财富 | `push2his.eastmoney.com/api/qt/stock/kline/get?klt=60&fqt=1` | **124 根**（2026-09-29 ~ 2026-11-17） | ✅ f57 | ✅ 前复权（fqt=1；**fqt=2 是后复权**） | `beg=20240101` 返回空，说明服务端只留最近约 1 个月 |
| 新浪 | `quotes.sina.cn/cn/api/json_v2.php/CN_MarketDataService.getKLineData?scale=60&datalen=1500` | **1500 根**（2025-05-06 ~ 2026-11-17） | ✅ amount | ❌ 不复权 | `datalen=2000` 返回 null（上限 ~1500） |
| 腾讯 | `ifzq.gtimg.cn/appstock/app/kline/mkline?param=sh600000,m60,,640` | **640 根**（2026-03-24 ~ 2026-11-17） | ❌（字段为 `{}`） | ❌ 不复权 | `fqkline` 不支持 m60（bad params） |
| 雪球 | `stock.xueqiu.com/v5/stock/chart/kline.json?period=60m` | 需登录 token | — | — | 无 cookie 返回 400016 |
| baostock | Python SDK，`frequency='60', adjustflag='2'` | **2020-01-02 起**（~5,774 根/只） | ✅ | ✅ 前复权 | 官方仅 Python SDK；原策略即用此源 |
| TDX（mootdx） | 通达信协议 | 5min 约 2 年 → 重采样 60min 约 2 年 | ✅ | ❌ | 也不够 7 年；Rust 需自行实现协议 |

（数据实测命令见 `docs/` 历史，均为 `curl` 直连公共接口，未使用 akshare。）

## 推荐方案：bootstrap + 增量 + 多源校验

1. **历史 bootstrap（一次性）**——二选一：
   - **A. 从你 Windows 那台导入**：`<生产机数据目录>\stock\60min\*.csv`（3.13GB）→ `chanlun-mcp bootstrap --from <目录>` → 转 Parquet。**与 518.79 万基线完全同源，可直接对拍**，最推荐。
   - **B. Mac 上用 baostock 重拉**：一次性 Python 脚本（非运行时依赖），5,235 只 × 7 年，约 3–6 小时。口径与原文一致，但需要网络与 baostock 稳定性。
2. **每日增量**：东财 60min 为主（近 1 个月足够补 4 根/日），新浪为备源（1.5 年窗口），腾讯为第三源（无成交额，仅校验价格）。
3. **多源校验**：同一 bar 收盘价容差 5 bps；不一致写入 `validation_report` 并告警。这个思路直接沿用你 `astock-data-mcp` 的多源直连 + 回退 + 缓存模式。
4. **复权口径**：历史用前复权序列（与 baostock 一致）；增量源若为不复权（新浪/腾讯），写入前用日线复权因子换算；东财 fqt=1 是前复权（fqt=2 实测为后复权，会把 600000 末根收盘放大到 ~110），需与历史序列做一次"同日期收盘比"校验。

## 对回测的影响

- 只有 A/B 两条路能复现 2020-2026 的 7 年回测；
- 若只接公开源（1.5 年），MCP 仍可提供**实时信号 + 最近 1 年回测**，但会失去与 518.79 万基线的对拍能力；
- **不建议**用多个短历史源拼接 2020 年以前的数据——口径/复权/bar 对齐差异会污染结构计算。

---

## 附：2026-11-17 晚 Rust 侧复测补充（data-porter）

同日用 `crates/chanlun-data` 的 ureq 实现直连复测（`cargo test -p chanlun-data -- --ignored` 可复现），
对上面表格的**两点修正/补充**：

1. **东财复权参数写反了**：`fqt=2` 实测是**后复权**（600000 末根收盘 110.94，真实价 10.86），
   `fqt=0/1` 才是原始/前复权（末根都是 10.86）。Rust 实现用 `fqt=1`（`EASTMONEY_FQT`），
   才能与 baostock 旧仓（`adjustflag=2` 前复权）拼接。
2. **成交量单位**：东财/腾讯 kline 的 volume 是**手**，新浪是**股**；本仓统一存**股**
   （`sources` 层对东财/腾讯 ×100）。`amount` 东财/新浪为元，腾讯 m60 该字段为 `{}`（无额）。
   三源 bar 时间标签都是 **bar 结束时间**（10:30 / 11:30 / 14:00 / 15:00）。

三源一致性实测（sh600000，2026-11-17，重叠 124 根）：

- 东财 124 根 / 新浪 1500 根 / 腾讯 640 根，末根收盘三源都是 10.86（锚定比 1.0）；
- **东财与腾讯 124 根逐根 0 差异**；**新浪有 9 根差 1 分钱**（10 元股价上 = 10.4 bps），
  因此多源校验判据是 `max(容差 5 bps, 1 个最小价位 0.01 元)`，否则天天误报
  （实现见 `crates/chanlun-data/src/validate.rs`）。
- 增量合并约定：仓内 CSV 只存 `date`（YYYYMMDD）不存时刻，**同一天 4 根靠文件行序**；
  增量按 `(date, 日内序号)` 覆盖，且抓取窗口首日 bar 数少于仓内当天时整段保留旧数据（防错位）。

---

## 附：GitHub / HuggingFace 公开数据集（2026-11-17 实测）

> 按用户要求额外搜了 GitHub / HuggingFace。**结论：没有找到能覆盖"全市场 × 2020-2026"的分钟数据 dump**，
> 但找到一个不错的 1 分钟长历史数据集（仅深市、到 2022），可作交叉验证 / 2005-2019 长历史研究用。

### 1) `jobs-git/StockChina-Minute`（HuggingFace，MIT）
- 原始 CSV，**1 分钟线**，README 声称 2005–2023 / 5,267 只；
- **实测文件清单只有 1,450 个 `.XSHE`（深市 000/001/002，无沪市/创业板/科创板）**；
- 单文件 ~108 MB（000001.XSHE.csv），全量 ~121 GB；字段：
  `open,close,high,low,volume,money,avg,high_limit,low_limit,pre_close,paused,factor`；
- 实测 000001.XSHE 首行 2005-01-04 09:31、末行 **2022-12-29 15:00**（不是 2023）。
- 用途：① 独立数据源交叉验证；② 2005-2019 长历史稳健性（深市子集）；③ 1min→60min 重采样对照。

### 2) `q1232990/StockChina-Minute-Parquet`（HuggingFace，CC-BY-4.0）
- 上面数据集的 Parquet 优化版（zstd），**1,450 个深市文件、约 9.9 GB**；
- 列：`datetime, open/close/high/low, volume, money, avg, high_limit, low_limit, pre_close, paused, factor`；
- 同样只到 2022-12，仅深市。

### 3) 其他查到的（不适用）
- `yifishbossman/financial-analyst-data-*`：qlib 格式，5min 只有 **滚动 ~7 天**（TDX 免费源），不能做历史；
- `hello245m/free-stockdb`（GitHub 3.2k stars）：本地数据引擎，分钟数据来自其自有同步服务（第三方、数据源不可控），
  不是公开数据 dump，未采用；
- `waizao/StockApiJava` 等：付费数据 API 的客户端，无公测数据。

### 4) 与 baostock 的实测对比（本机 macOS）

| 源 | 60min/1min 起点 | 覆盖 | 复权/成交额 | 可用性 |
|---|---|---|---|---|
| baostock | **2020-01-02**（60min 与 5min 相同） | 全 A 股（实测 5,510 只） | qfq + amount | ✅ 本机 login 1.6s，可拉 |
| HF jobs-git 1min | 2005-01-04 | **仅 1,450 只深市** | factor + money | ✅ 可下载（121GB 全量 / 9.9GB parquet） |
| 东财/新浪/腾讯 60min | ~124/1500/640 根 | 全市场 | 见上表 | ✅ 仅增量 |

**决策**：生产仓用 baostock 一次性 bootstrap（与原文档同源、可复现 518.79 万基线）；
HF 数据只作为长历史/交叉验证的可选补充，不进生产口径。
