#!/usr/bin/env python3
"""A minimal stdio MCP server, equivalent in spirit to the repository's mcp-fixture.

The repository fixture is a Mac binary and cannot run on the VM, so this stands in for it:
newline-delimited JSON-RPC on stdin/stdout, `initialize`, `tools/list`, and two tools.
`write_file` writes relative to the server's cwd (the workspace inside the sandbox).
"""
import json
import os
import sys

TOOLS = [
    {
        "name": "echo",
        "description": "Returns the text it was given.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "write_file",
        "description": "Writes content to a path relative to the server cwd.",
        "inputSchema": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"],
        },
    },
]


def text_result(text, is_error=False):
    return {"content": [{"type": "text", "text": text}], "isError": is_error}


def handle(msg):
    method = msg.get("method")
    if method == "initialize":
        return {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mcp-echo", "version": "1.0.0"},
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            return text_result(args.get("text", ""))
        if name == "write_file":
            try:
                with open(args["path"], "w", encoding="utf-8") as fh:
                    fh.write(args["content"])
                return text_result(f"wrote {args['path']}")
            except OSError as exc:
                return text_result(f"{type(exc).__name__}: {exc}", is_error=True)
        return text_result(f"unknown tool {name}", is_error=True)
    if method and method.startswith("notifications/"):
        return None
    return None


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as exc:
            print(json.dumps({"jsonrpc": "2.0", "error": {"code": -32700, "message": str(exc)}, "id": None}), flush=True)
            continue
        result = handle(msg)
        if result is not None and "id" in msg:
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
        elif result is None and "id" in msg:
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {}}), flush=True)


if __name__ == "__main__":
    main()
