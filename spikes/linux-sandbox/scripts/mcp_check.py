#!/usr/bin/env python3
"""Drives the stdio MCP server through a sandbox launcher and asserts the handshake.

Usage: mcp_check.py <launcher> <args...>   (the server is started as `<launcher> python3 mcp_echo.py`)
Env: WS = workspace, OUTSIDE = the escape target directory, expect the sandbox to apply.
Prints MCP_OK / MCP_FAIL and exits accordingly.
"""
import json
import os
import subprocess
import sys

launcher = [sys.argv[1]]
server = sys.argv[2]
cmd = launcher + ["python3", server]
# The server starts in the workspace, so relative paths in its tools resolve there.
proc = subprocess.Popen(
    cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=os.environ["WS"]
)


def rpc(method, params=None, rid=None):
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if rid is not None:
        msg["id"] = rid
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    if rid is None:
        return None
    line = proc.stdout.readline()
    if not line:
        raise SystemExit("MCP_FAIL: server closed stdout")
    return json.loads(line)


def main():
    init = rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "spike", "version": "1"}}, 1)
    assert init["result"]["serverInfo"]["name"] == "mcp-echo", init
    rpc("notifications/initialized")
    tools = rpc("tools/list", {}, 2)
    names = sorted(t["name"] for t in tools["result"]["tools"])
    assert names == ["echo", "write_file"], names
    echo = rpc("tools/call", {"name": "echo", "arguments": {"text": "hello-mcp"}}, 3)
    assert echo["result"]["content"][0]["text"] == "hello-mcp", echo
    inside = rpc("tools/call", {"name": "write_file", "arguments": {"path": "mcp_inside.txt", "content": "from-mcp"}}, 4)
    assert inside["result"]["isError"] is False, inside
    outside = rpc(
        "tools/call",
        {"name": "write_file", "arguments": {"path": os.path.join(os.environ["OUTSIDE"], "mcp_outside.txt"), "content": "x"}},
        5,
    )
    escaped = outside["result"]["isError"] is False
    print(f"MCP_OK echo={echo['result']['content'][0]['text']} inside_written={inside['result']['isError'] is False} outside_write_succeeded={escaped}")
    proc.stdin.close()
    proc.wait(timeout=10)


if __name__ == "__main__":
    main()
