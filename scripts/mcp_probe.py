#!/usr/bin/env python3
"""MCP 冒烟测试：启动 chanlun-mcp serve，完成 initialize → tools/list → tools/call。

用法:
  python3 scripts/mcp_probe.py [--exe target/debug/chanlun-mcp] [--tool data_health]
"""
import argparse, json, os, subprocess, sys

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=os.path.join("target", "debug", "chanlun-mcp"))
    ap.add_argument("--tool", default="data_health")
    ap.add_argument("--args", default="{}")
    args = ap.parse_args()

    p = subprocess.Popen([args.exe, "serve"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         text=True, bufsize=1)
    def send(obj):
        p.stdin.write(json.dumps(obj, ensure_ascii=False) + "\n"); p.stdin.flush()
    def recv():
        line = p.stdout.readline()
        if not line:
            raise RuntimeError("server closed stdout; stderr=" + (p.stderr.read() or ""))
        return json.loads(line)

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "probe", "version": "0"}}})
    init = recv()
    assert "result" in init, init
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    tools = recv()["result"]["tools"]
    print("tools:", [t["name"] for t in tools])
    send({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
          "params": {"name": args.tool, "arguments": json.loads(args.args)}})
    out = recv()
    print(json.dumps(out, ensure_ascii=False, indent=2)[:4000])
    p.stdin.close()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    return 0 if out.get("result", {}).get("isError") is False else 1

if __name__ == "__main__":
    sys.exit(main())
