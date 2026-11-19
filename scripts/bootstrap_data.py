#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""bootstrap_data.py —— 历史 60min 数据 bootstrap 的 B 方案（baostock 一次性重拉）。

**不是运行时依赖**：只在没有旧 Windows 数据仓可导入时，一次性把 2020-01-02 起的
60min 历史拉到本地仓；日常增量由 Rust 侧公开源（东财/新浪/腾讯）完成。

跟 A 方案（`bootstrap_import` 导入旧仓）产出同一种规范 CSV：

    date,open,high,low,close,volume,amount

- `date`       = YYYYMMDD（同一天 4 根 bar 日期相同，行序即时间序）
- `volume`     = 股（baostock 原生单位）
- `amount`     = 元
- 价格         = **前复权**（baostock `adjustflag="2"`），与旧仓/东财 fqt=1 口径一致

用法（示例）：

    # 首次全市场（约 5,235 只 × 5,774 根，3–6 小时；可中断，重跑自动续传）
    python3 bootstrap_data.py --out ~/.chanlun/data/60min

    # 先拉 50 只试水
    python3 bootstrap_data.py --out /tmp/chanlun-data --limit 50

    # 指定股票（代码不带市场前缀）
    python3 bootstrap_data.py --out /tmp/chanlun-data --codes 600000,000001

    # 离线自测（不联网、不需要 baostock，验证解析/合并/写盘逻辑）
    python3 bootstrap_data.py --self-test

依赖：`pip install baostock`（仅此脚本需要；Rust 运行时零 Python 依赖）。
"""

from __future__ import annotations

import argparse
import csv
import os
import sys
import tempfile
import time
import traceback
from typing import Dict, Iterable, List, Optional, Tuple

HEADER = ["date", "open", "high", "low", "close", "volume", "amount"]
FIELDS = "date,open,high,low,close,volume,amount"
SERIES_SUFFIX = "60min"
DEFAULT_START = "2020-01-02"


def log(msg: str) -> None:
    print(time.strftime("%Y-%m-%d %H:%M:%S") + "  " + msg, file=sys.stderr, flush=True)


# ---------------------------------------------------------------------------
# 纯逻辑：文件名 / 代码 / CSV 读写 / 增量合并（可离线自测）
# ---------------------------------------------------------------------------
def normalize_code(raw: str) -> Optional[Tuple[str, str]]:
    """`sh.600000` / `sh600000` / `600000` -> ("sh", "600000")；非法返回 None。"""
    text = raw.strip().lower().replace(".", "").replace("_", "")
    exchange = ""
    for prefix in ("sh", "sz", "bj"):
        if text.startswith(prefix):
            exchange, text = prefix, text[len(prefix) :]
            break
    digits = "".join(ch for ch in text if ch.isdigit())
    if len(digits) < 6:
        return None
    code = digits[-6:]
    if not exchange:
        exchange = "sh" if code[0] in "69" else ("bj" if code[0] in "48" else "sz")
    return exchange, code


def series_path(out_dir: str, exchange: str, code: str) -> str:
    return os.path.join(out_dir, f"{exchange}_{code}_{SERIES_SUFFIX}.csv")


def date8(raw: str) -> Optional[str]:
    """`2020-01-02` / `20200102` / `2020/01/02` -> `20200102`。"""
    digits = "".join(ch for ch in raw if ch.isdigit())
    if len(digits) < 8:
        return None
    return digits[:8]


def parse_rows(rows: Iterable[Iterable[str]]) -> List[List[str]]:
    """把 baostock 原始行规整成 [date8, open, high, low, close, volume, amount]。

    - 丢弃空行/停牌行（OHLC 非正）；
    - 同一天多根 bar 保持返回顺序；
    - 保留原始数值字符串（避免浮点漂移）。
    """
    out: List[List[str]] = []
    for row in rows:
        if not row or len(row) < 6:
            continue
        d = date8(str(row[0]))
        if d is None:
            continue
        vals = [str(x).strip() for x in row[1:8]]
        while len(vals) < 6:
            vals.append("0")
        try:
            o, h, l, c = (float(vals[0]), float(vals[1]), float(vals[2]), float(vals[3]))
        except ValueError:
            continue
        if o <= 0 or h <= 0 or l <= 0 or c <= 0:
            continue  # 停牌 / 未成交
        out.append([d] + vals[:6])
    return out


def read_existing(path: str) -> List[List[str]]:
    """读规范 CSV，返回 [date, open, ..., amount] 行（忽略坏行）。"""
    if not os.path.isfile(path):
        return []
    rows: List[List[str]] = []
    with open(path, "r", encoding="utf-8-sig", newline="") as f:
        reader = csv.reader(f)
        for i, row in enumerate(reader):
            if i == 0 or not row:
                continue
            d = date8(row[0])
            if d is None:
                continue
            rows.append([d] + row[1:7])
    return rows


def last_date_of(path: str) -> Optional[str]:
    """只读尾部取末行日期（与 Rust `store::last_bar_date` 同一思路）。"""
    if not os.path.isfile(path):
        return None
    with open(path, "rb") as f:
        f.seek(0, os.SEEK_END)
        size = f.tell()
        if size == 0:
            return None
        window = min(size, 8192)
        f.seek(size - window)
        text = f.read().decode("utf-8-sig", errors="replace")
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    for line in reversed(lines):
        d = date8(line.split(",")[0])
        if d:
            return d
    return None


def merge_rows(existing: List[List[str]], fetched: List[List[str]]) -> Tuple[List[List[str]], int, int]:
    """按 `(date, 日内序号)` 覆盖合并（与 Rust `Store::merge_incremental` 等价）。

    返回 (merged_rows, added, replaced)。抓取到的 bar 覆盖同身份旧 bar。

    第一天完整性保护：若抓取到的第一天 bar 数少于已有文件当天的 bar 数
    （源窗口从半天开始），整段保留旧文件当天、从次日起合并，
    避免把旧 bar 错位覆盖（与 Rust 侧 `kept_first_day` 行为一致）。
    """
    def keyed(rows: List[List[str]]) -> Dict[Tuple[str, int], List[str]]:
        seen: Dict[Tuple[str, int], List[str]] = {}
        ordinals: Dict[str, int] = {}
        for row in rows:
            d = row[0]
            ordinal = ordinals.get(d, 0)
            ordinals[d] = ordinal + 1
            seen[(d, ordinal)] = row
        return seen

    merged = keyed(existing)
    fetched_keyed = keyed(fetched)
    if fetched:
        first_date = fetched[0][0]
        existing_first = sum(1 for row in existing if row[0] == first_date)
        fetched_first = sum(1 for row in fetched if row[0] == first_date)
        if existing_first > fetched_first:
            fetched_keyed = {
                key: row for key, row in fetched_keyed.items() if key[0] != first_date
            }

    added = replaced = 0
    for key, row in fetched_keyed.items():
        old = merged.get(key)
        if old is None:
            added += 1
        elif old != row:
            replaced += 1
        merged[key] = row
    ordered = [merged[key] for key in sorted(merged, key=lambda k: (k[0], k[1]))]
    return ordered, added, replaced


def write_rows(path: str, rows: List[List[str]]) -> None:
    """原子写规范 CSV（先写 .tmp 再 rename）。"""
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(HEADER)
        for row in rows:
            writer.writerow(row[:7])
    os.replace(tmp, path)


# ---------------------------------------------------------------------------
# baostock 拉取
# ---------------------------------------------------------------------------
class BaostockPuller:
    def __init__(self, retries: int = 4) -> None:
        import baostock as bs  # 延迟 import：--self-test / --help 不需要

        self.bs = bs
        self.retries = retries
        self.logged_in = False

    def login(self) -> bool:
        if self.logged_in:
            return True
        result = self.bs.login()
        if result.error_code != "0":
            log(f"[baostock] 登录失败: {result.error_msg}")
            return False
        self.logged_in = True
        return True

    def logout(self) -> None:
        if self.logged_in:
            try:
                self.bs.logout()
            except Exception:
                pass
            self.logged_in = False

    def relogin(self, tag: str = "") -> bool:
        self.logout()
        time.sleep(1.0)
        ok = self.login()
        if not ok:
            log(f"[baostock] 重连失败{tag}")
        return ok

    def query(self, code: str, start: str, end: str) -> Optional[List[List[str]]]:
        """单只查询，指数退避 + 重连；返回 None 表示重试后仍失败。"""
        for attempt in range(self.retries):
            try:
                rs = self.bs.query_history_k_data_plus(
                    code,
                    FIELDS,
                    start_date=start,
                    end_date=end,
                    frequency="60",
                    adjustflag="2",  # 2 = 前复权（与旧仓一致）
                )
                if rs.error_code != "0":
                    raise RuntimeError(f"{rs.error_code} {rs.error_msg}")
                rows: List[List[str]] = []
                while rs.next():
                    rows.append(rs.get_row_data())
                return rows
            except Exception as exc:  # noqa: BLE001 - 需要吞掉 SDK 各类异常后重试
                wait = min(30, 2**attempt)
                log(f"[baostock] {code} 查询失败({attempt + 1}/{self.retries}): {exc}；{wait}s 后重试")
                time.sleep(wait)
                self.relogin(f"({code})")
        return None

    def all_codes(self, day: str) -> List[str]:
        """全市场 A 股代码（过滤指数/基金/债券；北交所 baostock 覆盖不全，跳过）。"""
        rs = self.bs.query_all_stock(day=day)
        codes: List[str] = []
        while rs.error_code == "0" and rs.next():
            row = rs.get_row_data()
            if not row:
                continue
            code = row[0]  # sh.600000 / sz.000001 / sh.000001(指数)
            exchange, digits = code.split(".")[0], code.split(".")[-1]
            if exchange == "sh" and digits.startswith("6"):
                codes.append(code)  # 沪市主板 + 科创板
            elif exchange == "sz" and digits[:1] in ("0", "3"):
                codes.append(code)  # 深市主板 + 创业板
        return sorted(set(codes))


def run_bootstrap(args: argparse.Namespace) -> int:
    os.makedirs(args.out, exist_ok=True)
    failures_path = os.path.join(args.out, "bootstrap_failures.txt")
    puller = BaostockPuller(retries=args.retries)
    if not puller.login():
        return 2
    started = time.time()
    failures: List[str] = []
    stats = {"imported": 0, "skipped": 0, "failed": 0, "bars": 0}
    try:
        if args.codes:
            codes = []
            for raw in args.codes.split(","):
                normalized = normalize_code(raw)
                if normalized:
                    codes.append(f"{normalized[0]}.{normalized[1]}")
                else:
                    log(f"[args] 忽略非法代码: {raw}")
        else:
            log("[plan] 拉取全市场股票列表…")
            codes = puller.all_codes(args.end)
        if args.limit > 0:
            codes = codes[: args.limit]
        log(f"[plan] 待处理 {len(codes)} 只，区间 {args.start} ~ {args.end}，输出 {args.out}")

        for i, full_code in enumerate(codes, 1):
            normalized = normalize_code(full_code)
            if normalized is None:
                continue
            exchange, code = normalized
            target = series_path(args.out, exchange, code)
            start = args.start
            last = last_date_of(target)
            if last is not None:
                if last >= date8(args.end):
                    stats["skipped"] += 1
                    continue  # 已到指定截止日
                # 从末行日期重查（baostock start 含端点），靠合并去重覆盖
                start = f"{last[:4]}-{last[4:6]}-{last[6:8]}"
                if args.resume_start and last >= date8(args.resume_start):
                    pass

            rows = puller.query(full_code, start, args.end)
            if rows is None:
                failures.append(full_code)
                stats["failed"] += 1
                if stats["failed"] >= args.max_consecutive_failures:
                    log("[baostock] 连续失败过多，疑似限流，中止（已拉取部分保留，重跑续传）")
                    break
                continue

            fetched = parse_rows(rows)
            if not fetched:
                stats["skipped"] += 1
                continue
            existing = read_existing(target)
            merged, added, replaced = merge_rows(existing, fetched)
            if added or replaced:
                write_rows(target, merged)
            stats["imported"] += 1
            stats["bars"] = stats["bars"] + len(fetched)
            if i % 50 == 0 or i == len(codes):
                elapsed = time.time() - started
                log(
                    f"[progress] {i}/{len(codes)}  导入 {stats['imported']}  跳过 {stats['skipped']}  "
                    f"失败 {stats['failed']}  末根 {merged[-1][0] if merged else '-'}  "
                    f"已用 {elapsed / 60:.1f}min"
                )
            if args.sleep > 0:
                time.sleep(args.sleep)
    finally:
        puller.logout()

    if failures:
        with open(failures_path, "w", encoding="utf-8") as f:
            f.write("\n".join(failures) + "\n")
        log(f"[done] 失败 {len(failures)} 只，清单: {failures_path}")
    log(
        f"[done] 完成：导入 {stats['imported']}，跳过 {stats['skipped']}，失败 {stats['failed']}，"
        f"抓取 {stats['bars']} 根，用时 {(time.time() - started) / 60:.1f}min"
    )
    return 0 if stats["failed"] == 0 else 1


# ---------------------------------------------------------------------------
# 离线自测
# ---------------------------------------------------------------------------
def self_test() -> int:
    errors = 0

    def check(name: str, got, want) -> None:
        nonlocal errors
        if got != want:
            errors += 1
            print(f"FAIL {name}: got={got!r} want={want!r}")

    check("normalize sh", normalize_code("sh.600000"), ("sh", "600000"))
    check("normalize sz", normalize_code("sz000001"), ("sz", "000001"))
    check("normalize bare sh", normalize_code("600000"), ("sh", "600000"))
    check("normalize bare sz", normalize_code("000001"), ("sz", "000001"))
    check("normalize bj", normalize_code("830799"), ("bj", "830799"))
    check("normalize bad", normalize_code("abc"), None)
    check("date8", date8("2020-01-02"), "20200102")

    raw = [
        ["2020-01-02", "9.0", "9.5", "8.9", "9.2", "1234567", "12345678.90"],
        ["2020-01-02", "9.2", "9.6", "9.1", "9.4", "1234567", "12345678.90"],
        ["2020-01-03", "0", "0", "0", "0", "0", "0"],  # 停牌，丢弃
        ["bad", "1", "1", "1", "1", "1", "1"],
    ]
    parsed = parse_rows(raw)
    check("parse_rows len", len(parsed), 2)
    check("parse_rows date", parsed[0][0], "20200102")
    check("parse_rows close", parsed[1][4], "9.4")

    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "sh_600000_60min.csv")
        fetched = [["20200102", "9.0", "9.5", "8.9", "9.2", "100", "1000"],
                   ["20200102", "9.2", "9.6", "9.1", "9.4", "100", "1000"],
                   ["20200103", "9.4", "9.7", "9.3", "9.5", "100", "1000"],
                   ["20200103", "9.5", "9.8", "9.4", "9.6", "100", "1000"]]
        merged, added, replaced = merge_rows([], fetched)
        check("merge fresh added", added, 4)
        write_rows(path, merged)
        check("last_date_of", last_date_of(path), "20200103")

        # 重抓同一天两根（收盘变 0.1）+ 新增一天：覆盖 2 根，新增 2 根
        again = [["20200103", "9.5", "9.8", "9.4", "9.6", "100", "1000"],
                 ["20200103", "9.6", "9.9", "9.5", "9.7", "100", "1000"],
                 ["20200106", "9.7", "10.0", "9.6", "9.8", "100", "1000"],
                 ["20200106", "9.8", "10.1", "9.7", "9.9", "100", "1000"]]
        merged2, added2, replaced2 = merge_rows(read_existing(path), again)
        check("merge added2", added2, 2)
        check("merge replaced2", replaced2, 2)
        check("merge total", len(merged2), 6)
        check("merge last", merged2[-1][0], "20200106")
        write_rows(path, merged2)
        check("reread rows", len(read_existing(path)), 6)
        check("reread last", last_date_of(path), "20200106")
        check("no tmp left", os.path.exists(path + ".tmp"), False)

    if errors == 0:
        print("self-test OK（解析/合并/写盘/末行日期 全部通过，未联网）")
        return 0
    print(f"self-test FAILED：{errors} 个断言不通过")
    return 1


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="60min 历史 bootstrap（B 方案：baostock 一次性重拉，输出与 Rust 仓同格式）"
    )
    parser.add_argument("--out", help="输出数据仓目录（例：~/.chanlun/data/60min）")
    parser.add_argument("--start", default=DEFAULT_START, help=f"起始日（默认 {DEFAULT_START}）")
    parser.add_argument("--end", default=time.strftime("%Y-%m-%d"), help="截止日（默认今天）")
    parser.add_argument("--codes", default="", help="只拉指定代码（逗号分隔，可带/不带市场前缀）")
    parser.add_argument("--limit", type=int, default=0, help="只处理前 N 只（试水用）")
    parser.add_argument("--sleep", type=float, default=0.02, help="每只之间限速秒数（默认 0.02）")
    parser.add_argument("--retries", type=int, default=4, help="单只重试次数（默认 4）")
    parser.add_argument("--resume-start", default="", help="保留参数：续传起始日（默认按末行日期）")
    parser.add_argument(
        "--max-consecutive-failures", type=int, default=8, help="连续失败熔断阈值（默认 8）"
    )
    parser.add_argument("--self-test", action="store_true", help="离线自测（不联网/不需要 baostock）")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.out:
        parser.error("--out 必填（或使用 --self-test）")
    args.out = os.path.expanduser(args.out)
    try:
        return run_bootstrap(args)
    except KeyboardInterrupt:
        log("用户中断；已写入的文件保留，重跑可续传")
        return 130
    except Exception:  # noqa: BLE001 - CLI 顶层兜底，打印堆栈便于排查
        traceback.print_exc()
        return 1


if __name__ == "__main__":
    sys.exit(main())
