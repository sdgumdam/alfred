#!/usr/bin/env bash
# ============================================================================
# R3 e2e：治理环闭环四用例（真跑）
#
#   1. 正路径全环：真规划（LLM 建图指令）→ 计划审查过 → 真容器执行 → 验收 C → Completed
#   2. 失败升级闭环：构造机械失败（--time-limit 1 → eval 超时）→ 同契约重跑 2 次
#      → 预算耗尽 Escalated → decide retry（--time-limit 300）→ 真重跑 → 新 verdict → Completed
#   3. 计划打回伪装闭环：离线注入不忠实计划 → 计划审查打回 → PlanRejected →
#      decide retry → 伪装消息进 planner（断言无结构化否决词）→ 重规划（离线注入忠实计划）
#      → 计划审查过 → 执行 → Completed
#   4. 多轮会话文档：打回 → 属主补充新需求（decide revise）→ converse 引用会话文档关键结论
#      （从 llm-calls/ 记录断言）
#
# 模型：默认 glm-4.7（省钱；zhipu key 经 ~/.config/alfred/config.yml 或
# ALFRED_CONFIG 提供）。可用 ALFRED_EXECUTOR_MODEL / ALFRED_REVIEWER_MODEL /
# ALFRED_PLANNER_MODEL 覆盖。
# 验收：cargo test 全绿 + 本脚本四用例 PASS。
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
echo "[r3] inspect CLI : $INSPECT"

# --- 沙箱镜像（case 1 需要）---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    echo "[r3] tag r0-lab-pi:latest -> $IMAGE"
    docker tag r0-lab-pi:latest "$IMAGE"
  else
    echo "[r3] build $IMAGE ..."
    docker build -t "$IMAGE" -f docker/Dockerfile docker/
  fi
fi

# --- 模型（glm-4.7 省钱）---
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-5.2}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-4.7}"
export ALFRED_PLANNER_MODEL="${ALFRED_PLANNER_MODEL:-glm-4.7}"

# --- cargo build + test ---
echo "[r3] cargo build ..."
cargo build --quiet
echo "[r3] cargo test ..."
cargo test --quiet

R3_RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$R3_RUNS"
EXEC_TL="${R3_EXEC_TIME_LIMIT:-300}"
REVIEW_TL="${R3_REVIEW_TIME_LIMIT:-300}"

# 通用 Python 断言：读 state.json 的状态
assert_state() { # <run_dir> <expected_state>
  python3 - "$1" "$2" <<'PY' || { echo "FAIL: state != $2" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1] + "/state.json"))
st = d["state_machine"]["state"]
assert st == sys.argv[2], f"state={st}, expected {sys.argv[2]}"
PY
}

# 找最后一个 exec-N 目录（execution_count 跨周期单调递增）
last_exec_ws() { # <run_dir>
  local run_dir="$1"
  ls -d "$run_dir"/exec-* 2>/dev/null | sort -V | tail -1 | xargs -I{} echo "{}/workspace"
}

# ============================================================================
# Case 1：正路径全环（真规划 → 审查过 → 执行 → 验收 C → Completed）
# ============================================================================
CASE1_DIR="$R3_RUNS/run-r3-case1"
rm -rf "$CASE1_DIR"
mkdir -p "$CASE1_DIR"
cat > "$CASE1_DIR/request.json" <<'JSON'
{
  "id": "req-r3-c1",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
echo "[r3] case1: alfred run（真规划 → 计划审查 → 执行 → 验收） ..."
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE1_DIR/request.json" \
  --run-dir "$CASE1_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"

assert_state "$CASE1_DIR" "completed"
# 产物 + 执行审查 C
HELLO="$CASE1_DIR/exec-1/workspace/hello.txt"
if [[ ! -f "$HELLO" ]] || [[ "$(cat "$HELLO")" != "Hello" ]]; then
  echo "FAIL(case1): workspace hello.txt missing/wrong content" >&2
  exit 1
fi
python3 - "$CASE1_DIR/state.json" <<'PY' || { echo "FAIL(case1): exec verdict not C" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1]))
vs = d["exec_verdicts"]
assert len(vs) >= 1, "no exec verdict"
v = vs[-1]
assert v["value"] == "C", f"expected C, got {v['value']}"
assert v.get("failure_class") is None
PY
# llm-calls 有 converse 记录（真 LLM）
if ! ls "$CASE1_DIR"/llm-calls/0000.json >/dev/null 2>&1; then
  echo "FAIL(case1): llm-calls/0000.json missing" >&2
  exit 1
fi
echo "PASS(case1): 正路径全环 Completed + 执行审查 C"

# ============================================================================
# Case 2：失败升级闭环（机械失败 → 重跑2次 → Escalated → decide retry → 真重跑）
#   用 --time-limit 1 让执行 eval 超时（机械失败）；计划审查用独立 --review-time-limit。
# ============================================================================
CASE2_DIR="$R3_RUNS/run-r3-case2"
rm -rf "$CASE2_DIR"
mkdir -p "$CASE2_DIR"
cat > "$CASE2_DIR/request.json" <<'JSON'
{
  "id": "req-r3-c2",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
# 离线注入忠实计划（规划器直通，省一轮 LLM）
cat > "$CASE2_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r3-c2",
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
echo "[r3] case2: alfred run（执行 --time-limit 1 强制机械超时） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE2_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE2_DIR/request.json" \
  --run-dir "$CASE2_DIR" \
  --time-limit 1 \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"

# 断言：Escalated + attempts=2 + audit 有 mechanical_retry ×2 + budget exhausted
assert_state "$CASE2_DIR" "escalated"
python3 - "$CASE2_DIR/state.json" "$CASE2_DIR/audit.jsonl" <<'PY' || { echo "FAIL(case2): 机械预算未耗尽" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1]))
assert d["attempts_used"] == 2, f"attempts={d['attempts_used']}, expected 2 (budget N=2)"
events = [json.loads(l)["event"] for l in open(sys.argv[2])]
mech = [e for e in events if e == "mechanical_retry"]
assert len(mech) >= 2, f"mechanical_retry count={len(mech)}"
assert "mechanical_budget_exhausted_escalated" in events, "budget exhausted missing"
PY
echo "PASS(case2a): 机械重跑2次 → 预算耗尽 → Escalated"

echo "[r3] case2: alfred decide retry（--time-limit 300 真重跑） ..."
cargo run --quiet -p alfred-cli -- decide \
  --run-dir "$CASE2_DIR" \
  --decision retry \
  --time-limit "$EXEC_TL" \
  --image "$IMAGE"

assert_state "$CASE2_DIR" "completed"
HELLO2="$(last_exec_ws "$CASE2_DIR")/hello.txt"
if [[ ! -f "$HELLO2" ]] || [[ "$(cat "$HELLO2")" != "Hello" ]]; then
  echo "FAIL(case2): decide retry 后 hello.txt 未产出（$(last_exec_ws "$CASE2_DIR")）" >&2
  ls "$CASE2_DIR"/exec-*/workspace/ 2>/dev/null >&2
  exit 1
fi
python3 - "$CASE2_DIR/state.json" <<'PY' || { echo "FAIL(case2): decide retry 后无新 exec verdict" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1]))
assert d["state_machine"]["state"] == "completed"
vs = d["exec_verdicts"]
assert len(vs) >= 1 and vs[-1]["value"] == "C", f"exec verdicts: {vs}"
PY
echo "PASS(case2): 失败升级闭环 → decide retry → 真重跑 → 新 verdict → Completed"

# ============================================================================
# Case 3：计划打回伪装闭环（不忠实计划 → PlanRejected → decide retry → 伪装消息 → 重规划 → 过）
# ============================================================================
CASE3_DIR="$R3_RUNS/run-r3-case3"
rm -rf "$CASE3_DIR"
mkdir -p "$CASE3_DIR"
cat > "$CASE3_DIR/request.json" <<'JSON'
{
  "id": "req-r3-c3",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
# 不忠实计划：做 world.txt（与需求 A 不符）→ 计划审查注定打回
cat > "$CASE3_DIR/plan-unfaithful.json" <<'JSON'
{
  "request_id": "req-r3-c3",
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
cat > "$CASE3_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r3-c3",
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
echo "[r3] case3: alfred run（离线注入不忠实计划 → 计划审查打回） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE3_DIR/plan-unfaithful.json" \
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE3_DIR/request.json" \
  --run-dir "$CASE3_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"

assert_state "$CASE3_DIR" "plan_rejected"
python3 - "$CASE3_DIR/audit.jsonl" <<'PY' || { echo "FAIL(case3): 计划审查未打回" >&2; exit 1; }
import json, sys
events = [json.loads(l)["event"] for l in open(sys.argv[1])]
assert "plan_review_rejected" in events, "plan_review_rejected missing"
PY
echo "PASS(case3a): 不忠实计划被计划审查打回 → PlanRejected"

echo "[r3] case3: alfred decide retry（伪装消息重规划） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE3_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli -- decide \
  --run-dir "$CASE3_DIR" \
  --decision retry \
  --image "$IMAGE"

assert_state "$CASE3_DIR" "completed"
# 断言：伪装消息无结构化否决词；llm-calls/0001.json（重规划）引用伪装消息
python3 - "$CASE3_DIR/state.json" "$CASE3_DIR/llm-calls" <<'PY' || { echo "FAIL(case3): 伪装消息含禁词或 llm 记录缺失" >&2; exit 1; }
import json, sys, glob, os
d = json.load(open(sys.argv[1]))
assert d["state_machine"]["state"] == "completed"
msg = d.get("owner_message") or ""
assert msg, "owner_message (disguised) missing"
# 禁词检查（P7）：reject/verdict/否决/review/审查/scorer/grader/eval/打回/评审/评分/评估
forbidden = ["reject", "rejected", "verdict", "review", "reviewer", "scorer", "score",
             "grader", "eval", "unscored", "否决", "审查", "评审", "评分", "评估", "打分", "打回", "判定"]
low = msg.lower()
hits = [w for w in forbidden if w in low]
assert not hits, f"disguised message contains forbidden words {hits}: {msg}"
# 重规划 converse 记录存在（0001.json：第二次 converse）
files = sorted(glob.glob(os.path.join(sys.argv[2], "*.json")))
assert len(files) >= 2, f"expect >=2 llm-calls records, got {len(files)}"
rec = json.load(open(files[-1]))
user = rec["messages"][-1]["content"]
assert "属主本轮消息" in user, "replan record missing owner message"
# 伪装消息确实喂给了 planner
assert msg in user, "disguised message not in replan prompt"
PY
echo "PASS(case3): 打回伪装 → decide retry → 伪装消息进 planner（无结构化否决词）→ 重规划 → Completed"

# ============================================================================
# Case 4：多轮会话文档（打回 → 属主补充 → converse 引用会话文档关键结论）
# ============================================================================
CASE4_DIR="$R3_RUNS/run-r3-case4"
rm -rf "$CASE4_DIR"
mkdir -p "$CASE4_DIR"
cat > "$CASE4_DIR/request.json" <<'JSON'
{
  "id": "req-r3-c4",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
cat > "$CASE4_DIR/plan-unfaithful.json" <<'JSON'
{
  "request_id": "req-r3-c4",
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
cat > "$CASE4_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r3-c4",
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
# 属主补充的新需求
cat > "$CASE4_DIR/supplement.txt" <<'TXT'
技术选型：内容必须是英文单词 Hello（大小写敏感），且文件必须位于工作区根目录。
TXT
echo "[r3] case4: alfred run（打回 → 挂起 PlanRejected） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE4_DIR/plan-unfaithful.json" \
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE4_DIR/request.json" \
  --run-dir "$CASE4_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"
assert_state "$CASE4_DIR" "plan_rejected"

echo "[r3] case4: alfred decide revise（属主补充新需求） ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE4_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli -- decide \
  --run-dir "$CASE4_DIR" \
  --decision revise \
  --message "$CASE4_DIR/supplement.txt" \
  --image "$IMAGE"

assert_state "$CASE4_DIR" "completed"
# 断言：第二次 converse 的 llm-calls 记录引用会话文档（review_summary + key_conclusions）
python3 - "$CASE4_DIR/state.json" "$CASE4_DIR/llm-calls" <<'PY' || { echo "FAIL(case4): converse 未引用会话文档关键结论" >&2; exit 1; }
import json, sys, glob, os
d = json.load(open(sys.argv[1]))
assert d["state_machine"]["state"] == "completed"
doc = d["session_doc"]
# maintain ②：属主补充进了 key_conclusions
assert any("技术选型" in c for c in doc["key_conclusions"]), f"key_conclusions missing supplement: {doc['key_conclusions']}"
# maintain ①：打回后 review_summary 非空
assert len(doc["review_summary"]) >= 1, "review_summary empty"
files = sorted(glob.glob(os.path.join(sys.argv[2], "*.json")))
assert len(files) >= 2, f"expect >=2 llm-calls, got {len(files)}"
rec = json.load(open(files[-1]))
user = rec["messages"][-1]["content"]
# 第二次 converse 的 prompt 包含维护者写入的关键结论（会话文档三段）
assert "技术选型" in user, "converse 未引用属主补充（key_conclusions）"
assert any(s[:20] in user for s in doc["review_summary"]), "converse 未引用审查摘要（review_summary）"
PY
echo "PASS(case4): 打回→属主补充→converse 引用会话文档关键结论（从 llm 调用记录断言）"

echo ""
echo "============================================="
echo "R3 e2e 全部通过：四用例真跑 PASS"
echo "  case1 正路径全环      : $CASE1_DIR/state.json"
echo "  case2 机械升级闭环    : $CASE2_DIR/state.json"
echo "  case3 打回伪装闭环    : $CASE3_DIR/state.json"
echo "  case4 多轮会话文档    : $CASE4_DIR/state.json"
echo "============================================="
exit 0
