#!/usr/bin/env python3
"""R6d Tier1 stand-in: local OpenAI-compatible chat/completions mock.

NOT a real model. Purpose: keep the R6d offline governance loop (Tier 1b)
deterministic without a real LLM while still reaching the exec-review step:

  - The plan-review old-eval path (host, no container) calls the reviewer
    model via Inspect's openai-api provider. The real kuaizi gateway exceeds
    Inspect's 45s scoring time limit (plan_review.py.tmpl sets no scorer
    time_limit), so Tier 1b must use a local mock that answers instantly.
  - The mock returns `{"pass": true, ...}` for every request, so a faithful
    plan passes plan review and the run advances to execution.

For the executor step (real docker sandbox) the e2e uses Inspect's built-in
mockllm model (raw id, no key, no network) so the eval completes without a
provider. This file is NOT used for the executor.

Listens on 127.0.0.1:<port>. Implements POST /v1/chat/completions (non-stream)
and logs every request body to the run dir for evidence.
"""
import http.server
import json
import socketserver
import sys
import time
from pathlib import Path

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8899
LOG = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("mock-requests.jsonl")
VERDICT = sys.argv[3] if len(sys.argv) > 3 else '{"pass": true, "reason": "plan faithfully addresses the owner request"}'


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def _record(self, body: bytes):
        rec = {
            "ts": time.time(),
            "method": self.command,
            "path": self.path,
            "body": body.decode("utf-8", "replace"),
        }
        with LOG.open("a") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        self._record(body)
        try:
            req = json.loads(body)
        except Exception:
            req = {}
        model = req.get("model", "unknown")
        content = VERDICT
        resp = {
            "id": "chatcmpl-r6dmock",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18},
        }
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        data = json.dumps({"service": "r6d-mock", "ok": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    LOG.parent.mkdir(parents=True, exist_ok=True)
    with socketserver.ThreadingTCPServer(("127.0.0.1", PORT), Handler) as httpd:
        print(f"r6d mock provider on 127.0.0.1:{PORT}", flush=True)
        httpd.serve_forever()
