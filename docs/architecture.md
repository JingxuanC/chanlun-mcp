# chanlun-mcp 架构

> 目标：把「A 股 60min 缠论二买」策略封装成一个 **Rust 单二进制 MCP 服务**（stdio），
> 提供行情/信号/回测/跟单卡工具，并保留与 v1.0 文档回测（legacy 518.79 万）逐笔对拍的能力。

## 1. 为什么是 Rust 单二进制

- 无 Python 运行时依赖：QMT/PTrade 终端版已不需要（用户确认），交付面只剩 MCP。
- 全市场扫描是纯 CPU（5,235 只 × ~5,774 根 ≈ 3,000 万 bar），Rust + rayon 预计从 6–8 分钟降到 30–60 秒。
- 与你现有的 `causal-memory` MCP（Rust 二进制 stdio）同模式，`mcpServers` 里一行配置即可。

## 2. Crate 划分

```text
crates/
  chanlun-core/    纯算法：merge/fractal/bi/zhongshu/b1b2/sell（无 IO，可单测）
  chanlun-data/    数据仓（CSV/Parquet）+ 公开源适配 + 多源校验 + 健康检查
  chanlun-engine/  组合回测：撮合/滑点/闸门/仓位/统计
  chanlun-mcp/     bin：clap CLI + rmcp stdio server + job 框架 + tools/resources/prompts
```

依赖方向：`mcp -> engine -> data -> core`，`data -> core`。**core 不依赖任何 crate 外的运行时**。

## 3. 数据流

```text
历史:  bootstrap（baostock 一次性 / 旧 Windows CSV 导入） -> 数据仓（CSV→Parquet）
增量:  东财 60min（近 1 月，含额，fqt=1 前复权；fqt=2 是后复权）为主
       新浪 60min（~1.5 年，含额，不复权）/ 腾讯 m60（~5 月，无额）为校验源
       多源交叉校验（容差 5 bps） -> 写入数据仓
信号:  夜间跑批（或 MCP 触发）scan_signals -> 落盘 signals.json
服务:  MCP 同步工具读落盘产物；长任务（回测/全市场扫描）走 job 子进程
```

## 4. MCP 工具（当前 9 个）

| Tool | 类型 | 说明 |
|---|---|---|
| `get_plan(as_of?)` | 同步 | 跟单卡：持仓/明日卖/明日买/净值/预算 |
| `scan_signals(as_of?, limit?)` | **异步 job** | 全市场二买候选 + B/C 过滤 + 可成交性 + 建议金额；省略 as_of 时取全市场最新可跟单信号日 |
| `analyze_stock(code, as_of?)` | 同步 | 单票结构 + 离场条件距离 |
| `explain_signal(code, as_of?)` | 同步 | 二买证据链（一买低点/反弹笔/回调笔/中枢/MA13/amt20） |
| `get_performance(mode)` | 同步 | 绩效 + 分年 + 回撤 + **前 5% 交易利润占比** |
| `data_health()` | 同步 | 按 sh/sz 分组末行日期（文档 §3 红线） |
| `run_backtest(spec)` | **异步** | 返回 job_id；spec = BacktestConfig JSON |
| `get_job(job_id)` | 同步 | 状态/进度/结果摘要；支持 cancel |
| `refresh_data(days?)` | 写（默认关闭） | 增量更新 + 校验 |
| `cancel_job(job_id)` | 同步 | 取消 queued/running job（kill 子进程） |
| `audit_trades(mode)` | 异步 | 逐笔审计（买价=bar 开盘、T+1、并发/单日上限）——待接 |

Resources / Prompts（已登记 URI/名称，读取实现待补）：`chanlun://strategy/*`、`premarket_briefing` 等。

实现：`rmcp 3.10`（官方 Rust SDK），stdio 传输；serverInfo = `chanlun-mcp 0.1.0`；
产物目录 `$CHANLUN_HOME/artifacts`（stats.json / snapshot json，由 `scripts/import_artifacts.sh` 导入）。

## 5. 异步 job 协议（关键）

MCP 客户端超时 ~60s，全市场回测分钟级，所以：

```text
run_backtest(spec) -> {"job_id": "bt-20261117-2130-xxxx"}
get_job(job_id)    -> {"state": "running", "progress": "scan 2100/5235"}
                   -> {"state": "done", "result": {...stats...}, "artifacts": {...}}
```

实现：`serve` 用 `current_exe worker --job_id --spec --jobs-dir` 拉子进程，写
`jobs/<id>/{spec,status,result}.json + log.txt`；主进程只负责状态机/并发上限/取消（kill pid）。

## 6. 口径防误用（比性能重要）

每个工具响应都带 `Caliber { as_of, funding_mode, adjust, with_execution_gates, warnings }`，
warnings 固定包含：前复权不可对账 / dynpos 容量不成立 / 未建模涨跌停排队。
tool description 里也要重复一遍，防止模型把回测曲线当实盘业绩。

## 7. 对拍（golden）策略

- **core**：用合成 OHLC 序列同时跑 Python `chanlun.py` 和 Rust core，比对
  合并K线/分型/笔/中枢/一买二买逐项一致（浮点容差 0，比较 `<`/`>` 前先量化到 1e-9）。
- **engine**：用同一份 60min 数据仓，Python `backtest_portfolio.py` 与 Rust engine 各跑一遍，
  要求**逐笔成交一致**（code/date/price/qty/reason），净值最大偏差 < 1e-6。
- **数据**：`bootstrap` 后与 Windows 原数据抽查 50 只，前复权收盘逐 bar 一致。

## 8. 配置（~/.claude.json / DSH mcpServers）

```json
{
  "mcpServers": {
    "chanlun": {
      "type": "stdio",
      "command": "/ABSOLUTE/PATH/TO/chanlun-mcp/target/release/chanlun-mcp",
      "args": ["serve"],
      "env": {
        "CHANLUN_HOME": "/ABSOLUTE/PATH/TO/.chanlun"
      }
    }
  }
}
```

数据仓默认 `$CHANLUN_HOME/data/60min`，job 默认 `$CHANLUN_HOME/jobs`。
