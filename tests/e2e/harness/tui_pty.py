#!/usr/bin/env python3
"""TUI S2a/S2b/S3 pty 手验：真实 pty 起 alfred chat TUI，黑盒驱动+断言。

阶段一（需求收集→Planning 停驻→退出卫生）：
  TUI 起界面（顶栏无 run/左列空/右列占位/输入区"需求"提示）→ 空提交按态文案
  （需求收集态"需求为空"，chat_session 单一真源与 REPL 同文，S1 审 P3 核对）
  → 提交需求 → 左列回声+已受理+[pi] 答复流式 → 顶栏 run_id+● planning 实时 →
  右列看板 planning → 输入区提示切"对 pi 说" → 空提交按态文案（Planning 态
  "空输入已忽略"）→ Ctrl-D 退出 → 退出码 0 + 备用屏恢复 + 告别行可见。
阶段二（断点恢复→建图→挂起提示→拍板→终态）：
  恢复 run → "直接建图" → [orchestrator] 事件化左列（计划审查中/已升级属主，
  S2b 前缀由渲染加回）+ [pi] 计划摘要 → 挂起提示可见（升级包+输入区
  "重试/放弃/修改意见"）→ 看板 escalated+节点 → "放弃" 拍板 → Abandoned
  终态呈现 + 输入区"新需求" → Ctrl-C 退出 → 退出码 0。
阶段三（规划失败升级块多行呈现，S3 核对）：
  新 run + 离线计划文件缺失 → planning_error 升级 → 升级块多行呈现完整
  （标题行 + run_dir 续行 + 尾段 retry/revise/abandon 折行不截断）+ 挂起态
  输入区拍板引导 → state.json 落盘 escalated。
"""
import fcntl
import json
import os
import pty
import pyte
import select
import shutil
import struct
import sys
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
BIN = os.path.join(REPO, "target/debug/alfred")
TS = time.strftime("%Y%m%d-%H%M%S")
STATE = f"/tmp/alfred-tui-pty-{TS}"
ROWS, COLS = 30, 100

failures = []
# 当前活跃会话（失败时 dump 屏幕自诊断）。
active_session = None


def check(name, cond, detail=""):
    tag = "PASS" if cond else "FAIL"
    print(f"{tag}({name}) {detail}")
    if not cond:
        failures.append(name)
        if active_session is not None:
            print(f"--- FAIL({name}) 屏幕 ---")
            for i, row in enumerate(active_session.text().split("\n")):
                print(f"{i:2}|{row.rstrip()}")
            print("--- dump 结束 ---")

def base_env():
    return {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "TERM": "xterm-256color",
        "ALFRED_OFFLINE": "1",
        "ALFRED_STATE_DIR": STATE,
        "ALFRED_PLANNER_MODEL": "glm-5.3-flash",
        "ALFRED_EXECUTOR_MODEL": "glm-5.3-flash",
        "ALFRED_REVIEWER_MODEL": "glm-5.3-flash",
    }


class PtySession:
    def __init__(self, args, env):
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            try:
                os.execve(BIN, [BIN] + args, env)
            finally:
                os._exit(127)
        # 子进程 exec 前定好窗口尺寸（TUI init 按它布局）。
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.raw = b""
        self.exited = None

    def pump(self, timeout=0.2):
        r, _, _ = select.select([self.master], [], [], timeout)
        if not r:
            return False
        try:
            data = os.read(self.master, 65536)
        except OSError:
            return False
        if not data:
            return False
        self.raw += data
        self.stream.feed(data)
        return True

    def text(self):
        # pyte .display 对宽字符尾格（data=""）有 wcwidth 崩 bug——直接按
        # buffer 逐格拼接（空 data 不贡献字符，CJK 行读序正确）。
        lines = []
        for y in range(self.screen.lines):
            row = []
            for x in range(self.screen.columns):
                row.append(self.screen.buffer[y][x].data or "")
            lines.append("".join(row))
        return "\n".join(lines)

    def reaped(self):
        try:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            return True
        if pid == self.pid:
            self.exited = status
            return True
        return False

    def pump_for(self, secs):
        """连续读满 secs（pump 单次读到即返回——半帧需要循环收尾）。"""
        end = time.time() + secs
        while time.time() < end:
            self.pump(0.05)

    def wait_for(self, marker, timeout=20):
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.pump(0.2)
            if self.reaped():
                break
            if marker in self.text():
                # 命中后连续收 0.5s 再复检——pty 采样可能撞上半帧（ratatui
                # 差分写流未收尾，屏幕截在半行/底部缺失）。
                self.pump_for(0.5)
                return marker in self.text()
        return marker in self.text()

    def wait_exit(self, timeout=10):
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.pump(0.1)
            if self.reaped():
                while self.pump(0.05):
                    pass
                return self.exited
        return None

    def send(self, data):
        if isinstance(data, str):
            data = data.encode("utf-8")
        os.write(self.master, data)

    def close(self):
        try:
            os.kill(self.pid, 9)
        except ProcessLookupError:
            pass
        try:
            os.close(self.master)
        except OSError:
            pass


os.makedirs(STATE, exist_ok=True)

# ── 阶段一：需求收集 → Planning 停驻 → 退出卫生 ──
run1 = f"{STATE}/run-tui-1"
reply_file = f"{STATE}/reply1.txt"
with open(reply_file, "w") as f:
    f.write("pi 需要先澄清吗\n")

env1 = base_env() | {"ALFRED_OFFLINE_REPLY_FILE": reply_file}
s1 = PtySession(["chat", "--run-dir", run1], env1)
active_session = s1
try:
    up = s1.wait_for("对话") and s1.wait_for("状态")
    check("tui-boot", up, "TUI 四区界面出现")
    t = s1.text()
    check("boot-statusbar-no-run", "无 run" in t, "顶栏无 run 占位")
    check("boot-input-hint", "需求" in t, "输入区需求收集提示")
    check("boot-panel-placeholder", "无 run——提交需求后建立" in t, "右列占位")

    # 空提交（需求收集态）：chat_session 单一真源按态文案，TUI 面经通道呈现
    # （S1 审 P3 核对——与 REPL 同文）。
    s1.send("\r")
    check("empty-submit-requirement",
          s1.wait_for("需求为空——请直接说需求。", 10),
          "需求收集态空提交按态文案（与 REPL 同文）")

    s1.send("写一个 hello.txt 内容是 Hello\r")
    got_pi = s1.wait_for("pi 需要先澄清吗", 20)
    t = s1.text()
    check("submit-echo", "你: 写一个 hello.txt 内容是 Hello" in t, "左列属主回声")
    check("accepted-notice", "已受理：写一个 hello.txt 内容是 Hello" in t, "左列已受理转写")
    check("pi-reply-stream", got_pi, "左列 [pi] 答复流式")
    check("statusbar-runid", "run-tui-1" in t, "顶栏 run_id 实时")
    check("statusbar-planning", "● planning" in t, "顶栏 ● planning 实时")
    check("panel-planning", t.count("● planning") >= 1 and "计划: —" in t, "右列看板 planning 态")
    check("input-hint-planning", "对 pi 说" in t, "输入区切对 pi 说提示")

    # 空提交（Planning 态）：另一套按态文案（S1 审 P3 核对——单一真源产生）。
    s1.send("\r")
    check("empty-submit-planning",
          s1.wait_for("空输入已忽略。", 10),
          "Planning 态空提交按态文案（与 REPL 同文）")

    s1.send(b"\x04")  # Ctrl-D 空缓冲退出
    status = s1.wait_exit(10)
    check("exit-code-0", status is not None and os.waitstatus_to_exitcode(status) == 0,
          f"退出码 {status}")
    check("leave-alt-screen", b"\x1b[?1049l" in s1.raw, "备用屏恢复序列")
    check("farewell-line", "会话结束" in s1.raw.decode("utf-8", "replace"), "告别行可见")
finally:
    s1.close()

# 阶段一落盘断言：run 建立 + planning 停驻
state1 = json.load(open(f"{run1}/state.json"))
check("run1-planning-persisted", state1["state_machine"]["state"] == "planning",
      f"state.json={state1['state_machine']['state']}")
req_id = json.load(open(f"{run1}/request.json"))["id"]

# ── 阶段二：断点恢复 → 建图 → 挂起提示 → 拍板 → 终态 ──
plan_file = f"{STATE}/plan.json"
with open(plan_file, "w") as f:
    json.dump({
        "request_id": req_id,
        "nodes": [{
            "id": "task-1",
            "summary": "create hello.txt with content Hello",
            "contract": {
                "prompt": "Create a file named hello.txt in the workspace.",
                "acceptance_criteria": "hello.txt exists",
                "reviewer_models": [],
            },
            "sandbox": {
                "volumes": [], "runtime": None, "packages": [],
                "network": False, "workspace_subdirs": ["src"],
            },
        }],
    }, f, ensure_ascii=False)

env2 = base_env() | {"ALFRED_OFFLINE_PLAN_FILE": plan_file}
s2 = PtySession(["chat", "--run-dir", run1], env2)
active_session = s2
try:
    resumed = s2.wait_for("恢复 run", 20)
    check("resume-banner", resumed, "左列恢复 run 横幅")
    t = s2.text()
    check("resume-planning-hint", "对 pi 说" in t, "恢复后输入区提示")

    s2.send("直接建图\r")
    got_susp = s2.wait_for("治理挂起，等待属主拍板", 30)
    t = s2.text()
    check("suspension-visible", got_susp, "挂起升级包可见")
    check("orchestrator-eventized",
          "计划审查中" in t and "已升级属主" in t,
          "治理 [orchestrator] 行事件化左列可见（前缀渲染加回）")
    check("plan-summary-pi", "计划（1 节点）" in t, "[pi] 计划摘要")
    check("escalation-reason", "升级原因" in t or "执行审查意见" in t, "升级原因/意见行")
    check("suspended-input-hint", "回复：重试 / 放弃 / 或直接说修改意见" in t, "挂起输入区提示")
    check("panel-escalated", "● escalated" in t, "看板 escalated 态")
    check("panel-node", "task-1: create hello.txt" in t, "看板计划节点")

    s2.send("放弃\r")
    got_abandoned = s2.wait_for("run 已放弃（Abandoned）", 20)
    t = s2.text()
    check("abandoned-terminal", got_abandoned, "Abandoned 终态呈现")
    check("new-requirement-hint", "新需求" in t, "终态后输入区新需求提示")
    check("panel-abandoned", "● abandoned" in t, "看板 abandoned 态")

    s2.send(b"\x03")  # Ctrl-C 恒退出
    status = s2.wait_exit(10)
    check("ctrl-c-exit-0", status is not None and os.waitstatus_to_exitcode(status) == 0,
          f"退出码 {status}")
finally:
    s2.close()

state2 = json.load(open(f"{run1}/state.json"))
check("run1-abandoned-persisted", state2["state_machine"]["state"] == "abandoned",
      f"state.json={state2['state_machine']['state']}")

# ── 阶段三：规划失败升级块多行呈现（S3 核对：run_dir 续行完整） ──
# 离线计划文件缺失 → planning_error 升级 → 升级块多行 notice（含 run_dir 续行）
# 经 OrchestratorNotice 进左列，conversation_rows 按 '\n' 展开——断言折行不截断。
run3 = f"{STATE}/run-tui-3"
env3 = base_env() | {"ALFRED_OFFLINE_PLAN_FILE": f"{STATE}/definitely-missing.json"}
s3 = PtySession(["chat", "--run-dir", run3], env3)
active_session = s3
try:
    s3.wait_for("对话", 20)
    s3.send("触发规划失败\r")
    got_block = s3.wait_for("规划失败已升级属主", 30)
    t = s3.text()
    check("escalation-block-multiline", got_block, "升级块多行呈现（标题行）")
    check("escalation-block-rundir", f"run_dir: {run3}" in t,
          f"升级块 run_dir 续行可见（run_dir: {run3}）")
    check("escalation-block-tail", "retry/revise/abandon" in t,
          "升级块续行尾段（retry/revise/abandon）折行不截断")
    check("escalation-suspend-hint", "回复：重试 / 放弃 / 或直接说修改意见" in t,
          "挂起态输入区拍板引导")
finally:
    s3.close()

state3 = json.load(open(f"{run3}/state.json"))
check("run3-escalated-persisted", state3["state_machine"]["state"] == "escalated",
      f"state.json={state3['state_machine']['state']}")

print("=" * 60)
if failures:
    print(f"TUI pty 手验失败：{failures}")
    sys.exit(1)
print("TUI pty 手验全部通过（阶段一+阶段二+阶段三）")
