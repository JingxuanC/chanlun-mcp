#!/usr/bin/env bash
# 把 v1.0 文档包的复算产物导入 MCP 的 artifacts 目录，
# 让 get_performance / get_plan 立即可用（不依赖真实数据仓）。
#
# 用法: scripts/import_artifacts.sh <缠论二买策略文档_20261117 目录> [CHANLUN_HOME]
set -euo pipefail
DOC_DIR="${1:?用法: $0 <doc_pkg_dir> [CHANLUN_HOME]}"
HOME_DIR="${2:-${CHANLUN_HOME:-$HOME/.chanlun}}"
ART="$HOME_DIR/artifacts"
mkdir -p "$ART"
cp -v "$DOC_DIR/backtest/stats.json" "$ART/stats.json"
cp -v "$DOC_DIR/backtest/paper_60min_ma13_dynpos_result_snapshot.json" "$ART/" 2>/dev/null || \
  echo "[warn] 未找到 dynpos snapshot（get_plan 会返回缺产物错误）"
cp -v "$DOC_DIR/backtest/paper_60min_ma13_result_nav.csv" "$ART/" 2>/dev/null || true
cp -v "$DOC_DIR/backtest/paper_60min_ma13_dynpos_result_nav.csv" "$ART/" 2>/dev/null || true
cp -v "$DOC_DIR/charts/"*.png "$ART/" 2>/dev/null || true
echo "[ok] artifacts -> $ART"
ls -la "$ART"
