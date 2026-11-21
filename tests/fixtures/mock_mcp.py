#!/usr/bin/env python3
"""Minimal stdio MCP server for tests: tools `echo` and `fail`.

It also does the awkward things real servers do: writes a log line to stdout before speaking
JSON-RPC, sends notifications, and asks the client a `ping` before answering tools/list.
"""
import json
import sys

out = sys.stdout


def send(obj):
    out.write(json.dumps(obj) + "\n")
    out.flush()


out.write("mock-mcp starting up (not JSON)\n")
out.flush()
print("debug noise on stderr", file=sys.stderr, flush=True)
pinged = False
pong = False
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if method is None:  # a response from the client (to our ping)
        if mid == "srv-1" and "result" in msg:
            pong = True
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-mcp", "version": "1.2.3"}}})
    elif method == "notifications/initialized":
        send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hello"}})
    elif method == "tools/list":
        if not pinged:
            pinged = True
            send({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "echo", "description": "Echo text back",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
            {"name": "fail", "description": "Always fails", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"].get("arguments", {})
        if name == "echo":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "echo: " + args.get("text", "") + (" (pong)" if pong else "")}]}})
        elif name == "fail":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "it broke"}], "isError": True}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "unknown tool " + name}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "nope"}})
