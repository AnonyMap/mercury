#!/usr/bin/env python3
"""A tiny OpenAI-compatible server for smoke tests and screenshots.

First turn: asks to run `terminal` (ls) and then answers in markdown.
Any message containing "delete" triggers a dangerous command (approval demo).
Usage: mock_llm.py PORT
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODELS = ["hermes-4-14b", "qwen3-coder-30b", "llama-3.3-70b"]


def chunk(delta, finish=None):
    return {"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self._json(200, {"object": "list", "data": [{"id": m} for m in MODELS]})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        n = int(self.headers.get("content-length", "0"))
        req = json.loads(self.rfile.read(n) or b"{}")
        msgs = req.get("messages", [])
        last = msgs[-1] if msgs else {}
        events = []
        if last.get("role") == "user" and "Summarise" in str(last.get("content", "")):
            events = [chunk({"content": "- earlier discussion summarised"})]
        elif last.get("role") == "user":
            text = str(last.get("content", ""))
            cmd = "rm -rf ./build" if "delete" in text.lower() else "ls -la | head -5"
            events.append(chunk({"reasoning_content": "The user wants to see the project. I'll list the directory first."}))
            events.append(chunk({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "terminal", "arguments": ""}}]}))
            args = json.dumps({"command": cmd})
            events.append(chunk({"tool_calls": [{"index": 0, "function": {"arguments": args}}]}))
            events.append(chunk({}, "tool_calls"))
        else:
            answer = (
                "Here's what I found in the working directory:\n\n"
                "| Item | What it is |\n|---|---|\n"
                "| `Cargo.toml` | the Rust manifest |\n| `src/` | application code |\n| `tests/` | integration tests |\n\n"
                "Next step: run `cargo test` to confirm everything passes.\n\n"
                "```bash\ncargo test --release\n```"
            )
            for i in range(0, len(answer), 12):
                events.append(chunk({"content": answer[i:i + 12]}))
            events.append(chunk({}, "stop"))
        events.append({"choices": [], "usage": {"prompt_tokens": 812, "completion_tokens": 96, "total_tokens": 908}})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for e in events:
            self.wfile.write(f"data: {json.dumps(e)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.02)
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
