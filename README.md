# chanlun-mcp

A 股 60 分钟**缠论二买**策略的 MCP 服务（Rust 单二进制，stdio）。把 v1.0 策略文档的生产系统
（`chanlun.py` + `backtest_portfolio.py` + 数据/跟单链）重构成一个可供 Claude / DSH 直接调用的 MCP 服务。

> 状态（2026-11-17）：**核心/数据/引擎/MCP 四层已落地并通过测试**；历史数据需 bootstrap（见下）；
> 真实数据仓上的 G-2 逐笔对拍待跑。

## 能力（9 个工具）

| Tool | 类型 | 说明 |
|---|---|---|
| `data_health` | 同步 | 按 sh/sz 分组统计末行日期、落后数量、红线判据（文档 §3） |
| `get_performance` | 同步 | legacy/dynpos 绩效（读 `artifacts/stats.json`，带口径警告） |
| `get_plan` | 同步 | 跟单卡快照（持仓/明日卖/明日买，读 `artifacts/*snapshot.json`） |
| `analyze_stock` | 同步 | 单票最近笔/中枢/一买二买 + 距止损/MA13/ZD 距离 |
| `explain_signal` | 同步 | 二买证据链（一买低点、反弹/回调笔力度、中枢、MA13 乖离、amt20） |
| `scan_signals` | 异步 job | 全市场二买扫描（默认取最新可跟单信号日；可 `as_of` 指定） |
| `run_backtest` | 异步 job | 组合回测（legacy/dynpos），`get_job` 轮询 |
| `get_job` / `cancel_job` | 同步 | job 状态/结果/取消 |

每个响应都带 `caliber.warnings`：前复权不可与券商对账 / dynpos 容量不成立 / 未建模涨跌停排队。

## 快速开始

```bash
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo-home}"   # 可选：本机 cargo 镜像缓存
cargo build --release

# 1) 历史数据（二选一）
#   A. 从旧 Windows 仓导入（推荐，与 518.79 万基线同源）：
./target/release/chanlun-mcp bootstrap --from /path/to/stock/60min --to ~/.chanlun/data/60min
#   B. Mac 上一次性 baostock 重拉：python3 scripts/bootstrap_data.py --help
./target/release/chanlun-mcp doctor --json        # 红线检查

# 2) 绩效/跟单卡产物（可选，来自 v1.0 文档包）
scripts/import_artifacts.sh "/path/缠论二买策略文档_20261117" ~/.chanlun

# 3) 启动 MCP
./target/release/chanlun-mcp serve

# 4) 冒烟测试（无需真实行情；会生成 3 只合成票）
python3 scripts/e2e_mcp.py
```

MCP 配置（~/.claude.json 或 DSH mcpServers）：

```json
{
  "mcpServers": {
    "chanlun": {
      "type": "stdio",
      "command": "/ABSOLUTE/PATH/TO/chanlun-mcp/target/release/chanlun-mcp",
      "args": ["serve"],
      "env": { "CHANLUN_HOME": "/ABSOLUTE/PATH/TO/.chanlun" }
    }
  }
}
```

> 把 `/ABSOLUTE/PATH/TO` 换成你自己的路径；`CHANLUN_HOME` 省略时默认 `~/.chanlun`。

## 数据来源（实测结论）

免费公开源的 60min 历史只有 1–1.5 年（东财 ~124 根 / 新浪 ~1500 根 / 腾讯 ~640 根），
覆盖不到 2020；**2020-2026 回测必须 bootstrap**（旧 Windows 仓导入 或 baostock 一次性重拉）。
日常增量用东财 `fqt=1`（注意：`fqt=2` 实测是后复权）为主，新浪/腾讯多源交叉校验（容差 5bps）。
详见 [docs/data-sources.md](docs/data-sources.md)。

## 仓库结构

```text
crates/chanlun-core/    纯算法：MACD/合并K线/分型/笔/中枢/一买二买/卖点（已与 Python 逐信号对拍）
crates/chanlun-data/    CSV 仓（旧仓兼容）/bootstrap_import/东财+新浪+腾讯源/多源校验/health
crates/chanlun-engine/  组合回测：撮合/滑点/P1P2 闸门/仓位/统计（含收益集中度）
crates/chanlun-mcp/     rmcp 3.10 stdio server + 9 工具 + 异步 job 子进程
scripts/                golden_core.py / golden_engine.py / bootstrap_data.py / e2e_mcp.py / import_artifacts.sh
docs/                   architecture / data-sources / plan / review-notes
```

## 测试

```bash
cargo test --workspace          # 90 个测试全绿（4 个联网 ignored、1 个真实数据 G-2 ignored）
python3 scripts/e2e_mcp.py      # MCP 端到端：9 工具 + scan/backtest job
```

验证口径：
- **core**：合成数据 Python vs Rust，95,342 项比对 0 差异（`tests/golden_core.rs`）；
- **engine**：合成数据全链路 G-2，逐笔成交 0 差异、nav 相对偏差 ~1e-16（`tests/golden_engine.rs`）；
- **MCP**：`scripts/e2e_mcp.py` 跑通 initialize/tools/list/analyze/explain/scan/backtest/get_job。

## 下一步

1. 真实旧仓 bootstrap（5,235 只，约 3 分钟），抽查 50 只逐 bar 对拍；
2. 真实数据仓 G-2（`scripts/golden_engine.py` → `cargo test -- --ignored`）；
3. Parquet 序列（当前 CSV，5,235 只全量扫描/回测可换 arrow 提速）；
4. `Every60MinBar` 出场档位对照回测（文档说每 bar 判定，代码是每日收盘判定）。

## License

MIT (见 [LICENSE](LICENSE))。策略逻辑来自作者本人的 qmt-chanlun 生产系统；
文档/回测数字见 `docs/`。
