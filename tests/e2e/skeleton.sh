#!/usr/bin/env bash
set -euo pipefail
# skeleton.sh：S2 真容器执行（execute_in_container）的 e2e 四类用例。
# 离线模式（ALFRED_OFFLINE=1）跑通治理环全路径，不依赖 LLM / Docker。
# 负路径用 ALFRED_INJECT_EXEC_VERDICT / ALFRED_INJECT_PLAN_VERDICT 注入确定性裁决。
# 黑盒：只驱动 CLI（run / status / decide），验证 state.json + audit.jsonl + verdicts.jsonl。

export ALFRED_OFFLINE=1

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO_ROOT/target/debug/alfred"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

pass=0
fail=0

check() {
    local name="$1"; shift
    if "$@"; then
        echo "PASS: $name"
        pass=$((pass + 1))
    else
        echo "FAIL: $name"
        fail=$((fail + 1))
    fi
}

# 复合断言：接收一个命令字符串，在子 shell 中执行；用于含管道的检查。
check_eval() {
    local name="$1"; shift
    if eval "$@"; then
        echo "PASS: $name"
        pass=$((pass + 1))
    else
        echo "FAIL: $name"
        fail=$((fail + 1))
    fi
}

expect_exit() {
    local expected="$1"; shift
    local actual=0
    "$@" >/dev/null 2>&1 || actual=$?
    [ "$actual" -eq "$expected" ]
}

json_field() {
    python3 - "$1" "$2" <<'PYEOF'
import json, sys
doc = json.load(open(sys.argv[1]))
cursor = doc
for key in sys.argv[2].split('.'):
    cursor = cursor[int(key)] if key.isdigit() else cursor[key]
print(json.dumps(cursor))
PYEOF
}

# 恢复加载被 run 打回/升级后的 state.json 并返回指定字段
state_field() {
    python3 -c "import json; print(json.load(open('$1'))['state'])"
}

audit_last_to() {
    python3 - "$1" <<'PYEOF'
import sys, json
lines = [l for l in open(sys.argv[1]) if l.strip()]
if not lines:
    print("")
else:
    print(json.loads(lines[-1])['to_state'])
PYEOF
}

audit_has_transition() {
    python3 - "$1" "$2" "$3" "$4" <<'PYEOF'
import sys, json
want_event, want_from, want_to = sys.argv[2], sys.argv[3], sys.argv[4]
found = False
for l in open(sys.argv[1]):
    if not l.strip():
        continue
    ev = json.loads(l)
    if (ev['event'] == want_event and ev['from_state'] == want_from
            and ev['to_state'] == want_to):
        found = True
        break
sys.exit(0 if found else 1)
PYEOF
}

audit_count() {
    python3 -c "print(sum(1 for l in open('$1') if l.strip()))"
}

verdict_count() {
    python3 -c "print(sum(1 for l in open('$1') if l.strip()))"
}

verdict_types() {
    python3 -c "
import json
types = []
for l in open('$1'):
    if l.strip(): types.append(json.loads(l)['type'])
print(' '.join(types))
"
}

status_output() {
    "$BIN" status "$1" 2>/dev/null
}

# ────────────────────────────────────────────────────────────
echo "--- S2 e2e: alfred skeleton (black-box, via CLI only) ---"
echo "binary: $BIN"

# ═══════════════════════════════════════════════════════════
# 用例一：正路径——正常需求 → Completed
# 验证：exit 0、status 打印 completed、verdicts 有 plan+exec 两条、
#        audit 四次转移 planning→plan_reviewing→executing→exec_reviewing→completed
# ═══════════════════════════════════════════════════════════
cat > "$WORK_DIR/positive.json" <<'EOF'
{
  "request_id": "skel-positive",
  "requirement": "Create hello.txt containing Hello Alfred",
  "acceptance_criteria": "hello.txt exists with content Hello Alfred",
  "dag_spec": {
    "name": "greeting",
    "version": 1,
    "entrypoint": "write-greeting",
    "nodes": [
      {
        "node_id": "write-greeting",
        "node_type": "step",
        "contract": {
          "prompt": "Write Hello Alfred into hello.txt",
          "acceptance_criteria": "hello.txt exists with content Hello Alfred",
          "reviewer_models": ["judge-a"]
        }
      }
    ],
    "edges": []
  }
}
EOF

RUN_POS="$WORK_DIR/run-positive"
check "positive run exits 0" expect_exit 0 "$BIN" run "$WORK_DIR/positive.json" --out-dir "$RUN_POS"
check "positive state is completed" [ "$(state_field "$RUN_POS/state.json")" = "completed" ]
check_eval "positive status prints completed" "status_output '$RUN_POS' | grep -q '^state: completed\$'"
check "positive has 2 verdicts (plan+exec)" [ "$(verdict_count "$RUN_POS/verdicts.jsonl")" = "2" ]
check "positive verdict types are plan exec" [ "$(verdict_types "$RUN_POS/verdicts.jsonl")" = "plan exec" ]
check "positive audit has 4 transitions" [ "$(audit_count "$RUN_POS/audit.jsonl")" = "4" ]
check "positive audit has exec_reviewed->completed" audit_has_transition "$RUN_POS/audit.jsonl" exec_reviewed exec_reviewing completed

# ═══════════════════════════════════════════════════════════
# 用例二：契约注定失败→Escalated→decide abandon→Completed
# 注入 contract_fault 裁裁决（模拟"在 /proc 下写文件"这类不可能满足的契约）。
# 验证：run exit 1、state escalated、status 打印 escalated、
#        decide abandon → exit 0、state completed
# ═══════════════════════════════════════════════════════════
cat > "$WORK_DIR/contract-fail.json" <<'EOF'
{
  "request_id": "skel-contract-fail",
  "requirement": "Create a file in /proc/impossible",
  "acceptance_criteria": "file exists at /proc/impossible",
  "dag_spec": {
    "name": "impossible",
    "version": 1,
    "entrypoint": "write-impossible",
    "nodes": [
      {
        "node_id": "write-impossible",
        "node_type": "step",
        "contract": {
          "prompt": "Create a file at /proc/impossible (cannot be done)",
          "acceptance_criteria": "file exists at /proc/impossible",
          "reviewer_models": ["judge-a"]
        }
      }
    ],
    "edges": []
  }
}
EOF

RUN_CF="$WORK_DIR/run-contract-fail"
check_eval "contract-fail run exits 1" "ALFRED_INJECT_EXEC_VERDICT=contract_fault expect_exit 1 \"\$BIN\" run \"\$WORK_DIR/contract-fail.json\" --out-dir \"\$RUN_CF\""
check "contract-fail state is escalated" [ "$(state_field "$RUN_CF/state.json")" = "escalated" ]
check_eval "contract-fail status prints escalated" "status_output '$RUN_CF' | grep -q '^state: escalated\$'"
check "contract-fail audit ends at escalated" audit_has_transition "$RUN_CF/audit.jsonl" exec_reviewed exec_reviewing escalated
check "contract-fail has plan+exec verdicts" [ "$(verdict_count "$RUN_CF/verdicts.jsonl")" = "2" ]
# decide abandon → Completed
check "decide abandon exits 0" expect_exit 0 "$BIN" decide "$RUN_CF" abandon
check "after abandon state is completed" [ "$(state_field "$RUN_CF/state.json")" = "completed" ]
check "after abandon audit has owner_decided->completed" audit_has_transition "$RUN_CF/audit.jsonl" owner_decided escalated completed

# ═══════════════════════════════════════════════════════════
# 用例三：计划打回→PlanRejected→decide retry→Planning
# 注入 plan reject（模拟 reviewer 判计划不忠实于属主需求）。
# 验证：run exit 1、state plan_rejected、dagspec 不落地、
#        decide retry → exit 0、state planning
# ═══════════════════════════════════════════════════════════
cat > "$WORK_DIR/plan-reject.json" <<'EOF'
{
  "request_id": "skel-plan-reject",
  "requirement": "Do something vague without clear acceptance",
  "acceptance_criteria": "vague thing done",
  "dag_spec": {
    "name": "vague",
    "version": 1,
    "entrypoint": "vague-step",
    "nodes": [
      {
        "node_id": "vague-step",
        "node_type": "step",
        "contract": {
          "prompt": "Do a vague thing",
          "acceptance_criteria": "vague thing done",
          "reviewer_models": ["judge-a"]
        }
      }
    ],
    "edges": []
  }
}
EOF

RUN_PR="$WORK_DIR/run-plan-reject"
check_eval "plan-reject run exits 1" "ALFRED_INJECT_PLAN_VERDICT=reject expect_exit 1 \"\$BIN\" run \"\$WORK_DIR/plan-reject.json\" --out-dir \"\$RUN_PR\""
check "plan-reject state is plan_rejected" [ "$(state_field "$RUN_PR/state.json")" = "plan_rejected" ]
check_eval "plan-reject status prints plan_rejected" "status_output '$RUN_PR' | grep -q '^state: plan_rejected\$'"
# 计划被拒：dagspec 不落地
check "plan-reject dagspec not written" test ! -f "$RUN_PR/dagspec.json"
check "plan-reject audit has plan_reviewed->plan_rejected" audit_has_transition "$RUN_PR/audit.jsonl" plan_reviewed plan_reviewing plan_rejected
check "plan-reject has only plan verdict" [ "$(verdict_count "$RUN_PR/verdicts.jsonl")" = "1" ]
check "plan-reject verdict is plan" [ "$(verdict_types "$RUN_PR/verdicts.jsonl")" = "plan" ]
# decide retry → Planning
check "decide retry exits 0" expect_exit 0 "$BIN" decide "$RUN_PR" retry
check "after retry state is planning" [ "$(state_field "$RUN_PR/state.json")" = "planning" ]
check "after retry audit has owner_decided->planning" audit_has_transition "$RUN_PR/audit.jsonl" owner_decided plan_rejected planning

# ═══════════════════════════════════════════════════════════
# 用例四：decide 三选一——挂起态分别验证 retry / revise-contract / abandon
# 用 contract_fault 注入使 run 到 escalated，然后三组分别验证 decide 转移。
# 同时覆盖 plan_rejected 三选一，共六组 decide 路径。
# ═══════════════════════════════════════════════════════════

# --- 4a: decide retry on escalated → Executing ---
RUN_DEC_RETRY="$WORK_DIR/run-dec-retry"
ALFRED_INJECT_EXEC_VERDICT=contract_fault "$BIN" run "$WORK_DIR/contract-fail.json" --out-dir "$RUN_DEC_RETRY" >/dev/null 2>&1 || true
check "decide-retry setup is escalated" [ "$(state_field "$RUN_DEC_RETRY/state.json")" = "escalated" ]
check "decide retry on escalated exits 0" expect_exit 0 "$BIN" decide "$RUN_DEC_RETRY" retry
check "decide retry → executing" [ "$(state_field "$RUN_DEC_RETRY/state.json")" = "executing" ]
check "decide retry audit transition" audit_has_transition "$RUN_DEC_RETRY/audit.jsonl" owner_decided escalated executing

# --- 4b: decide revise-contract on escalated → Planning ---
RUN_DEC_REVISE="$WORK_DIR/run-dec-revise"
ALFRED_INJECT_EXEC_VERDICT=contract_fault "$BIN" run "$WORK_DIR/contract-fail.json" --out-dir "$RUN_DEC_REVISE" >/dev/null 2>&1 || true
check "decide-revise setup is escalated" [ "$(state_field "$RUN_DEC_REVISE/state.json")" = "escalated" ]
check "decide revise-contract on escalated exits 0" expect_exit 0 "$BIN" decide "$RUN_DEC_REVISE" revise-contract
check "decide revise-contract → planning" [ "$(state_field "$RUN_DEC_REVISE/state.json")" = "planning" ]
check "decide revise-contract audit transition" audit_has_transition "$RUN_DEC_REVISE/audit.jsonl" owner_decided escalated planning

# --- 4c: decide abandon on escalated → Completed ---
RUN_DEC_ABANDON="$WORK_DIR/run-dec-abandon"
ALFRED_INJECT_EXEC_VERDICT=contract_fault "$BIN" run "$WORK_DIR/contract-fail.json" --out-dir "$RUN_DEC_ABANDON" >/dev/null 2>&1 || true
check "decide-abandon setup is escalated" [ "$(state_field "$RUN_DEC_ABANDON/state.json")" = "escalated" ]
check "decide abandon on escalated exits 0" expect_exit 0 "$BIN" decide "$RUN_DEC_ABANDON" abandon
check "decide abandon → completed" [ "$(state_field "$RUN_DEC_ABANDON/state.json")" = "completed" ]
check "decide abandon audit transition" audit_has_transition "$RUN_DEC_ABANDON/audit.jsonl" owner_decided escalated completed

# --- 4d: decide retry on plan_rejected → Planning (covering plan_rejected 三选一) ---
RUN_DEC_PR_RETRY="$WORK_DIR/run-dec-pr-retry"
ALFRED_INJECT_PLAN_VERDICT=reject "$BIN" run "$WORK_DIR/plan-reject.json" --out-dir "$RUN_DEC_PR_RETRY" >/dev/null 2>&1 || true
check "decide-pr-retry setup is plan_rejected" [ "$(state_field "$RUN_DEC_PR_RETRY/state.json")" = "plan_rejected" ]
check "decide retry on plan_rejected exits 0" expect_exit 0 "$BIN" decide "$RUN_DEC_PR_RETRY" retry
check "decide retry on plan_rejected → planning" [ "$(state_field "$RUN_DEC_PR_RETRY/state.json")" = "planning" ]

# --- 4e: decide revise-contract on plan_rejected → Planning ---
RUN_DEC_PR_REVISE="$WORK_DIR/run-dec-pr-revise"
ALFRED_INJECT_PLAN_VERDICT=reject "$BIN" run "$WORK_DIR/plan-reject.json" --out-dir "$RUN_DEC_PR_REVISE" >/dev/null 2>&1 || true
check "decide-pr-revise setup is plan_rejected" [ "$(state_field "$RUN_DEC_PR_REVISE/state.json")" = "plan_rejected" ]
check "decide revise-contract on plan_rejected exits 0" expect_exit 0 "$BIN" decide "$RUN_DEC_PR_REVISE" revise-contract
check "decide revise-contract on plan_rejected → planning" [ "$(state_field "$RUN_DEC_PR_REVISE/state.json")" = "planning" ]

# --- 4f: decide abandon on plan_rejected → Completed ---
RUN_DEC_PR_ABANDON="$WORK_DIR/run-dec-pr-abandon"
ALFRED_INJECT_PLAN_VERDICT=reject "$BIN" run "$WORK_DIR/plan-reject.json" --out-dir "$RUN_DEC_PR_ABANDON" >/dev/null 2>&1 || true
check "decide-pr-abandon setup is plan_rejected" [ "$(state_field "$RUN_DEC_PR_ABANDON/state.json")" = "plan_rejected" ]
check "decide abandon on plan_rejected exits 0" expect_exit 0 "$BIN" decide "$RUN_DEC_PR_ABANDON" abandon
check "decide abandon on plan_rejected → completed" [ "$(state_field "$RUN_DEC_PR_ABANDON/state.json")" = "completed" ]

echo ""
echo "--- result: $pass passed, $fail failed ---"
[ "$fail" -eq 0 ]
