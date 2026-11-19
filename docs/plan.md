# 移植计划与状态（Rust 单二进制 MCP）

> 更新：2026-11-17 晚。**M0–M4 完成**（合成数据验收）；M5 待真实数据。

## 0. 里程碑

| 里程碑 | 内容 | 状态 | 验收 |
|---|---|---|---|
| M0 脚手架 | workspace / 类型 / 接口 / docs | ✅ | `cargo build` 通过 |
| M1 核心算法 | core 全量移植 | ✅ | G-1：95,342 项比对 0 差异 |
| M2 数据层 | bootstrap + 增量 + 校验 + health | ✅（真实旧仓待跑） | 合成仓 36 测试 + 4 个真实联网测试 |
| M3 回测引擎 | 撮合/滑点/闸门/统计 | ✅ | G-2 合成数据逐笔 0 差异、nav 1e-16 |
| M4 MCP 服务 | rmcp stdio + 9 工具 + 2 job 类型 | ✅ | `scripts/e2e_mcp.py` 通过 |
| M5 打包/真实数据 | release 单二进制 + 真实仓 G-2 | ⏳ 进行中 | 72 只真实票 legacy+dynpos G-2 通过（逐笔 0 差异、nav<1e-15）；全市场数据 72/5510（baostock 限流，守护续传中） |

## 1. 任务表（完成情况）

| ID | 任务 | 状态 | 验收证据 |
|---|---|---|---|
| T-01 | `find_bis` 移植 | ✅ | golden 分型/笔逐项一致 |
| T-02 | `find_zhongshus` 移植 | ✅ | golden 中枢 ZG/ZD 一致 |
| T-03 | `find_buy_signals`（B1/B2，adjacent+pivot） | ✅ | golden B1 36 / B2 13 条一致 |
| T-04 | `latest_sell_advice` | ✅ | golden 782 条 entry 用例一致 |
| T-05 | Store CSV/Parquet + bootstrap | ✅ CSV（Parquet 待办） | 36 测试 + 真实旧仓样例 |
| T-06 | 增量源 + 多源校验 + health | ✅ | 4 个联网测试：东财/新浪/腾讯 真实抓取 |
| T-07 | 引擎移植 | ✅ | 35 测试 + 合成 G-2 |
| T-08 | job 框架（Backtest/Scan/cancel） | ✅ | e2e 两个 job 均 done |
| T-09 | rmcp stdio server + 工具 | ✅ | e2e 9 工具 |
| T-10 | 集成测试 + 文档 + release | ⏳ | 真实数据 G-2 / release 打包 |
| G-1 | core golden diff | ✅ | `cargo test -p chanlun-core` |
| G-2 | engine golden diff | ✅ 合成 + ✅ 72 只真实票 / ⏳ 全市场 | 合成逐笔 0 差异；真实 72 只 legacy+dynpos 逐笔 0 差异、nav<1e-15（修复 float32 均值 parity bug） |

## 2. 已完成的关键口径决定

1. **卖出频率**：按 Python 代码（每交易日最后一根 bar 收盘判一次，`ExitCheckMode::DailyClose`）；文档的 bar 级口径留开关，未实现。
2. **包含合并方向**：按 Python 启发式 parity（K 线阴阳），并保留注释说明与文档 §4.1 不一致。
3. **复权**：东财增量用 `fqt=1`（fqt=2 实测为后复权），与 baostock 前复权历史对齐。
4. **数据历史**：必须 bootstrap（免费公开源只有 1–1.5 年），见 docs/data-sources.md。
5. **收益集中度**：`get_performance` 输出 top5pct/top1pct 利润占比（实测 74.31% / 27.04%）。

## 3. 参考材料

- 策略文档包：`缠论二买策略文档_20261117/`（自行准备，见 `CHANLUN_DOC_PKG`）
- 文档审阅问题：`docs/review-notes.md`
- 实测数据源：`docs/data-sources.md`
- 架构/工具契约：`docs/architecture.md`
