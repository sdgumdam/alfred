#!/usr/bin/env bash
# ============================================================================
# R4 e2e：决策面板 RPC 闭环（P4/P5 块A）——真跑
#
#   1. 升级闭环（escalated → panel → 属主 abandon）：
#      离线注入忠实计划 + 执行 --time-limit 1 → 机械预算耗尽 → Escalated →
#      `echo 3 | alfred panel`（属主选 3=放弃）→ extension_ui_request/
#      extension_ui_response 结构化对 → decide abandon → Abandoned（终态）。
#   2. 打回续跑闭环（plan_rejected → panel → 属主 retry → 真重跑 → Completed）：
#      离线注入不忠实计划 → 计划审查打回 → PlanRejected →
#      `echo 1 | ALFRED_OFFLINE=1 ... alfred panel`（属主选 1=重跑）→
#      decide retry（离线注入忠实计划）→ 重规划 → 审查过 → 真容器执行 → Completed。
#
# 断言（结构化消息流，非 stdout 文本猜测）：panel-session.jsonl 里
#   - extension_ui_request {method:"select", id, options:3项}
#   - extension_ui_response {id: 同 id, value ∈ options}
#   - state.json 推进（abandoned / completed + hello.txt）
#
# 模型：面板属主会话 pi 用 ALFRED_PANEL_MODEL（默认 glm-4.7，省钱）。
# 验收：cargo test 全绿 + 本脚本两用例 PASS。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

# --- inspect CLI ---
if [[ -n "${ALFRED_INSPECT:-}" ]]; then
  INSPECT="$ALFRED_INSPECT"
elif [[ -x ".plans/r0-lab/venv/bin/inspect" ]]; then
  INSPECT="$REPO_ROOT/.plans/r0-lab/venv/bin/inspect"
else
  INSPECT="$(command -v inspect || true)"
  if [[ -z "$INSPECT" ]]; then
    echo "ERROR: no inspect CLI. Set ALFRED_INSPECT=<venv>/bin/inspect" >&2
    exit 1
  fi
fi
export ALFRED_INSPECT="$INSPECT"
echo "[r4] inspect CLI : $INSPECT"

# --- pi CLI（面板属主会话宿主直连）---
if ! command -v pi >/dev/null 2>&1; then
  echo "ERROR: no pi CLI (needed for alfred panel). Install pi-coding-agent." >&2
  exit 1
fi
echo "[r4] pi CLI      : $(command -v pi)"

# --- 沙箱镜像（case 2 执行需要）---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    echo "[r4] tag r0-lab-pi:latest -> $IMAGE"
    docker tag r0-lab-pi:latest "$IMAGE"
  else
    echo "[r4] build $IMAGE ..."
    docker build -t "$IMAGE" -f docker/Dockerfile docker/
  fi
fi

# --- 模型（glm-4.7 省钱；面板属主会话单独 ALFRED_PANEL_MODEL）---
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-5.2}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-4.7}"
export ALFRED_PLANNER_MODEL="${ALFRED_PLANNER_MODEL:-glm-4.7}"
export ALFRED_PANEL_MODEL="${ALFRED_PANEL_MODEL:-glm-4.7}"

# --- cargo build + test ---
echo "[r4] cargo build ..."
cargo build --quiet
echo "[r4] cargo test ..."
cargo test --quiet

R4_RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$R4_RUNS"
EXEC_TL="${R4_EXEC_TIME_LIMIT:-300}"
REVIEW_TL="${R4_REVIEW_TIME_LIMIT:-300}"
PANEL_TL="${R4_PANEL_TIME_LIMIT:-180}"

# 通用 Python 断言：读 state.json 的状态
assert_state() { # <run_dir> <expected_state>
  python3 - "$1" "$2" <<'PY' || { echo "FAIL: state != $2" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1] + "/state.json"))
st = d["state_machine"]["state"]
assert st == sys.argv[2], f"state={st}, expected {sys.argv[2]}"
PY
}

# 结构化断言：panel-session.jsonl 里 extension_ui_request(method=select,id,options=3项)
#                + extension_ui_response(同 id, value ∈ options) + 决策落盘
assert_panel_session() { # <run_dir> <expected_option>
  python3 - "$1" "$2" <<'PY' || { echo "FAIL: panel-session structured assertion" >&2; exit 1; }
import json, sys
run_dir, expected_option = sys.argv[1], sys.argv[2]
lines = [json.loads(l) for l in open(run_dir + "/panel-session.jsonl")]
reqs = [l["msg"] for l in lines
        if l.get("dir") == "recv" and l.get("msg", {}).get("type") == "extension_ui_request"
        and l["msg"].get("method") == "select"]
resps = [l["msg"] for l in lines
         if l.get("dir") == "send" and l.get("msg", {}).get("type") == "extension_ui_response"]
assert reqs, f"no extension_ui_request(select) in panel-session.jsonl: {lines}"
req = reqs[0]
assert req["method"] == "select", f"method != select: {req}"
assert req.get("id"), f"missing id: {req}"
assert set(req["options"]) == {"重跑", "改契约", "放弃"}, f"options != 3项: {req['options']}"
assert resps, f"no extension_ui_response in panel-session.jsonl: {lines}"
resp = resps[0]
assert resp["id"] == req["id"], f"id mismatch: resp {resp['id']} != req {req['id']}"
assert resp["value"] in req["options"], f"value not in options: {resp['value']}"
assert resp["value"] == expected_option, f"value {resp['value']} != expected {expected_option}"
# 决策落盘
dec = json.load(open(run_dir + "/panel-decision.json"))
assert dec["option"] == expected_option, f"panel-decision option {dec.get('option')} != {expected_option}"
PY
}

# ============================================================================
# Case 1：升级闭环（Escalated → panel abandon → Abandoned）
# ============================================================================
CASE1_DIR="$R4_RUNS/run-r4-case1"
rm -rf "$CASE1_DIR"
mkdir -p "$CASE1_DIR"
cat > "$CASE1_DIR/request.json" <<'JSON'
{
  "id": "req-r4-c1",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
cat > "$CASE1_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r4-c1",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      }
    }
  ]
}
JSON
echo "[r4] case1: alfred run（执行 --time-limit 1 强制机械超时 → Escalated） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE1_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE1_DIR/request.json" \
  --run-dir "$CASE1_DIR" \
  --time-limit 1 \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"
assert_state "$CASE1_DIR" "escalated"
echo "PASS(case1a): 机械预算耗尽 → Escalated"

echo "[r4] case1: alfred panel（属主选 3=放弃） ..."
echo "3" | cargo run --quiet -p alfred-cli -- panel \
  --run-dir "$CASE1_DIR" \
  --timeout "$PANEL_TL"
assert_state "$CASE1_DIR" "abandoned"
assert_panel_session "$CASE1_DIR" "放弃"
echo "PASS(case1): 升级闭环 → panel 结构化决策卡 → decide abandon → Abandoned"

# ============================================================================
# Case 2：打回续跑闭环（PlanRejected → panel retry → 真重跑 → Completed）
# ============================================================================
CASE2_DIR="$R4_RUNS/run-r4-case2"
rm -rf "$CASE2_DIR"
mkdir -p "$CASE2_DIR"
cat > "$CASE2_DIR/request.json" <<'JSON'
{
  "id": "req-r4-c2",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
cat > "$CASE2_DIR/plan-unfaithful.json" <<'JSON'
{
  "request_id": "req-r4-c2",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create world.txt with content World",
      "contract": {
        "prompt": "Create a file named world.txt in the workspace. Its content must be exactly: World",
        "acceptance_criteria": "world.txt exists in the workspace and its content is exactly 'World'",
        "reviewer_models": []
      }
    }
  ]
}
JSON
cat > "$CASE2_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r4-c2",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
        "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
        "reviewer_models": []
      }
    }
  ]
}
JSON
echo "[r4] case2: alfred run（离线注入不忠实计划 → 计划审查打回 → PlanRejected） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE2_DIR/plan-unfaithful.json" \
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE2_DIR/request.json" \
  --run-dir "$CASE2_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"
assert_state "$CASE2_DIR" "plan_rejected"
echo "PASS(case2a): 不忠实计划被计划审查打回 → PlanRejected"

echo "[r4] case2: alfred panel（属主选 1=重跑 → decide retry 离线忠实计划重规划 → 执行） ..."
printf "1\n" | ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE2_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli -- panel \
  --run-dir "$CASE2_DIR" \
  --timeout "$PANEL_TL"
assert_state "$CASE2_DIR" "completed"
HELLO2="$CASE2_DIR/exec-1/workspace/hello.txt"
if [[ ! -f "$HELLO2" ]] || [[ "$(cat "$HELLO2")" != "Hello" ]]; then
  echo "FAIL(case2): exec-1 workspace hello.txt missing/wrong" >&2
  ls "$CASE2_DIR"/exec-*/workspace/ 2>/dev/null >&2
  exit 1
fi
assert_panel_session "$CASE2_DIR" "重跑"
echo "PASS(case2): 打回续跑闭环 → panel retry → 真重跑 → Completed"

echo ""
echo "============================================="
echo "R4 e2e 全部通过：决策面板 RPC 闭环真跑 PASS"
echo "  case1 升级闭环(abandon): $CASE1_DIR/state.json"
echo "  case2 打回续跑(retry)  : $CASE2_DIR/state.json"
echo "============================================="
exit 0
