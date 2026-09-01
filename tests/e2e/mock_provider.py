#!/usr/bin/env python3
"""R6d Tier1 stand-in: local OpenAI-compatible chat/completions mock.

NOT a real model. Purpose: keep the R6d offline governance loop (Tier 1b)
deterministic without a real LLM while still reaching the exec-review step.

R6d(补) 起 Tier1b 的计划审查走 **reviewer 容器**（pi 在容器内读 /inputs 判
忠实度、写 /outputs/verdict.json）。mock 必须驱动 pi **经正常工具循环**写
verdict 文件（非宿主直调 reviewer 模型——宿主只代发 LLM，文件由容器内 pi
用 write 工具落盘）：

  - 计划审查首请求（请求含 pi tools 且 messages **无** tool 角色消息）→
    返回 OpenAI `tool_calls=[write /outputs/verdict.json, content=<VERDICT>]`；
    pi 执行 write 工具把 verdict 落盘（工具循环闭环）。
  - 后续请求（messages **含** tool 角色消息 = write 工具结果已回传）→
    返回纯文本收尾（pi 结束循环 → reviewer driver 读 /outputs/verdict.json
    成功，宿主 Pydantic 等价校验 pass）。
  - 兜底（无 pi tools，如旧 eval 直判路径）→ 恒返回 VERDICT 纯文本
    （兼容历史行为）。

工具名/参数经 pi 源码探针确认：write 工具名 `write`，参数 `{path, content}`
（见 @earendil-works/pi-coding-agent dist/core/tools/write.js writeSchema）。

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

# pi write 工具名 + 产出路径（reviewer 容器 driver 约定 /outputs/verdict.json）。
WRITE_TOOL = "write"
VERDICT_OUTPUT = "/outputs/verdict.json"
TOOL_CALL_ID = "call_r6d_plan_write"
# 工具循环收尾纯文本（pi 拿到无 tool_calls 的回复即 settle）。
CLOSE_TEXT = "Verdict written to /outputs/verdict.json. Task complete."


def has_pi_write_tool(req: dict) -> bool:
    """请求含 pi tools（tools 列表里有 write 工具定义）→ reviewer 容器内请求。

    OpenAI 格式：tools[].function.name。pi 默认工具集含 write（read/bash/
    edit/write/grep/find/ls）。
    """
    for tool in req.get("tools") or []:
        fn = tool.get("function") or {}
        if fn.get("name") == WRITE_TOOL:
            return True
    return False


def has_tool_role_message(req: dict) -> bool:
    """messages 含 tool 角色消息 → 上一轮工具结果已回传（后续轮次收尾）。"""
    for msg in req.get("messages") or []:
        if msg.get("role") == "tool":
            return True
    return False


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

    def _respond(self, payload: dict):
        data = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        self._record(body)
        try:
            req = json.loads(body)
        except Exception:
            req = {}
        model = req.get("model", "unknown")
        usage = {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}

        if has_pi_write_tool(req) and not has_tool_role_message(req):
            # 计划审查首请求：驱动 pi 经正常 write 工具循环写 verdict.json。
            arguments = json.dumps(
                {"path": VERDICT_OUTPUT, "content": VERDICT}, ensure_ascii=False
            )
            resp = {
                "id": "chatcmpl-r6dmock-tool",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": None,
                            "tool_calls": [
                                {
                                    "id": TOOL_CALL_ID,
                                    "type": "function",
                                    "function": {
                                        "name": WRITE_TOOL,
                                        "arguments": arguments,
                                    },
                                }
                            ],
                        },
                        "finish_reason": "tool_calls",
                    }
                ],
                "usage": usage,
            }
        elif has_tool_role_message(req):
            # write 工具结果已回传：纯文本收尾（pi settle → driver 读 verdict 成功）。
            resp = {
                "id": "chatcmpl-r6dmock-close",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": CLOSE_TEXT},
                        "finish_reason": "stop",
                    }
                ],
                "usage": usage,
            }
        else:
            # 兜底（无 pi tools，如旧 eval 直判路径）：恒返回 VERDICT 纯文本。
            resp = {
                "id": "chatcmpl-r6dmock",
                "object": "chat.completion",
                "created": int(time.time()),
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": VERDICT},
                        "finish_reason": "stop",
                    }
                ],
                "usage": usage,
            }
        self._respond(resp)

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
