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

M4（multinode.sh）新增两模式（env 开关，缺省关闭——既有用例行为不变）：
  - ALFRED_MOCK_EXEC_VERDICT：执行审查 verdict 内容（verdict 路径含
    /exec-review/ 时写它；缺省 grade C——parse_exec_verdict_json 契约）。
    计划审查（/plan-review/）照旧写第 3 参 VERDICT。
  - ALFRED_MOCK_EXECUTOR_SCRIPT：执行者脚本模式（notes_report）——非审查类
    pi 请求（prompt 无 verdict 产出路径 = 容器内执行者 pi）按契约脚本化驱动
    工具循环：task-1 写 /workspace/notes.md；task-2 读 notes.md 后从读到的
    真实内容派生 /workspace/report.md（共享 ws 传递走真实数据流）。

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
# exec-review 停摆模式（exec-review-deadlock.sh）：ALFRED_MOCK_EXEC_STALL 非空时，
# 请求目标是 exec-review 产出（prompt 给出的 verdict 绝对路径含 /exec-review/）→
# 永不回包——pi 挂起不退出，宿主 driver 等满 time-limit 强制 kill →
# "reviewer host pi timed out after Ns"（复现用户死锁链的 reviewer 超时）。
# 计划审查请求（/plan-review/）照常返回 write 工具调用（正常写 verdict 闭环）。
EXEC_STALL = bool(os.environ.get("ALFRED_MOCK_EXEC_STALL"))
# 执行审查 verdict（M4 multinode.sh）：请求目标是 exec-review 产出（verdict
# 路径含 /exec-review/）→ 写本 verdict（grade C/I/P + failure_class +
# rationale，parse_exec_verdict_json 契约）；计划审查（/plan-review/）照旧写
# 第 3 参 VERDICT（{"pass", "reason"}）。缺省 C——现有用例（r6d tier1b 离线
# 回退 / deadlock stall）不触达本分支，无回归面。
EXEC_VERDICT = os.environ.get(
    "ALFRED_MOCK_EXEC_VERDICT",
    '{"grade": "C", "failure_class": null, "rationale": "artifacts satisfy every node acceptance criteria"}',
)
# 执行者脚本模式（M4 multinode.sh，notes_report）：ALFRED_MOCK_EXECUTOR_SCRIPT
# 非空时，非审查类 pi 请求（prompt 无 /outputs/verdict.json——即容器内执行者
# pi）按契约内容脚本化驱动工具循环，产出确定性 ws 产物：
#   task-1（契约只提 notes.md）→ write /workspace/notes.md（固定 bullet 要点）；
#   task-2（契约提 report.md，基于前置 notes.md）→ read /workspace/notes.md →
#     从读到的真实内容派生 report.md（要点改写成完整句子）→ write。
# task-2 的产物内容由 read 工具结果派生（非 mock 内常量）——共享 ws 传递
# （task-2 产物基于 task-1 产物）经真实数据流验证。
EXECUTOR_SCRIPT = os.environ.get("ALFRED_MOCK_EXECUTOR_SCRIPT", "")
NOTES_PATH = "/workspace/notes.md"
REPORT_PATH = "/workspace/report.md"
NOTES_CONTENT = (
    "# Notes\n"
    "\n"
    "- Alpha point about the shared workspace mount semantics.\n"
    "- Beta point about the topological execution order.\n"
    "- Gamma point about the multi-node artifact passing.\n"
)



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

def last_tool_call(req: dict):
    """(最近一次工具调用的工具名, 其 tool 结果内容)——无工具轮次返回 (None, "")。

    OpenAI 消息序：... assistant(tool_calls) → tool(结果)。倒序找 tool 角色消息，
    再回看其前的 assistant 消息取 tool_calls[-1] 的函数名。
    """
    msgs = req.get("messages") or []
    for i in range(len(msgs) - 1, -1, -1):
        if msgs[i].get("role") != "tool":
            continue
        content = msgs[i].get("content")
        text = content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)
        for j in range(i - 1, -1, -1):
            if msgs[j].get("role") != "assistant":
                continue
            calls = msgs[j].get("tool_calls") or []
            if calls:
                return (calls[-1].get("function") or {}).get("name"), text
            break
        return None, text
    return None, ""


def extract_bullet_points(text: str) -> list[str]:
    """从 read 结果（notes.md 内容）提取 bullet 要点文本。

    宽容行内前缀（pi read 可能带行号等装饰）：取每行首个 "- " 之后的部分。
    """
    points = []
    for line in text.splitlines():
        if "- " in line:
            point = line.split("- ", 1)[1].strip()
            if point:
                points.append(point)
    return points


def executor_script_response(req: dict, prompt_text: str, model: str) -> dict:
    """notes_report 执行者脚本（确定性，见 EXECUTOR_SCRIPT 注释）。

    任务分支按契约内容判（prompt = 契约 + 挂载锚）：task-2 契约提 report.md
    （且提 notes.md 作前置）；task-1 契约只提 notes.md。轮次按
    last_tool_call 判（None=首轮；read/write=对应工具结果已回传）。
    """
    is_report_task = "report.md" in prompt_text
    last_tool, last_result = last_tool_call(req)
    if not is_report_task:
        # task-1：写 notes.md → 收尾。
        if last_tool is None:
            return tool_call_response("write", {"path": NOTES_PATH, "content": NOTES_CONTENT}, model)
        return plain_text_response(f"notes.md written to {NOTES_PATH}. Task complete.", model)
    # task-2：读前置产物 → 由读到的内容派生 report.md → 收尾。
    if last_tool is None:
        return tool_call_response("read", {"path": NOTES_PATH}, model)
    if last_tool == "read":
        points = extract_bullet_points(last_result)
        if not points:
            return plain_text_response(
                "read of notes.md returned no bullet points; cannot derive report.md", model
            )
        lines = ["# Report", "", "The notes points restated as full sentences:", ""]
        for i, point in enumerate(points, 1):
            lines.append(f"{i}. {point} — restated as a full sentence for the report.")
        content = "\n".join(lines) + "\n"
        return tool_call_response("write", {"path": REPORT_PATH, "content": content}, model)
    return plain_text_response(f"report.md written to {REPORT_PATH}. Task complete.", model)


def tool_call_response(fn_name: str, arguments: dict, model: str) -> dict:
    """OpenAI tool_calls 响应（pi 执行工具 → 结果回传 → 下一轮收尾）。"""
    return {
        "id": f"chatcmpl-mock-tool-{fn_name}",
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
                            "id": f"call_mock_{fn_name}",
                            "type": "function",
                            "function": {
                                "name": fn_name,
                                "arguments": json.dumps(arguments, ensure_ascii=False),
                            },
                        }
                    ],
                },
                "finish_reason": "tool_calls",
            }
        ],
        "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18},
    }


def plain_text_response(text: str, model: str) -> dict:
    """纯文本收尾（pi 拿到无 tool_calls 的回复即 settle）。"""
    return {
        "id": "chatcmpl-mock-close",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18},
    }


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
        # 审查类请求判别：driver prompt 恒含 verdict 产出绝对路径
        # （<run>/<mode>-review/outputs/verdict.json）；执行者请求（容器内 pi，
        # prompt = 契约 + 挂载锚）不含。
        is_review_request = "/outputs/verdict.json" in prompt_text
        if EXECUTOR_SCRIPT and has_pi_write_tool(req) and not is_review_request:
            # 执行者脚本模式（M4 multinode.sh）：按契约内容脚本化驱动工具循环
            # （task-1 写 notes.md / task-2 读 notes.md 派生 report.md）。
            resp = executor_script_response(req, prompt_text, model)
        elif has_pi_write_tool(req) and not has_tool_role_message(req):
            # 审查首请求：驱动 pi 经正常 write 工具循环写 verdict.json。
            # 路径单一真源 = driver prompt 给出的 outputs/verdict.json 绝对路径
            # （宿主形态路径只在 prompt 里；ALFRED_MOCK_VERDICT_OUTPUT 仅旧容器
            # 形态兜底）。找不到路径 → 兜底默认值（保持历史行为）。
            # verdict 内容按目标分流：exec-review → EXEC_VERDICT（grade 契约）；
            # 其余（plan-review）→ VERDICT（{"pass","reason"} 契约）。
            import re
            m = re.search(r"(/\S+/outputs/verdict\.json)", prompt_text)
            write_path = m.group(1) if m else VERDICT_OUTPUT
            verdict = EXEC_VERDICT if "/exec-review/" in write_path else VERDICT
            resp = tool_call_response(
                WRITE_TOOL, {"path": write_path, "content": verdict}, model
            )
        elif has_tool_role_message(req):
            # write 工具结果已回传：纯文本收尾（pi settle → driver 读 verdict 成功）。
            resp = plain_text_response(CLOSE_TEXT, model)
        else:
            # 兜底（无 pi tools，如旧 eval 直判路径）：恒返回 VERDICT 纯文本。
            resp = plain_text_response(VERDICT, model)
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
