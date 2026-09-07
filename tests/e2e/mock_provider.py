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
import os
import socketserver
import sys
import time
from pathlib import Path

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 8899
LOG = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("mock-requests.jsonl")
VERDICT = sys.argv[3] if len(sys.argv) > 3 else '{"pass": true, "reason": "plan faithfully addresses the owner request"}'

# pi write 工具名 + 产出路径（宿主形态：host.rs prompt 指定
# <run>/plan-review/outputs/verdict.json 绝对路径，env 注入；缺省保持容器时代
# 旧值供历史复现）。
WRITE_TOOL = "write"
VERDICT_OUTPUT = os.environ.get("ALFRED_MOCK_VERDICT_OUTPUT", "/outputs/verdict.json")
TOOL_CALL_ID = "call_r6d_plan_write"
# exec-review 停摆模式（exec-review-deadlock.sh）：ALFRED_MOCK_EXEC_STALL 非空时，
# 请求目标是 exec-review 产出（prompt 给出的 verdict 绝对路径含 /exec-review/）→
# 永不回包——pi 挂起不退出，宿主 driver 等满 time-limit 强制 kill →
# "reviewer host pi timed out after Ns"（复现用户死锁链的 reviewer 超时）。
# 计划审查请求（/plan-review/）照常返回 write 工具调用（正常写 verdict 闭环）。
EXEC_STALL = bool(os.environ.get("ALFRED_MOCK_EXEC_STALL"))
# 工具循环收尾纯文本（pi 拿到无 tool_calls 的回复即 settle）。
CLOSE_TEXT = f"Verdict written to {VERDICT_OUTPUT}. Task complete."


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

    def _respond(self, payload: dict, stream: bool = False):
        if not stream:
            data = json.dumps(payload).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        # SSE（pi 宿主形态恒 stream:true；把完整响应包成单个 chunk + [DONE]）
        chunk = {
            "id": payload.get("id", "chatcmpl-mock"),
            "object": "chat.completion.chunk",
            "created": payload.get("created", 0),
            "model": payload.get("model", "mock"),
            "choices": [
                {
                    "index": 0,
                    "delta": payload["choices"][0]["message"],
                    "finish_reason": payload["choices"][0].get("finish_reason"),
                }
            ],
        }
        body = f"data: {json.dumps(chunk)}\n\ndata: [DONE]\n\n".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
        # 本请求的 driver prompt 全文（system + user，字符串/分段两种形态都扫）——
        # 判请求目标是计划审查还是执行审查（宿主形态 verdict 路径只在 prompt 里）。
        prompt_text = ""
        for m in req.get("messages") or []:
            c = m.get("content")
            if isinstance(c, str):
                prompt_text += c + "\n"
            elif isinstance(c, list):
                prompt_text += " ".join(
                    x.get("text", "") for x in c if isinstance(x, dict)
                ) + "\n"

        if EXEC_STALL and "/exec-review/" in prompt_text:
            # exec-review 停摆：请求已记录，但**永不回包**——pi 的 LLM 请求挂起，
            # pi 进程不退出，宿主 driver 等满 time-limit 强制 kill →
            # "reviewer host pi timed out after Ns"（用户死锁链同款超时语义，
            # 复现 run-18d1c83accf4c04002 的 reviewer driver timed_out）。
            # 线程化 server（ThreadingTCPServer）下挂住本连接不影响其他请求。
            import threading
            threading.Event().wait()  # 永久阻塞
            return
        if has_pi_write_tool(req) and not has_tool_role_message(req):
            # 计划审查首请求：驱动 pi 经正常 write 工具循环写 verdict.json。
            # 路径单一真源 = driver prompt 给出的 outputs/verdict.json 绝对路径
            # （宿主形态路径只在 prompt 里；ALFRED_MOCK_VERDICT_OUTPUT 仅旧容器
            # 形态兜底）。找不到路径 → 兜底默认值（保持历史行为）。
            import re
            m = re.search(r"(/\S+/outputs/verdict\.json)", prompt_text)
            write_path = m.group(1) if m else VERDICT_OUTPUT
            arguments = json.dumps(
                {"path": write_path, "content": VERDICT}, ensure_ascii=False
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
        self._respond(resp, stream=bool(req.get("stream")))

    def do_GET(self):
        data = json.dumps({"service": "r6d-mock", "ok": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    LOG.parent.mkdir(parents=True, exist_ok=True)
    class ReusableServer(socketserver.ThreadingTCPServer):
        allow_reuse_address = True

    with ReusableServer(("127.0.0.1", PORT), Handler) as httpd:
        print(f"r6d mock provider on 127.0.0.1:{PORT}", flush=True)
        httpd.serve_forever()
