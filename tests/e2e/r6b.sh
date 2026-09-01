#!/usr/bin/env bash
# ============================================================================
# R6b e2e：planner 容器化回归（宿主直调 → 容器 Agent）
#
# 三层：
#   Tier 0（默认，无外部依赖）：cargo test —— 离线 converse/maintain 单测 +
#     新容器驱动（compose 渲染 / 模板注入 / 输入落盘）单测。这是"离线路径全绿"
#     的核心。
#   Tier 1（需 inspect CLI，无需 docker / 无需真 LLM）：ALFRED_OFFLINE 规划 +
#     mockllm 计划审查（unscored → 升级）——断言 converse 离线产物
#     （llm-calls/dagspec.json/conversation.json）+ maintain 离线（driver feed
#     revise 更新 key_conclusions）。inspect 缺失时跳过（打印 SKIP）。
#   Tier 2（需 inspect + docker + 真模型，验方跑）：R6B_REAL=1 时真容器 converse
#     产合法 DagSpec（zhipu 真跑）+ ws 只读断言。默认关闭（留给验方）。
#
# 模型：Tier 1 用 mockllm（inspect 内建，无需 key）做计划审查——planner 离线
#   直通、executor 不触发，因此不需要 docker 与真实 provider。
# 驱动：alfred CLI 已删（08-31），黑盒经库驱动示例 `examples/driver.rs`（r6b 以
#   `cargo run --example driver -- run|feed` 驱动治理环——run 初始化 + 推进；
#   feed 喂属主消息 → `governance::feed_owner_message` 续跑）。非 CLI 子命令。
# 验收：cargo test 全绿 + Tier 1 离线回归 PASS（或 inspect 缺失 SKIP）。
# ============================================================================
#
# Tier 1 用例：
#   caseA 离线 converse（driver run → 计划 → mockllm 审查 unscored → 升级挂起）
#   caseB 离线 maintain②（driver feed revise 属主补充 → key_conclusions 更新）
#   caseC P1-2 Reply 多轮续入（规划器答复 → state=Planning → driver feed revise
#         续入属主答复 → 重规划 → 升级挂起）
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
echo "R6b Tier 0：cargo test（离线单测 + 容器驱动单测）"
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
  echo "  Tier 0 已覆盖离线 converse/maintain 单测。"
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
  cargo run --quiet -p alfred-cli --example driver -- run \
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

  # ---- Case B：离线 maintain②（driver feed revise 属主补充 → key_conclusions 更新）----
  echo "[r6b] caseB: driver feed revise（离线 maintain → key_conclusions 追加） ..."
  MSG_FILE="$CASE_A/owner-msg.txt"
  cat > "$MSG_FILE" <<'TXT'
技术选型用 Rust
TXT
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_A/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --example driver -- feed \
    --run-dir "$CASE_A" \
    --decision revise \
    --message "$MSG_FILE"

  python3 - "$CASE_A" <<'PY' || { echo "FAIL(caseB): maintain 离线更新断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert "技术选型用 Rust" in state["session_doc"]["key_conclusions"], \
    f"key_conclusions={state['session_doc']['key_conclusions']}"
assert state["owner_message"] == "技术选型用 Rust", "owner_message not updated"
# maintain 记录（llm-calls 新 seq，role=maintain, transport=offline? —— maintain 离线不落盘）
# 注：maintain 离线路径（ALFRED_OFFLINE）是确定性更新，不写 llm-calls（与 R3 一致）。
# 只断言会话文档更新成功。
PY
  # ---- Case C：P1-2 Reply 多轮续入（规划器答复 → state=Planning → decide revise 续入 → 重规划）----
  # 规划器第一轮先答复属主（不产计划，§2.4 Reply 分支）→ 状态停 Planning（对话继续）；
  # 属主经 `driver feed --decision revise --message <回答>` 从 Planning 态续入下一轮
  # 消息（设 owner_message → maintain② → planning_step 复用 revise 机制）→ 重规划
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
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$CASE_C/reply.txt" \
  cargo run --quiet -p alfred-cli --example driver -- run \
    --request "$CASE_C/request.json" \
    --run-dir "$CASE_C" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

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

  echo "[r6b] caseC: driver feed revise（Planning 态续入属主答复 → 重规划） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_C/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --example driver -- feed \
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
# maintain②：属主答复进了 key_conclusions
assert any("技术选型用 Rust" in c for c in state["session_doc"]["key_conclusions"]), \
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
  echo "PASS(caseC): Planning 态 decide revise 续入 → 重规划 → 升级挂起（多轮闭环）"

  unset ALFRED_CONFIG
  echo ""
  echo "R6b Tier 1 全部通过：离线规划回归 PASS"
fi

# --- Tier 2：真容器 converse（验方跑） ---
if [[ "${R6B_REAL:-0}" == "1" ]]; then
  echo ""
  echo "============================================="
  echo "R6b Tier 2：真容器 converse（需 docker 镜像 + 真模型）"
  echo "============================================="
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$IMAGE"
    else
      docker build -t "$IMAGE" -f docker/Dockerfile docker/
    fi
  fi
  # AGT 拦写层：planner deny-write 策略（写 /workspace 被拒、/outputs 放行）
  AGT_DIR="$R6B_RUNS/run-r6b-agt"
  rm -rf "$AGT_DIR"
  mkdir -p "$AGT_DIR"
  cp tests/e2e/agt/agt-policy.ts "$AGT_DIR/agt-policy.ts"
  cp tests/e2e/agt/planner-policy.json "$AGT_DIR/policy.json"
  export ALFRED_AGT_DIR="$AGT_DIR"

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
  echo "[r6b] caseT: driver run（真容器 planner converse → 真计划审查/执行） ..."
  cargo run --quiet -p alfred-cli --example driver -- run \
    --request "$CASE_T/request.json" \
    --run-dir "$CASE_T" \
    --time-limit 900 \
    --review-time-limit 300 \
    --planner-time-limit 900 \
    --image "$IMAGE"

  python3 - "$CASE_T" <<'PY' || { echo "FAIL(caseT): 真容器 converse 未产合法 DagSpec" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "completed", f"state={state['state_machine']['state']}"
recs = sorted(os.listdir(os.path.join(run, "llm-calls")))
rec = json.load(open(os.path.join(run, "llm-calls", recs[0])))
assert rec["transport"] == "container_bridge", f"transport={rec['transport']}"
assert rec["offline"] is False
# planner 容器工作区存在（ws 只读挂载源）
assert os.path.isdir(os.path.join(run, "ws")), "planner ws/ 未创建"
PY
  echo "PASS(caseT): 真容器 converse 产合法 DagSpec → 全环 Completed"

  echo ""
  echo "[r6b] ws 只读实测（容器内写 /workspace 被拒——ro 挂载层）："
  WS_DIR="$CASE_T/ws"
  if docker run --rm -v "$WS_DIR":/workspace:ro --network none "$IMAGE" bash -c "echo x > /workspace/probe.txt" >/dev/null 2>&1; then
    echo "FAIL(ws-ro): 容器内写 /workspace 竟然成功" >&2
    exit 1
  fi
  echo "  PASS: 容器内写 /workspace 被拒（ro 挂载）"
  unset ALFRED_AGT_DIR
fi

echo ""
echo "============================================="
echo "R6b e2e 完成"
echo "  Tier 0 : cargo test 全绿（离线单测 + 容器驱动单测）"
if [[ -n "$INSPECT" ]]; then
  echo "  Tier 1 : 离线规划回归 PASS"
fi
echo "  Tier 2 : ${R6B_REAL:-0}（R6B_REAL=1 时真容器 converse + ws 只读实测）"
echo "============================================="
exit 0
