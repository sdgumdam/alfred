#!/usr/bin/env bash
# ============================================================================
# R6b e2e：planner 宿主 pi 化回归（宿主 pi agent，去容器）
#
# 三层：
#   Tier 0（默认，无外部依赖）：cargo test —— 离线 converse 单测 +
#     宿主驱动（pi-config 生成 / AGT 策略渲染）单测。这是"离线路径全绿"
#     的核心。
#   Tier 1（需 inspect CLI，无需 docker / 无需真 LLM）：ALFRED_OFFLINE 规划 +
#     mockllm 计划审查（unscored → 升级）——断言 converse 离线产物
#     （llm-calls/dagspec.json/conversation.json）。inspect 缺失时跳过（打印 SKIP）。
#   Tier 2（需宿主 pi + 真模型，验方跑）：R6B_REAL=1 时真宿主 converse
#     产合法 DagSpec（zhipu 真跑）+ AGT 审计断言。默认关闭（留给验方）。
#
# 模型：Tier 1 用 mockllm（inspect 内建，无需 key）做计划审查——planner 离线
#   直通、executor 不触发，因此不需要 docker 与真实 provider。
# 驱动：黑盒经真实 `alfred` bin（codux 可调度 CLI driver：run/feed/status）驱动——
#   r6b 以 `cargo run --bin alfred -- run|feed` 驱动治理环（run 初始化 + 推进；
#   feed 喂属主消息 → `governance::feed_owner_message` 续跑）。
# 验收：cargo test 全绿 + Tier 1 离线回归 PASS（或 inspect 缺失 SKIP）。
# ============================================================================
#
# Tier 1 用例：
#   caseA 离线 converse（driver run → 计划 → mockllm 审查 unscored → 升级挂起）
#   caseB feed revise 属主补充（维护者已回退——session_doc 保持空文档语义）
#   caseC P1-2 Reply 多轮续入（规划器答复 → state=Planning → driver feed revise
#         续入属主答复 → 重规划 → 升级挂起）
#   caseD P2a/P2b Planning 态 Abandon（converse 答复停驻 → feed abandon 无消息、
#         planner 不可用也能弃 → 终态 Abandoned；不落 owner.message 轮）
#   caseE Retry 消息可选（feed retry 无 --message → 按来源重审 → 再升级挂起）
#   caseG P2-1 append 透传（--append-system-prompt <memory> → planner converse
#         system prompt：llm-calls 记录的 converse system prompt 含注入内存 + 基础
#         建图 schema 仍在；run 与 feed 各验一轮）
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
# --- 容器驱动 Python（P1）：ALFRED_PYTHON 优先，否则本仓 venv（Rust python_binary() 兜底 PATH） ---
if [[ -z "${ALFRED_PYTHON:-}" && -x "$REPO_ROOT/.plans/r0-lab/venv/bin/python" ]]; then
  export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"
fi

R6B_RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$R6B_RUNS"

echo "============================================="
echo "R6b Tier 0：cargo test（离线单测 + 宿主驱动单测）"
echo "============================================="
cargo test --quiet
echo "PASS(Tier0): cargo test 全绿"

# --- Tier 1：inspect CLI（离线规划回归需要） ---
if [[ -n "${ALFRED_INSPECT:-}" ]]; then
  INSPECT="$ALFRED_INSPECT"
elif [[ -x ".plans/r0-lab/venv/bin/inspect" ]]; then
  INSPECT="$REPO_ROOT/.plans/r0-lab/venv/bin/inspect"
else
  INSPECT="$(command -v inspect || true)"
fi

if [[ -z "$INSPECT" ]]; then
  echo "SKIP(Tier1): 无 inspect CLI（ALFRED_INSPECT 或 PATH）。离线 CLI 回归跳过；"
  echo "  Tier 0 已覆盖离线 converse 单测。"
else
  echo ""
  echo "============================================="
  echo "R6b Tier 1：离线规划回归（ALFRED_OFFLINE + mockllm 审查，无 docker/真 LLM）"
  echo "============================================="
  export ALFRED_INSPECT="$INSPECT"
  echo "[r6b] inspect CLI : $INSPECT"

  # 最小 config：planner/executor 用 dummy provider（不真调），reviewer 走 mockllm
  CFG_DIR="$R6B_RUNS/run-r6b-cfg"
  rm -rf "$CFG_DIR"
  mkdir -p "$CFG_DIR"
  cat > "$CFG_DIR/config.yml" <<'YAML'
providers:
  dummy:
    base_url: "http://127.0.0.1:9/v1"
    api_key: "sk-dummy-never-called"
models:
  - id: glm-4.7
    provider: dummy
roles:
  executor: glm-4.7
  planner: glm-4.7
YAML
  export ALFRED_CONFIG="$CFG_DIR/config.yml"
  export ALFRED_PLANNER_MODEL="glm-4.7"
  export ALFRED_EXECUTOR_MODEL="glm-4.7"
  export ALFRED_REVIEWER_MODEL="mockllm/model"

  # ---- Case A：离线 converse（driver run → 计划审查 unscored → 升级挂起）----
  CASE_A="$R6B_RUNS/run-r6b-offline-converse"
  rm -rf "$CASE_A"
  mkdir -p "$CASE_A"
  cat > "$CASE_A/request.json" <<'JSON'
{
  "id": "req-r6b-c1",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON
  cat > "$CASE_A/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6b-c1",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  echo "[r6b] caseA: driver run（离线规划 → mockllm 审查 unscored → escalated） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_A/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE_A/request.json" \
    --run-dir "$CASE_A" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

  python3 - "$CASE_A" <<'PY' || { echo "FAIL(caseA): converse 离线产物断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
# dagspec 落盘（converse 离线产出）
assert os.path.exists(os.path.join(run, "dagspec.json")), "dagspec.json missing"
dag = json.load(open(os.path.join(run, "dagspec.json")))
assert dag["request_id"] == "req-r6b-c1", f"request_id={dag['request_id']}"
# llm-calls/0000.json：离线 converse 记录
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
assert recs, "llm-calls/ empty"
rec = json.load(open(os.path.join(run, "llm-calls", recs[0])))
assert rec["role"] == "converse", f"role={rec['role']}"
assert rec["offline"] is True, f"offline={rec['offline']}"
assert rec["transport"] == "offline", f"transport={rec['transport']}"
user = rec["messages"][-1]["content"]
assert "req-r6b-c1" in user, "user message missing request id"
assert "属主本轮消息" in user, "user message missing owner-message marker"
assert "owner_feedback" in user, "projection missing owner_feedback"
# conversation.json：request.submit + converse.reply
conv = json.load(open(os.path.join(run, "conversation.json")))
sources = [t["source"] for t in conv["turns"]]
assert "request.submit" in sources, f"sources={sources}"
assert "converse.reply" in sources, f"sources={sources}"
# 计划审查 unscored → 升级挂起（不悄悄放行）
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
PY
  echo "PASS(caseA): 离线 converse 产物（dagspec/llm-calls/conversation.json）+ 审查出错升级"

  # ---- Case B：feed revise 属主补充（维护者已重做：ConverseDone 滚动维护——
  # 重规划轮 converse 落定后维护者更新 session_doc；离线经
  # ALFRED_MAINTAIN_OFFLINE_FILE 注入更新后文档）----
  echo "[r6b] caseB: driver feed revise（属主补充 → 重规划 → ConverseDone 滚动维护） ..."
  MSG_FILE="$CASE_A/owner-msg.txt"
  cat > "$MSG_FILE" <<'TXT'
技术选型用 Rust
TXT
  # 维护者离线注入：重规划轮维护者产出（滚动累积：key_conclusions 吸收属主补充）
  cat > "$CASE_A/maintained.json" <<'JSON'
{
  "key_file_paths": [],
  "key_conclusions": ["技术选型用 Rust"],
  "owner_feedback": []
}
JSON
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_A/plan-faithful.json" \
  ALFRED_MAINTAIN_OFFLINE_FILE="$CASE_A/maintained.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- feed \
    --run-dir "$CASE_A" \
    --decision revise \
    --message "$MSG_FILE"

  python3 - "$CASE_A" <<'PY' || { echo "FAIL(caseB): feed revise 滚动维护断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# 维护者已重做：重规划轮 ConverseDone 维护把属主补充固化进 key_conclusions
assert state["session_doc"]["key_conclusions"] == ["技术选型用 Rust"], \
    f"key_conclusions={state['session_doc']['key_conclusions']}"
assert state["owner_message"] == "技术选型用 Rust", "owner_message not updated"
# maintain llm-calls 记录：role=maintain + offline 直通 + 触发载荷含探查/产出摘要段
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
maintain = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "maintain"]
assert maintain, f"no maintain llm-call record: {recs}"
rec = json.load(open(os.path.join(run, "llm-calls", maintain[-1])))
assert rec["offline"] is True and rec["transport"] == "offline", \
    f"offline={rec['offline']} transport={rec['transport']}"
user = rec["messages"][-1]["content"]
assert "本轮探查读过" in user, "maintain prompt missing read-paths section"
assert "本轮对话产出摘要" in user, "maintain prompt missing reply-summary section"
PY
  echo "PASS(caseB): feed revise → ConverseDone 滚动维护（key_conclusions 固化属主补充 + maintain llm-calls 记录）"
  # ---- Case C：P1-2 Reply 多轮续入（规划器答复 → state=Planning → decide revise 续入 → 重规划）----
  # 规划器第一轮先答复属主（不产计划，§2.4 Reply 分支）→ 状态停 Planning（对话继续）；
  # 属主经 `driver feed --decision revise --message <回答>` 从 Planning 态续入下一轮
  # 消息（设 owner_message → planning_step 复用 revise 机制）→ 重规划
  # → 计划审查（mockllm unscored）→ 升级挂起。多轮对话端到端闭环。
  CASE_C="$R6B_RUNS/run-r6b-reply-continue"
  rm -rf "$CASE_C"
  mkdir -p "$CASE_C"
  cat > "$CASE_C/request.json" <<'JSON'
{
  "id": "req-r6b-c3",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-01T00:00:00Z"
}
JSON
  cat > "$CASE_C/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6b-c3",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  # 规划器答复（第一轮：先问属主确认技术选型，不产计划）
  cat > "$CASE_C/reply.txt" <<'TXT'
收到需求。技术选型确认一下：内容用 Rust 实现，可以吗？
TXT
  # 属主答复（第二轮：确认，作为 decide revise 的 --message）
  cat > "$CASE_C/answer.txt" <<'TXT'
可以，技术选型用 Rust。
TXT
  echo "[r6b] caseC: driver run（离线 Reply 分支 → state=Planning，对话继续） ..."
  # P2-2：规划器答复必须 surface 到终端（owner 直读，不再只落 conversation.json）。
  RUN_OUT="$CASE_C/run-output.txt"
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$CASE_C/reply.txt" \
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE_C/request.json" \
    --run-dir "$CASE_C" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60 > "$RUN_OUT" 2>&1
  grep -q "收到需求。技术选型确认一下" "$RUN_OUT" \
    || { echo "FAIL(caseC1): 规划器答复未打印到终端（run-output.txt）" >&2; exit 1; }

  python3 - "$CASE_C" <<'PY' || { echo "FAIL(caseC): Reply 分支产物断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
# 规划器答复后：状态仍 Planning（对话继续，不产计划）
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "planning", f"state={state['state_machine']['state']}"
assert not os.path.exists(os.path.join(run, "dagspec.json")), "Reply 分支不应产 dagspec"
# conversation.json：request.submit + converse.reply（规划器答复原文）
conv = json.load(open(os.path.join(run, "conversation.json")))
sources = [t["source"] for t in conv["turns"]]
assert sources == ["request.submit", "converse.reply"], f"sources={sources}"
assert "技术选型确认" in conv["turns"][1]["content"], f"reply content={conv['turns'][1]['content']}"
PY
  echo "PASS(caseC1): Reply 分支 → Planning（对话继续）"

  echo "[r6b] caseC: driver feed revise（Planning 态续入属主答复 → 重规划 + 滚动维护） ..."
  # 维护者离线注入：续入轮维护者产出（key_conclusions 吸收属主答复）
  cat > "$CASE_C/maintained.json" <<'JSON'
{
  "key_file_paths": [],
  "key_conclusions": ["技术选型确认：用 Rust"],
  "owner_feedback": []
}
JSON
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_C/plan-faithful.json" \
  ALFRED_MAINTAIN_OFFLINE_FILE="$CASE_C/maintained.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- feed \
    --run-dir "$CASE_C" \
    --decision revise \
    --message "$CASE_C/answer.txt"

  python3 - "$CASE_C" <<'PY' || { echo "FAIL(caseC): Planning 态 decide revise 续入断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# 续入后推进：重规划 → 计划审查（mockllm unscored）→ 升级挂起
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
# owner_message = 属主答复（decide revise 设入）
assert state["owner_message"] == "可以，技术选型用 Rust。", f"owner_message={state['owner_message']}"
# 维护者已重做：续入轮 ConverseDone 维护把属主答复固化进 key_conclusions
assert state["session_doc"]["key_conclusions"] == ["技术选型确认：用 Rust"], \
    f"key_conclusions={state['session_doc']['key_conclusions']}"
# conversation.json：request.submit → converse.reply(规划器提问) → owner.message(属主答复) → converse.reply(重规划)
conv = json.load(open(os.path.join(run, "conversation.json")))
sources = [t["source"] for t in conv["turns"]]
assert sources == ["request.submit", "converse.reply", "owner.message", "converse.reply"], \
    f"sources={sources}"
assert conv["turns"][2]["role"] == "owner", "turns[2] 应为属主答复"
assert "可以，技术选型用 Rust" in conv["turns"][2]["content"], f"owner turn={conv['turns'][2]['content']}"
# 续入后产出了 dagspec（重规划成功）
assert os.path.exists(os.path.join(run, "dagspec.json")), "decide revise 后续入未产 dagspec"
PY
  # ---- Case D：Planning 态 Abandon（P2a/P2b：属主放弃恒可选 + Abandon 前置路由）----
  # P2a：Planning（converse 答复停驻）→ feed abandon → 终态 Abandoned（此前无
  #      (Planning, OwnerAbandon) 转移，属主从该态无法终止 run——违背"放弃恒可选"）。
  # P2b：Abandon 前置路由——不要求消息、不落 owner.message 轮。坏 run 也能弃：
  #      feed abandon 不带 ALFRED_OFFLINE（planner 容器路径不可用，dummy provider
  #      无 docker）——Abandon 不触碰 planner 直接进终态。
  CASE_D="$R6B_RUNS/run-r6b-planning-abandon"
  rm -rf "$CASE_D"
  mkdir -p "$CASE_D"
  cat > "$CASE_D/request.json" <<'JSON'
{
  "id": "req-r6b-c4",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-01T00:00:00Z"
}
JSON
  cat > "$CASE_D/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6b-c4",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  cat > "$CASE_D/reply.txt" <<'TXT'
收到需求。技术选型确认一下：内容用 Rust 实现，可以吗？
TXT
  echo "[r6b] caseD: driver run（离线 Reply 分支 → state=Planning，对话继续） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$CASE_D/reply.txt" \
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE_D/request.json" \
    --run-dir "$CASE_D" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

  python3 - "$CASE_D" <<'PY' || { echo "FAIL(caseD1): Reply 分支 → Planning" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "planning", f"state={state['state_machine']['state']}"
PY
  echo "PASS(caseD1): Reply 分支 → Planning（对话继续）"

  # 坏 run：不带 ALFRED_OFFLINE——planner 容器路径不可用。前置路由让 Abandon
  # 不触碰 planner 直接进终态。
  echo "[r6b] caseD: driver feed abandon（无 --message；planner 不可用也能弃） ..."
  env -u ALFRED_OFFLINE -u ALFRED_OFFLINE_PLAN_FILE -u ALFRED_OFFLINE_REPLY_FILE \
  cargo run --quiet -p alfred-cli --bin alfred -- feed \
    --run-dir "$CASE_D" \
    --decision abandon

  python3 - "$CASE_D" <<'PY' || { echo "FAIL(caseD2): Planning 态 Abandon 断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# 终态 Abandoned（P2a：Planning → OwnerAbandon 合法转移）
assert state["state_machine"]["state"] == "abandoned", f"state={state['state_machine']['state']}"
# P2b：不落 owner.message 轮——但 Retry/Abandon 决策补落
# panel.decision 轮（ConversationSource::PanelDecision，reviewer 可见升级拍板，
# P3 契约）：conversation.json = request.submit → converse.reply → panel.decision
conv = json.load(open(os.path.join(run, "conversation.json")))
sources = [t["source"] for t in conv["turns"]]
assert sources == ["request.submit", "converse.reply", "panel.decision"], \
    f"sources={sources}"
assert "owner.message" not in sources, f"sources={sources}"
# 会话文档保持空（key_conclusions 无新增）
assert state["session_doc"]["key_conclusions"] == [], \
    f"key_conclusions={state['session_doc']['key_conclusions']}"
# P2b：不设 owner_message（保持 None，无消息轮）
assert "owner_message" not in state or state["owner_message"] is None, \
    f"owner_message={state.get('owner_message')}"
PY
  echo "PASS(caseD): Planning 态 Abandon → Abandoned（坏 run planner 不可用也能弃）"

  # ---- Case E：Retry 消息可选（--message 不强制非空）----
  # caseC 结束后 run 处于 Escalated（计划审查 unscored 升级，来源 PlanReview）。
  # feed retry 不带 --message：不应因消息缺失/为空而 bail——按来源路由回
  # PlanReviewing 重审同一计划（ALFRED_OFFLINE=1 离线跳过 → unscored → 再升级挂起）。
  echo "[r6b] caseE: driver feed retry（无 --message → 按来源重审 → 再升级） ..."
  ALFRED_OFFLINE=1 \
  cargo run --quiet -p alfred-cli --bin alfred -- feed \
    --run-dir "$CASE_C" \
    --decision retry

  python3 - "$CASE_C" <<'PY' || { echo "FAIL(caseE): Retry 无消息断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# retry 无消息不应 bail；从 Escalated(PlanReview) 重审同一计划 → 离线跳过 → 再升级挂起
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
# 无消息 → 不新增 owner.message 轮；Retry 决策补落
# panel.decision 轮（ConversationSource::PanelDecision，P3 契约）
conv = json.load(open(os.path.join(run, "conversation.json")))
sources = [t["source"] for t in conv["turns"]]
assert sources == ["request.submit", "converse.reply", "owner.message", "converse.reply", "panel.decision"], \
    f"sources={sources}"
# caseC 的 owner.message 仅一轮（无新增属主消息轮）
assert sum(1 for s in sources if s == "owner.message") == 1, f"sources={sources}"
PY
  # ---- Case F：PlanRejected+Retry 伪装重规划（P3a）+ panel.decision 轮（P3b）----
  # 离线注入空 workspace_subdirs 计划 → 计划审查结构闸门 pass=false → PlanRejected。
  # feed retry（无消息）→ disguise_rejection 把审查理由伪装成属主口吻驱动重规划
  # （owner_message=伪装消息，无结构化否决词）→ 落 owner.message（伪装）+
  # panel.decision 轮（拍板，§二.8）。
  CASE_F="$R6B_RUNS/run-r6b-planrejected-retry"
  rm -rf "$CASE_F"
  mkdir -p "$CASE_F"
  cat > "$CASE_F/request.json" <<'JSON'
{
  "id": "req-r6b-c5",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-01T00:00:00Z"
}
JSON
  # 不忠实计划：缺 workspace_subdirs 声明 → 计划审查结构闸门直接打回（pass=false）
  cat > "$CASE_F/plan-unfaithful.json" <<'JSON'
{
  "request_id": "req-r6b-c5",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": []
      }
    }
  ]
}
JSON
  # 重规划用的忠实计划：声明 workspace_subdirs → 结构闸门过 → 离线审查 unscored → 升级
  cat > "$CASE_F/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6b-c5",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  echo "[r6b] caseF: driver run（离线空 workspace_subdirs → 计划审查结构闸门打回 → PlanRejected） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_F/plan-unfaithful.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE_F/request.json" \
    --run-dir "$CASE_F" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

  python3 - "$CASE_F" <<'PY' || { echo "FAIL(caseF1): PlanRejected 断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "plan_rejected", \
    f"state={state['state_machine']['state']}"
assert state["plan_verdicts"] and state["plan_verdicts"][-1]["pass"] is False, \
    f"plan_verdicts={state.get('plan_verdicts')}"
PY
  echo "PASS(caseF1): 结构闸门打回 → PlanRejected（pass=false verdict）"

  echo "[r6b] caseF: driver feed retry（无消息 → 伪装消息驱动重规划） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_F/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- feed \
    --run-dir "$CASE_F" \
    --decision retry

  python3 - "$CASE_F" <<'PY' || { echo "FAIL(caseF2): 伪装重规划断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# 终态：重规划（忠实计划）→ 离线审查 unscored → 升级挂起（不悄悄放行）
assert state["state_machine"]["state"] in ("escalated", "plan_rejected"), \
    f"state={state['state_machine']['state']}"
# P3a：owner_message = 伪装消息（属主口吻，无结构化否决信号）；A2 技术词中和：
# 计划 schema 技术词（字段名 workspace_subdirs/角色名 执行者/结构词 节点·可见范围）
# 同样零出现——结构闸门 reason 源头去字段名化（"任务 X 需要指定工作目录"），伪装
# 通道兜底中和表覆盖 pi 生成的同类措辞，两层都不许技术词进 planner 记忆。
msg = state.get("owner_message") or ""
assert msg, "owner_message (disguised) missing"
forbidden = ["reject", "rejected", "rejection", "rejects", "verdict", "reviewer", "review",
             "reviews", "reviewed", "scorer", "scored", "score", "grader", "graded",
             "eval", "evaluated", "evaluation", "unscored", "workspace_subdirs",
             "执行者", "可见范围", "节点",
             "审查", "审查者", "评审", "评审者", "评分", "评估", "打分", "否决", "打回", "判定"]
low = msg.lower()
hits = [f for f in forbidden if f in low]
assert not hits, f"disguise leaked forbidden signal(s) {hits}: {msg}"
assert "重新" in msg and "需求" in msg, f"disguise missing owner tone: {msg}"
# P3a：conversation.json owner.message 轮 = 伪装消息；P3b：panel.decision 轮 = 拍板
conv = json.load(open(os.path.join(run, "conversation.json")))
sources = [t["source"] for t in conv["turns"]]
assert "owner.message" in sources, f"sources={sources}"
assert "panel.decision" in sources, f"sources={sources}"
owner_turns = [t for t in conv["turns"] if t["source"] == "owner.message"]
assert owner_turns and owner_turns[-1]["content"] == msg, \
    f"owner.message content != disguise: {owner_turns[-1]['content'] if owner_turns else None}"
dec_turns = [t for t in conv["turns"] if t["source"] == "panel.decision"]
assert dec_turns and dec_turns[-1]["content"] == "重跑（retry）", \
    f"panel.decision={[t['content'] for t in dec_turns]}"
PY
  echo "PASS(caseF): PlanRejected+Retry 伪装重规划（无否决词）+ panel.decision 轮"

  # ---- Case G：P2-1 append 透传（--append-system-prompt <memory> → planner converse
  # system prompt）----
  # S2P2ReReview 审出：P2-1 的 append 透传路径无任何自动化测试覆盖（全仓无测试用
  # --append-system-prompt / ALFRED_APPEND_SYSTEM_PROMPT），此前"空转"正是无测试掩护
  # 所致。黑盒断言（只读 llm-calls 产物，不掏内部实现）：codux 注入的内存经
  # `--append-system-prompt` 进 planner converse system prompt（llm-calls 记录与容器
  # 驱动同源），基础建图 schema 仍在。G1: driver run 透传；G2: driver feed revise
  # 透传（同 run 续跑，新一轮 converse 同样注入）。
  CASE_G="$R6B_RUNS/run-r6b-append-passthrough"
  rm -rf "$CASE_G"
  mkdir -p "$CASE_G"
  cat > "$CASE_G/request.json" <<'JSON'
{
  "id": "req-r6b-c6",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-01T00:00:00Z"
}
JSON
  cat > "$CASE_G/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6b-c6",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  # 注入内存标记（codux wrapper 每轮注入的项目上下文；断言它出现在 planner converse system prompt）
  APPEND_MEMORY="MEMORY-PROJECT-CTX：项目核心引擎用 Rust 实现，关键路径禁止同步 IO"
  echo "[r6b] caseG1: driver run（--append-system-prompt 透传 → planner converse system prompt） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_G/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- \
    --append-system-prompt "$APPEND_MEMORY" \
    run \
    --request "$CASE_G/request.json" \
    --run-dir "$CASE_G" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

  python3 - "$CASE_G" "$APPEND_MEMORY" <<'PY' || { echo "FAIL(caseG1): run append 透传断言" >&2; exit 1; }
import json, os, sys
run, memory = sys.argv[1], sys.argv[2]
# 黑盒只读产物：llm-calls 记录的 converse system prompt（与容器驱动同源）
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
assert recs, "llm-calls/ empty"
rec = json.load(open(os.path.join(run, "llm-calls", recs[0])))
assert rec["role"] == "converse", f"role={rec['role']}"
assert rec["offline"] is True and rec["transport"] == "offline", \
    f"offline={rec['offline']} transport={rec['transport']}"
system = rec["messages"][0]["content"]
assert system.startswith("你是治理系统的规划器"), "基础建图 schema 不在 system prompt"
assert "附加的项目上下文（codux 注入）" in system, "append 段落头缺失"
assert memory in system, f"注入内存标记未透传到 converse system prompt: {system!r}"
# 注入不透传不破坏治理环：run 照常推进到升级挂起（mockllm 审查 unscored）
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "escalated", \
    f"state={state['state_machine']['state']}"
PY
  echo "PASS(caseG1): run --append-system-prompt → converse system prompt 含注入内存 + 基础 schema"

  echo "[r6b] caseG2: driver feed revise（--append-system-prompt 透传 → 新一轮 converse system prompt） ..."
  cat > "$CASE_G/owner-msg.txt" <<'TXT'
继续，技术选型用 Rust。
TXT
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_G/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- \
    --append-system-prompt "$APPEND_MEMORY" \
    feed \
    --run-dir "$CASE_G" \
    --decision revise \
    --message "$CASE_G/owner-msg.txt"

  python3 - "$CASE_G" "$APPEND_MEMORY" <<'PY' || { echo "FAIL(caseG2): feed append 透传断言" >&2; exit 1; }
import json, os, sys
run, memory = sys.argv[1], sys.argv[2]
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
# G1 run 一轮 + G2 feed 续跑一轮 = 至少 2 条 converse 记录（都带同一注入）
converse = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "converse"]
assert len(converse) >= 2, f"期望 run+feed 两条 converse 记录，实际 {converse}"
for r in converse:
    rec = json.load(open(os.path.join(run, "llm-calls", r)))
    assert rec["offline"] is True and rec["transport"] == "offline", \
        f"{r}: offline={rec['offline']} transport={rec['transport']}"
    system = rec["messages"][0]["content"]
    assert system.startswith("你是治理系统的规划器"), f"{r}: 基础建图 schema 不在 system prompt"
    assert "附加的项目上下文（codux 注入）" in system, f"{r}: append 段落头缺失"
    assert memory in system, f"{r}: 注入内存标记未透传: {system!r}"
PY
  echo "PASS(caseG2): feed --append-system-prompt → 新一轮 converse system prompt 含注入内存 + 基础 schema"

  # ---- Case H：维护者重做全链路（滚动维护 + key_file_paths 审计数据源 +
  #      PlanReviewed disguise 投影 + 不可知断言）----
  # H1 轮1：run（Reply 分支）→ ConverseDone 维护（审计注入 2 条 allow read →
  #    key_file_paths 吸收；key_conclusions 吸收答复摘要）；含 1 条 deny read +
  #    1 条 allow write（deny 路径不进 key_file_paths——不可知：deny 路径泄露治理面）。
  # H2 轮2：feed revise（重规划）→ ConverseDone 维护（滚动累积，不清空轮1条目）。
  # H3 审查后：计划审查拒绝（空 workspace_subdirs）→ PlanReviewed 维护——审查
  #    理由经 disguise 投影进 review_summary（属主口吻，无禁词）；维护者 prompt
  #    零 reviewer 痕迹。
  CASE_H="$R6B_RUNS/run-r6b-maintainer-rolling"
  rm -rf "$CASE_H"
  mkdir -p "$CASE_H"
  cat > "$CASE_H/request.json" <<'JSON'
{
  "id": "req-r6b-h1",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-01T00:00:00Z"
}
JSON
  # 审计增量（维护者 key_file_paths 真实数据源——离线下由测试预写模拟 planner
  # 宿主 pi 真实读过的宿主路径；deny 记录的路径必须被过滤）：
  AGT_DIR="$CASE_H/planner/agt/audit"
  mkdir -p "$AGT_DIR"
  cat > "$AGT_DIR/audit.jsonl" <<'JSONL'
{"ts":"2026-09-01T00:00:01Z","tool_name":"read","path":"/tmp/r6b-h-src/main.rs","decision":"allow","rule":null,"reason":"default_action=allow"}
{"ts":"2026-09-01T00:00:02Z","tool_name":"read","path":"/tmp/r6b-h-src/README.md","decision":"allow","rule":null,"reason":"default_action=allow"}
{"ts":"2026-09-01T00:00:03Z","tool_name":"read","path":"/tmp/r6b-h-run/state.json","decision":"deny","rule":"deny-governance-files","reason":"路径不在允许的工作范围"}
{"ts":"2026-09-01T00:00:04Z","tool_name":"write","path":"/tmp/r6b-h-run/planner/outputs/reply.txt","decision":"allow","rule":null,"reason":"default_action=allow"}
JSONL
  cat > "$CASE_H/reply.txt" <<'TXT'
收到需求。技术选型确认一下：内容用 Rust 实现，可以吗？
TXT
  # H1 维护者离线注入（模拟维护者 LLM 产出：轮1 key_file_paths 收 allow read
  # 的宿主路径，不含 deny 的 state.json；reply 写路径不算 read——正确被排除；
  # key_conclusions 吸收属主本轮消息原文——需求进记忆，原始用例修复断言面）。
  cat > "$CASE_H/maintained-1.json" <<'JSON'
{
  "key_file_paths": ["/tmp/r6b-h-src/main.rs", "/tmp/r6b-h-src/README.md"],
  "key_conclusions": ["属主需求：创建 hello.txt，内容必须是 Hello"],
  "owner_feedback": []
}
JSON
  # H2 重规划用忠实计划（workspace_subdirs=["src"]；H3 复用 run1 落的 dagspec 语义）
  cat > "$CASE_H/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6b-h1",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  echo "[r6b] caseH1: driver run（Reply 分支 → ConverseDone 维护，key_file_paths 吸收 allow read 宿主路径） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$CASE_H/reply.txt" \
  ALFRED_MAINTAIN_OFFLINE_FILE="$CASE_H/maintained-1.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE_H/request.json" \
    --run-dir "$CASE_H" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

  python3 - "$CASE_H" <<'PY' || { echo "FAIL(caseH1): 滚动维护轮1断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
doc = state["session_doc"]
# key_file_paths = AGT 审计 allow read 的宿主路径（deny 的 state.json 与 write 的
# reply.txt 都不进——不可知：deny 路径泄露治理面）
assert sorted(doc["key_file_paths"]) == ["/tmp/r6b-h-src/README.md", "/tmp/r6b-h-src/main.rs"], \
    f"key_file_paths={doc['key_file_paths']}"
# 原始用例修复断言面：属主本轮消息（原始需求）经维护者固化进 key_conclusions
assert doc["key_conclusions"] == ["属主需求：创建 hello.txt，内容必须是 Hello"], \
    f"key_conclusions={doc['key_conclusions']}"
assert doc["review_summary"] == [], f"review_summary={doc['review_summary']}"
# 持久审计基线已推进到 4 行（维护成功后推进）
baseline = open(os.path.join(run, "planner", "audit-baseline")).read().strip()
assert baseline == "4", f"audit-baseline={baseline}"
# maintain llm-calls 记录：user 载荷含"本轮探查读过"清单 = 2 条 allow read 路径，
# 无 deny 路径 state.json、无 deny 语义（不可知）
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
maintain = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "maintain"]
rec = json.load(open(os.path.join(run, "llm-calls", maintain[0])))
user = rec["messages"][-1]["content"]
assert "/tmp/r6b-h-src/main.rs" in user and "/tmp/r6b-h-src/README.md" in user, \
    f"maintain prompt missing allow-read paths: {user}"
# 属主本轮消息原文进维护者载荷（数据流缺口修复——需求进记忆）
assert "属主本轮消息" in user, f"maintain prompt missing owner-message marker: {user}"
assert "Create a file named hello.txt" in user, \
    f"maintain prompt missing owner message verbatim: {user}"
assert "state.json" not in user, f"maintain prompt leaked deny path: {user}"
assert "reply.txt" not in user, f"maintain prompt leaked write path: {user}"
PY
  echo "PASS(caseH1): ConverseDone 维护轮1（allow read 路径 + 属主消息进 key_conclusions，deny/write 过滤）"
  # H2 轮2：feed revise 续入（重规划）→ ConverseDone 维护（滚动累积：轮1的
  # key_file_paths 保留 + key_conclusions 新增；审计基线后新增 1 条 allow read）
  cat >> "$AGT_DIR/audit.jsonl" <<'JSONL'
{"ts":"2026-09-01T00:01:00Z","tool_name":"read","path":"/tmp/r6b-h-src/Cargo.toml","decision":"allow","rule":null,"reason":"default_action=allow"}
JSONL
  # H2 维护者离线注入（维护者 LLM 一轮产出：滚动累积——轮1的属主需求结论保留 +
  # key_conclusions 新增属主答复；review_summary 吸收伪装反馈）
  cat > "$CASE_H/maintained-2.json" <<'JSON'
{
  "key_file_paths": ["/tmp/r6b-h-src/main.rs", "/tmp/r6b-h-src/README.md", "/tmp/r6b-h-src/Cargo.toml"],
  "key_conclusions": ["属主需求：创建 hello.txt，内容必须是 Hello", "技术选型确认：用 Rust"],
  "owner_feedback": ["我重新看了下需求，你给的方案跟我要的不太对。你再按我原来的需求重新弄一版，别想当然。"]
}
JSON
  cat > "$CASE_H/answer.txt" <<'TXT'
可以，技术选型用 Rust。
TXT
  # 不忠实计划（空 workspace_subdirs → 计划审查结构闸门拒绝 → PlanReviewed 维护）
  cat > "$CASE_H/plan-unfaithful.json" <<'JSON'
{
  "request_id": "req-r6b-h1",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": []
      }
    }
  ]
}
JSON
  # H2 注入**不忠实计划**（空 workspace_subdirs）：converse 落定 → ConverseDone
  # 维护（滚动累积）→ 计划审查结构闸门 pass=false → PlanReviewed 维护（审查理由
  # disguise 投影进 review_summary）→ PlanRejected 挂起。一步覆盖两个触发点。
  echo "[r6b] caseH2: driver feed revise（重规划 → ConverseDone 维护 + 审查拒绝 → PlanReviewed 维护 → PlanRejected） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_H/plan-unfaithful.json" \
  ALFRED_MAINTAIN_OFFLINE_FILE="$CASE_H/maintained-2.json" \
  cargo run --quiet -p alfred-cli --bin alfred -- feed \
    --run-dir "$CASE_H" \
    --decision revise \
    --message "$CASE_H/answer.txt"

  python3 - "$CASE_H" <<'PY' || { echo "FAIL(caseH2): 滚动维护+PlanReviewed 断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
doc = state["session_doc"]
# 审查拒绝（空 workspace_subdirs 结构闸门）→ PlanRejected 挂起
assert state["state_machine"]["state"] == "plan_rejected", \
    f"state={state['state_machine']['state']}"
# ConverseDone 滚动累积：轮1 路径保留 + 轮2 新增 Cargo.toml；key_conclusions 固化属主答复
assert sorted(doc["key_file_paths"]) == [
    "/tmp/r6b-h-src/Cargo.toml", "/tmp/r6b-h-src/README.md", "/tmp/r6b-h-src/main.rs"], \
    f"key_file_paths={doc['key_file_paths']}"
# ConverseDone 滚动累积（含 owner_message 数据流）：轮1 属主需求结论保留 + 轮2
# 属主答复新增（滚动语义——原始需求不再丢，下轮规划器有需求基线）
assert doc["key_conclusions"] == [
    "属主需求：创建 hello.txt，内容必须是 Hello", "技术选型确认：用 Rust"], \
    f"key_conclusions={doc['key_conclusions']}"
# PlanReviewed 维护：review_summary 落伪装转写（属主口吻，无禁词）
assert doc["review_summary"], f"review_summary empty: {doc}"
for entry in doc["review_summary"]:
    forbidden = ["reject", "verdict", "review", "reviewer", "scorer", "score",
                 "grader", "eval", "unscored", "审查", "评审", "评分", "评估",
                 "打分", "否决", "打回", "判定"]
    hits = [w for w in forbidden if w in entry.lower()]
    assert not hits, f"review_summary 含禁词 {hits}: {entry}"
# 基线推进到 5 行
baseline = open(os.path.join(run, "planner", "audit-baseline")).read().strip()
assert baseline == "5", f"audit-baseline={baseline}"
# 轮内顺序 = converse 先、维护后：本轮 converse prompt 应含**上一轮**维护的
# key_file_paths（滚动语义——轮1的 main.rs/README.md 在轮2 converse 可见）；
# 本轮维护新增的 Cargo.toml 落在 converse 之后（上方 state.json 断言已覆盖）。
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
converse = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "converse"]
last = json.load(open(os.path.join(run, "llm-calls", converse[-1])))
user = last["messages"][-1]["content"]
assert "/tmp/r6b-h-src/main.rs" in user and "/tmp/r6b-h-src/README.md" in user, \
    "下轮 converse 未看到上一轮维护的 key_file_paths（滚动语义破坏）"
assert "owner_feedback" in user, "投影缺 owner_feedback 字段名"
assert "review_summary" not in user, "converse 输入泄露磁盘字段名 review_summary"
# PlanReviewed 轮的维护者 user 载荷 = 伪装后的属主口吻反馈。禁词标准与 r6b
# caseF 伪装断言一致（结构化否决信号——disguise_rejection 的中和面）；A2 起
# 计划 schema 技术词（workspace_subdirs/执行者/节点/可见范围）同入禁词面：
# 结构闸门 reason 源头去字段名化，伪装通道技术词一并中和，两产物同一标准。
maintain = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "maintain"
            and "属主对本轮方案的反馈" in json.load(open(os.path.join(run, "llm-calls", r)))["messages"][-1]["content"]]
assert maintain, "无 PlanReviewed 载荷的 maintain 记录"
rec = json.load(open(os.path.join(run, "llm-calls", maintain[-1])))
user = rec["messages"][-1]["content"]
assert "属主对本轮方案的反馈" in user, f"PlanReviewed 载荷缺失: {user}"
# ConverseDone 轮的维护者载荷必须带属主本轮消息原文（数据流缺口修复断言面）：
converse_maintains = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "maintain"
            and "属主本轮消息" in json.load(open(os.path.join(run, "llm-calls", r)))["messages"][-1]["content"]]
assert converse_maintains, "ConverseDone 维护载荷缺属主本轮消息段"
cm = json.load(open(os.path.join(run, "llm-calls", converse_maintains[-1])))
cm_user = cm["messages"][-1]["content"]
assert "可以，技术选型用 Rust" in cm_user, \
    f"ConverseDone 维护载荷缺属主答复原文: {cm_user}"
forbidden = ["verdict", "review", "reviewer", "workspace_subdirs", "执行者", "节点", "可见范围",
             "审查", "评审", "否决", "打回", "评分"]
hits = [w for w in forbidden if w in user.lower()]
assert not hits, f"维护者 prompt 泄露审查语义 {hits}: {user}"
PY
  echo "PASS(caseH2): 重规划轮 ConverseDone 滚动累积 + PlanReviewed disguise 维护（零审查语义泄露）+ PlanRejected"
  # ---- Case I：A5 维护者收割 Err 降级行为覆盖（DebtInventory A5）----
  # run_maintain 收割失败 → planning_step Err → StepIntent::Escalate（
  # planning_error_escalated，escalation_source=planning）此前只有编译期同构、
  # 零行为证据。确定性触发：ALFRED_MAINTAIN_OFFLINE_FILE 指向非法会话文档
  # （非三字段合法形态）→ converse 离线成功（Reply 分支，converse_reply 已落）→
  # 维护者收割解析显式 Err → 治理降级挂起拍板（记忆坏了要可见，不悄悄放行）。
  CASE_I="$R6B_RUNS/run-r6b-maintain-err-escalate"
  rm -rf "$CASE_I"
  mkdir -p "$CASE_I"
  cat > "$CASE_I/request.json" <<'JSON'
{
  "id": "req-r6b-c9",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-01T00:00:00Z"
}
JSON
  cat > "$CASE_I/reply.txt" <<'TXT'
收到需求。技术选型确认一下：内容用 Rust 实现，可以吗？
TXT
  printf '{{{ not a session doc' > "$CASE_I/maintain-bad.json"
  echo "[r6b] caseI: driver run（离线 Reply 成功 → 维护者收割坏 JSON → planning_error_escalated） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$CASE_I/reply.txt" ALFRED_MAINTAIN_OFFLINE_FILE="$CASE_I/maintain-bad.json" cargo run --quiet -p alfred-cli --bin alfred -- run --request "$CASE_I/request.json" --run-dir "$CASE_I" --time-limit 60 --review-time-limit 60 --planner-time-limit 60

  python3 - "$CASE_I" <<'PY' || { echo "FAIL(caseI1): 维护者收割失败降级断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# 治理降级：Escalated + 来源 planning（不悄悄放行，不卡任何中间态）
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
assert state.get("escalation_source") == "planning", f"escalation_source={state.get('escalation_source')}"
# 审计序：converse 成功已落（converse_reply）→ 维护失败降级（Reply 分支 Err 臂）
events = [json.loads(l)["event"] for l in open(os.path.join(run, "audit.jsonl"))]
assert events == ["governance_started", "state_entered", "converse_reply",
                  "planning_error_escalated", "governance_paused"], f"events={events}"
# 失败可见：降级事件带收割解析失败原因（记忆坏了要可见）
esc = [json.loads(l) for l in open(os.path.join(run, "audit.jsonl"))
       if json.loads(l)["event"] == "planning_error_escalated"]
assert "not valid SessionDoc" in esc[0]["data"]["error"], f"error={esc[0]['data']}"
# Reply 分支：失败发生在维护环节，不产计划
assert not os.path.exists(os.path.join(run, "dagspec.json")), "Reply 分支不应产 dagspec"
PY
  echo "PASS(caseI1): 维护者收割坏 JSON → planning_error_escalated（source=planning，失败原因可见）"

  # 恢复拍板：修复注入（合法三字段文档）→ feed retry → Escalated(planning) 路由回
  # Planning → converse 重答 + 维护成功（maintain_done）→ 对话继续，环不死锁。
  cat > "$CASE_I/maintained.json" <<'JSON'
{
  "key_file_paths": [],
  "key_conclusions": ["技术选型确认：用 Rust"],
  "owner_feedback": []
}
JSON
  echo "[r6b] caseI: driver feed retry（修复维护注入 → 拍板恢复 → maintain_done → Planning） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$CASE_I/reply.txt" ALFRED_MAINTAIN_OFFLINE_FILE="$CASE_I/maintained.json" cargo run --quiet -p alfred-cli --bin alfred -- feed --run-dir "$CASE_I" --decision retry

  python3 - "$CASE_I" <<'PY' || { echo "FAIL(caseI2): 降级恢复断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
# 恢复：retry 拍板 → 重规划轮 converse 重答停驻 Planning（对话继续，非再降级）
assert state["state_machine"]["state"] == "planning", f"state={state['state_machine']['state']}"
# 维护成功：修复后的注入被收割（key_conclusions 固化）
assert state["session_doc"]["key_conclusions"] == ["技术选型确认：用 Rust"], \
    f"key_conclusions={state['session_doc']['key_conclusions']}"
events = [json.loads(l)["event"] for l in open(os.path.join(run, "audit.jsonl"))]
assert events[-5:] == ["feed_owner_message", "state_entered", "converse_reply",
                       "maintain_done", "governance_paused"], f"events={events}"
# 维护 llm-calls 记录照落（role=maintain，offline 直通）
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
maintain = [r for r in recs
            if json.load(open(os.path.join(run, "llm-calls", r)))["role"] == "maintain"]
assert maintain, f"no maintain llm-call record: {recs}"
rec = json.load(open(os.path.join(run, "llm-calls", maintain[0])))
assert rec["offline"] is True and rec["transport"] == "offline", \
    f"offline={rec['offline']} transport={rec['transport']}"
PY
  echo "PASS(caseI2): 修复注入 → feed retry 恢复（maintain_done + 对话回 Planning，环不死锁）"

  unset ALFRED_CONFIG

  echo ""
  echo "R6b Tier 1 全部通过：离线规划回归 PASS"
fi

# --- Tier 2：真宿主 converse（验方跑） ---
if [[ "${R6B_REAL:-0}" == "1" ]]; then
  echo ""
  echo "============================================="
  echo "R6b Tier 2：真宿主 converse（需宿主 pi + 真模型）"
  echo "============================================="
  command -v pi >/dev/null || { echo "FAIL(tier2): 宿主 pi 不在 PATH" >&2; exit 1; }

  CASE_T="$R6B_RUNS/run-r6b-real-converse"
  rm -rf "$CASE_T"
  mkdir -p "$CASE_T"
  cat > "$CASE_T/request.json" <<'JSON'
{
  "id": "req-r6b-real",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON
  echo "[r6b] caseT: driver run（真宿主 planner converse → 真计划审查/执行） ..."
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE_T/request.json" \
    --run-dir "$CASE_T" \
    --time-limit 900 \
    --review-time-limit 300 \
    --planner-time-limit 900

  python3 - "$CASE_T" <<'PY' || { echo "FAIL(caseT): 真宿主 converse 未产合法 DagSpec" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "completed", f"state={state['state_machine']['state']}"
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
rec = json.load(open(os.path.join(run, "llm-calls", recs[0])))
assert rec["transport"] == "host_pi", f"transport={rec['transport']}"
assert rec["offline"] is False
# 宿主驱动产物面：pi-config（模型单源投影）与 outputs 目录存在
assert os.path.isfile(os.path.join(run, "planner/pi-config/agent/models.json")), "planner pi-config models.json 未生成"
assert os.path.isdir(os.path.join(run, "planner/outputs")), "planner outputs/ 未创建"
# AGT 审计存在（扩展加载 + 拦截决策落盘）
audit = os.path.join(run, "planner/agt/audit/audit.jsonl")
assert os.path.isfile(audit), "AGT 审计 audit.jsonl 未落盘"
PY
  echo "PASS(caseT): 真宿主 converse 产合法 DagSpec → 全环 Completed（pi-config/outputs/AGT 审计齐全）"
fi

echo ""
echo "============================================="
echo "R6b e2e 完成"
echo "  Tier 0 : cargo test 全绿（离线单测 + 宿主驱动单测）"
if [[ -n "$INSPECT" ]]; then
  echo "  Tier 1 : 离线规划回归 PASS"
fi
echo "  Tier 2 : ${R6B_REAL:-0}（R6B_REAL=1 时真宿主 converse + AGT 审计实测）"
echo "============================================="
exit 0
