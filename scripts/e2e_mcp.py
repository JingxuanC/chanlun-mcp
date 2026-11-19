#!/usr/bin/env python3
"""chanlun-mcp 端到端冒烟测试（无需真实行情）。

流程：
1. 生成 3 只票 × 700 根合成 60min（确定性随机漫步），写入临时 CHANLUN_HOME/data/60min；
2. 启动 `chanlun-mcp serve`（stdio）；
3. initialize → tools/list → data_health → analyze_stock → explain_signal（无信号则跳过）
   → scan_signals(job) 轮询 → run_backtest(job) 轮询；
4. 断言 job 最终 done，打印摘要。

用法: python3 scripts/e2e_mcp.py [--exe target/debug/chanlun-mcp]
"""
import argparse, json, math, os, random, subprocess, sys, tempfile, time

def gen_repo(root: str) -> None:
    random.seed(42)
    codes = [("sh", "600000", 10.0), ("sz", "000001", 15.0), ("sz", "300750", 40.0)]
    os.makedirs(root, exist_ok=True)
    # 2024-01-02 起 700 个"交易日"（只造日期，不校验真实日历）
    import datetime
    d = datetime.date(2024, 1, 2)
    dates = []
    while len(dates) < 700:
        if d.weekday() < 5:
            dates.append(d.strftime("%Y%m%d"))
        d += datetime.timedelta(days=1)
    for ex, code, px in codes:
        px = px
        rows = []
        for i, ds in enumerate(dates):
            wave = 0.004 * math.sin(i / 23.0) + 0.002 * math.sin(i / 7.0)
            px *= (1.0 + wave + random.gauss(0, 0.006))
            o = px * (1 + random.gauss(0, 0.001))
            c = px
            h = max(o, c) * (1 + abs(random.gauss(0, 0.002)))
            l = min(o, c) * (1 - abs(random.gauss(0, 0.002)))
            vol = 200_000 + random.randint(0, 100_000)
            amt = vol * c
            rows.append([ds, f"{o:.3f}", f"{h:.3f}", f"{l:.3f}", f"{c:.3f}", str(vol), f"{amt:.1f}"])
        path = os.path.join(root, f"{ex}_{code}_60min.csv")
        with open(path, "w", newline="", encoding="utf-8-sig") as f:
            f.write("date,open,high,low,close,volume,amount\n")
            for r in rows:
                f.write(",".join(r) + "\n")
    print(f"[setup] 生成 {len(codes)} 只 × {len(dates)} 根 -> {root}")

class Mcp:
    def __init__(self, exe: str, env: dict):
        self.p = subprocess.Popen([exe, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, bufsize=1, env=env)
        self.msg_id = 0
        self.send("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "0"}})
        r = self.recv()
        assert "result" in r, r
        self.send_notify("notifications/initialized")

    def send(self, method, params):
        self.msg_id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.msg_id, "method": method,
                                       "params": params}, ensure_ascii=False) + "\n")
        self.p.stdin.flush()
        return self.msg_id

    def send_notify(self, method, params=None):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method,
                                       "params": params or {}}, ensure_ascii=False) + "\n")
        self.p.stdin.flush()

    def recv(self):
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("server stdout closed; stderr=" + (self.p.stderr.read() or ""))
        return json.loads(line)

    def call_tool(self, name, args):
        self.send("tools/call", {"name": name, "arguments": args})
        return self.recv()

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.p.kill()

def text_of(resp):
    return resp["result"]["content"][0]["text"]

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=os.path.join("target", "debug", "chanlun-mcp"))
    args = ap.parse_args()

    home = tempfile.mkdtemp(prefix="chanlun-e2e-")
    data_dir = os.path.join(home, "data", "60min")
    os.makedirs(os.path.join(home, "jobs"), exist_ok=True)
    gen_repo(data_dir)

    env = dict(os.environ, CHANLUN_HOME=home)
    mcp = Mcp(args.exe, env)
    ok = True
    try:
        mcp.send("tools/list", {})
        tools = [t["name"] for t in mcp.recv()["result"]["tools"]]
        print("[tools]", tools)

        health = json.loads(text_of(mcp.call_tool("data_health", {})))
        print("[data_health] ok=", health["report"]["ok"], "exchanges=",
              list(health["report"]["by_exchange"].keys()))
        assert health["report"]["total_symbols"] == 3, health

        an = json.loads(text_of(mcp.call_tool("analyze_stock", {"code": "600000"})))
        print("[analyze_stock] last_close=", an["last_close"], "bars=", an["bars"],
              "structure_ready=", an["structure"]["structure_ready"],
              "bis=", len(an["structure"].get("recent_bis", [])))
        assert an["structure"]["structure_ready"], an["structure"]
        assert an["bars"] == 700

        scan_args = {}
        try:
            ex = json.loads(text_of(mcp.call_tool("explain_signal", {"code": "600000"})))
            print("[explain_signal] date=", ex["signal"]["date"], "kind=", ex["signal"]["kind"],
                  "holds_b1_low=", ex["signal"]["pullback_holds_b1_low"])
            # 读到该票日期序列，取信号 bar 之后第 6 根 bar 作为数据截止日（覆盖 g+4 确认 + g+5 买入）
            dates = []
            with open(os.path.join(data_dir, "sh_600000_60min.csv"), encoding="utf-8-sig") as f:
                next(f)
                for line in f:
                    dates.append(int(line.split(",")[0]))
            sig_date = int(ex["signal"]["date"])
            idx = dates.index(sig_date) if sig_date in dates else 0
            scan_args = {"as_of": str(dates[min(idx + 6, len(dates) - 1)])}
        except AssertionError:
            print("[explain_signal] 合成数据无二买（可接受）")

        scan_job = json.loads(text_of(mcp.call_tool("scan_signals", scan_args)))["job_id"]
        bt_job = json.loads(text_of(mcp.call_tool("run_backtest",
                                                  {"funding_mode": "legacy"})))["job_id"]
        print("[jobs] scan=", scan_job, "backtest=", bt_job)

        deadline = time.time() + 180
        finished = {}
        while time.time() < deadline and len(finished) < 2:
            for jid in (scan_job, bt_job):
                if jid in finished:
                    continue
                r = json.loads(text_of(mcp.call_tool("get_job", {"job_id": jid})))
                st = r["status"]["state"]
                if st in ("done", "failed", "cancelled"):
                    finished[jid] = r
                    print(f"[job {jid}] state={st}",
                          "signals=" + str(r["result"].get("n_signals")) if r["result"] else
                          "error=" + str(r["status"].get("error"))[:200])
            time.sleep(0.5)
        assert scan_job in finished and bt_job in finished, "jobs 未在 180s 内结束"
        assert finished[scan_job]["status"]["state"] == "done", finished[scan_job]["status"]
        assert finished[bt_job]["status"]["state"] == "done", finished[bt_job]["status"]
        if scan_args:
            n_sig = finished[scan_job]["result"]["n_signals"]
            print("[scan] as_of=", scan_args["as_of"], "signals=", n_sig)
        stats = finished[bt_job]["result"]["stats"]
        print("[backtest] nav_end=", round(stats["nav_end"], 2), "trades=",
              finished[bt_job]["result"]["n_trades"], "max_dd=", stats["max_dd"])
        print("E2E OK")
    except Exception as e:
        ok = False
        print("E2E FAILED:", repr(e))
        raise
    finally:
        mcp.close()
    return 0 if ok else 1

if __name__ == "__main__":
    sys.exit(main())
