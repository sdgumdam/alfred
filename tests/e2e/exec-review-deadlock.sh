#!/usr/bin/env bash
# ============================================================================
# exec-review-deadlock e2e：用户自由使用路径死锁复现 + 修复闭环（9/3 欠账）
#
# 用户实撞死锁链（真实 run-18d1c83accf4c04002）：
#   1. 挂起 run 恢复 → 拍板"重试" → 执行跑完 → 执行审查超时
#      （"exec review container failed: reviewer driver timed_out after 300s"）
#      → chat catch+reload → state=exec_reviewing
#   2. 再操作 → chat 报 "run 停在 exec_reviewing（执行审查中断，执行 outcome
#      不落盘无法续跑）——重新执行或人工介入" → 死锁，用户无法继续
#
# 本脚本黑盒复现整条链并断言修复后行为（确定性：mock provider 驱动宿主 pi，
# executor=mockllm 容器，无真 LLM）：
#
#   Step 1  复现：driver run（planner 离线注入忠实计划 → 计划审查宿主 pi 在线
#           （mock 驱动 write 工具循环写 verdict）→ mockllm 容器真实执行 →
#           执行审查宿主 pi 用"永不写 verdict"的 stall 模式 + --review-time-limit 1
#           强制超时）→ 断言修复后：state=escalated（非卡 exec_reviewing）+
#           escalation_source=execution + audit 含 review_host_failure_escalated
#           + exec-review/state.json 落盘 outcome（eval_status=timed_out，
#           容器时代 fail_exec_review 语义）。
#   Step 2  chat 恢复：`alfred chat --run-dir <run>` 恢复该 escalated run →
#           升级包呈现（来源 Some(Execution)）→ 属主拍板"重试" → 重入执行 →
#           执行审查（stall 模式，mock 收到 exec-review 模式请求 → 恒挂起超时）
#           → 再次治理降级 escalated → **chat 不死锁、会话可继续**（EOF 正常退出）。
#   Step 3  磁盘重入直验（修复②核心）：手工把 state.json 回写为 exec_reviewing
#           （模拟旧死锁现场/进程在执行审查中被杀）→ `alfred chat` 恢复 →
#           直接从磁盘重入执行审查（不再报"outcome 不落盘无法续跑"）→
#           拍板"放弃" → Abandoned 终态（死锁路径可继续会话到终态）。
#
# 运行：bash tests/e2e/exec-review-deadlock.sh（需 inspect + docker；缺失 SKIP）
# 验收：全 PASS + cargo test 全绿（由 skeleton.sh 汇总链覆盖）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TS="$(date +%Y%m%d-%H%M%S)"
RUNS="$REPO_ROOT/tests/e2e/.runs/exec-review-deadlock-$TS"
LOG="$RUNS/logs"
mkdir -p "$LOG"

fail() { echo "FAIL($1): $2" >&2; exit 1; }
pass() { echo "PASS($1): $2"; }

# --- inspect CLI 定位（同 r6d.sh） ---
INSPECT="${ALFRED_INSPECT:-}"
if [[ -z "$INSPECT" ]]; then
  for c in "$REPO_ROOT/.plans/r0-lab/venv/bin/inspect" "$(command -v inspect || true)"; do
    [[ -n "$c" && -x "$c" ]] && { INSPECT="$c"; break; }
  done
fi
if [[ -z "$INSPECT" ]]; then
  echo "SKIP: inspect CLI 缺失（执行审查超时复现需 mockllm 容器执行）。"
  exit 0
fi
if ! docker info >/dev/null 2>&1; then
  echo "SKIP: docker 不可用（mockllm 容器执行需要）。"
  exit 0
fi
export ALFRED_INSPECT="$INSPECT"

ALFRED() { cargo run --quiet -p alfred-cli --bin alfred -- "$@"; }

# ============================================================================
# 公共夹具：mock provider（两模式）+ request + 忠实计划 + config
# ============================================================================
FIX="$RUNS/fix"
mkdir -p "$FIX"

cat > "$FIX/request.json" <<'JSON'
{
  "id": "req-deadlock-repro",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-07T00:00:00Z"
}
JSON

# 忠实计划（planner 离线直通；计划审查必须 PASS 才走到执行/执行审查）
cat > "$FIX/plan-faithful.json" <<'JSON'
{
  "request_id": "req-deadlock-repro",
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

cat > "$FIX/config.yml" <<YAML
providers:
  mock:
    base_url: "http://127.0.0.1:%MOCK_PORT%/v1"
    api_key: "sk-mock-never-checked"
    protocol: openai-compatible
models:
  - id: mock-reviewer
    provider: mock
    maxTokens: 4096
roles:
  planner: mock-reviewer
  executor: mock-reviewer
  reviewer: mock-reviewer
YAML

# --- mock provider 端口分配 + 按 case 重启（stall 标志不同） ---
MOCK_PORT=""
find_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}
MOCK_PID=""
start_mock() { # $1 = request log, $2 = exec-review stall 标志（非空 = 挂起不写 verdict）
  local logreq="$1" stall="$2"
  MOCK_PORT="$(find_port)"
  ALFRED_MOCK_EXEC_STALL="$stall" \
  python3 tests/e2e/mock_provider.py "$MOCK_PORT" "$logreq" \
    '{"pass": true, "reason": "plan faithfully addresses the owner request"}' \
    >"$LOG/mock-$MOCK_PORT.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" \
      -H 'Content-Type: application/json' \
      -d '{"model":"mock-reviewer","messages":[{"role":"user","content":"hi"}]}' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.3
  done
  fail mock "mock provider 未就绪（port=$MOCK_PORT）"
}
stop_mock() {
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
  MOCK_PID=""
}
trap stop_mock EXIT

write_config() {
  sed "s/%MOCK_PORT%/$MOCK_PORT/" "$FIX/config.yml" > "$FIX/config-live.yml"
}

state_of() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['state_machine']['state'])" "$1/state.json"; }
src_of()   { python3 -c "import json;print(json.load(open(sys.argv[1])).get('escalation_source'))" "$1/state.json"; }

export ALFRED_CONFIG="$FIX/config-live.yml"
export ALFRED_REVIEWER_MODEL="mock-reviewer"
export ALFRED_PLANNER_OFFLINE=1
export ALFRED_OFFLINE_PLAN_FILE="$FIX/plan-faithful.json"
# 执行审查在线（走真超时路径）——不设 ALFRED_EXEC_REVIEW_OFFLINE / ALFRED_OFFLINE

# ============================================================================
# Step 1：复现用户死锁链——执行审查超时 → 治理降级 escalated（修复①）
# ============================================================================
echo "[deadlock] Step 1: driver run 到执行审查 → --review-time-limit 1 强制宿主 pi 超时 ..."
start_mock "$RUNS/mock-step1.jsonl" "1"
write_config

RUN1="$RUNS/run-step1"
rm -rf "$RUN1"; mkdir -p "$RUN1"

set +e
ALFRED run \
  --request "$FIX/request.json" \
  --run-dir "$RUN1" \
  --time-limit 300 \
  --review-time-limit 1 \
  --planner-time-limit 60 \
  --no-ctl > "$LOG/step1.out" 2>&1
RC1=$?
echo "[deadlock] Step 1 driver rc=${RC1}（0 = 治理降级后正常挂起返回）"

python3 - "$RUN1" <<'PY' || { echo "FAIL(step1): 执行审查超时治理降级断言" >&2; sed -n '1,40p' "$RUN1/../logs/step1.out" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
# 1) 修复①核心：state=escalated（用户旧版卡死在 exec_reviewing）
state = json.load(open(os.path.join(run, "state.json")))
sm = state["state_machine"]["state"]
assert sm == "escalated", f"state={sm}（期望 escalated——治理降级，不卡 exec_reviewing）"
assert state.get("escalation_source") == "execution", \
    f"escalation_source={state.get('escalation_source')}（期望 execution）"
# 2) 审计：编排层接住宿主驱动超时 → 治理降级事件
audit = open(os.path.join(run, "audit.jsonl")).read()
assert "review_host_failure_escalated" in audit, "audit 缺 review_host_failure_escalated"
assert "timed out after 1s" in audit or "timed_out" in audit or "timed out" in audit, \
    "审计降级事件应携带超时原因"
# 3) 审查 outcome 落盘（容器时代 fail_exec_review 语义：失败也留 state.json 证据）
er = os.path.join(run, "exec-review", "state.json")
assert os.path.exists(er), "exec-review/state.json 缺失（失败 outcome 应落盘）"
rec = json.load(open(er))
assert rec["exec_review"]["eval_status"] in ("error", "timed_out"), \
    f"exec_review.eval_status={rec['exec_review']['eval_status']}"
assert rec["exec_review"]["verdict"] is None, "失败 outcome 不应有 verdict"
assert rec["exec_review"].get("unscored_reason"), "失败 outcome 应带 unscored_reason"
# 4) 执行真跑过（mockllm 容器产物）：ws 里有执行者产物
assert os.path.exists(os.path.join(run, "exec-1", "state.json")), "exec-1/state.json 缺失——执行未跑"
assert os.path.exists(os.path.join(run, "exec-1", "driver.done.json")), "exec-1 driver.done.json 缺失"
# 5) 计划审查在线通过（证明走的是"计划审查过 → 执行 → 执行审查超时"用户链路）
assert os.path.exists(os.path.join(run, "plan-review", "outputs", "verdict.json")), \
    "plan-review/outputs/verdict.json 缺失——计划审查未在线通过"
PY
pass step1 "执行审查宿主 pi 超时 → 治理降级 escalated（escalation_source=execution，outcome 落盘 exec-review/state.json）——不卡 exec_reviewing"

stop_mock

# ============================================================================
# Step 2：chat 恢复 escalated run → 拍板"重试" → 重入执行 → 执行审查再次超时 →
#         再次治理降级 → 会话继续（不死锁不崩）——修复① chat 侧闭环
# ============================================================================
echo "[deadlock] Step 2: chat 恢复 escalated run → 重试 → 执行审查再超时 → 再降级 → 会话继续 ..."
start_mock "$RUNS/mock-step2.jsonl" "1"
write_config

# 属主拍板序列：升级包呈现后"重试"→ 重入执行 → 执行审查再超时 → 再降级 →
# 新升级包呈现 → 再"重试"一次（验证降级后可反复拍板）→ 放弃 → Abandoned 终态
printf '重试\n放弃\n' | ALFRED chat --run-dir "$RUN1" > "$LOG/step2.out" 2>&1
RC2=$?
[[ "$RC2" -eq 0 ]] || fail step2 "chat 会话应正常 EOF 退出（rc=${RC2}）——死锁修复后会话可继续"
[[ "$(state_of "$RUN1")" == "abandoned" ]] || fail step2 "拍板放弃后应到 abandoned（实际 $(state_of "$RUN1")）"
grep -q "已升级属主" "$LOG/step2.out" || fail step2 "chat 应呈现执行审查失败升级（升级包）"
grep -q "治理挂起，等待属主拍板" "$LOG/step2.out" || fail step2 "chat 应呈现升级包拍板提示"
if grep -q "执行 outcome 不落盘无法续跑" "$LOG/step2.out" "$LOG/step2.err" 2>/dev/null; then
  fail step2 "chat 不应再报'outcome 不落盘无法续跑'死锁文案"
fi
# conversation.json 有属主拍板轮（会话真继续了）
python3 - "$RUN1" <<'PY' || fail step2 "拍板轮应落 conversation.json"
import json, os, sys
run = sys.argv[1]
log = json.load(open(os.path.join(run, "conversation.json")))
contents = [t.get("content", "") for t in log.get("turns", [])]
# chat 拍板 = 属主轮（原始输入"重试"/"放弃"，仅首拍板落轮）+ panel.decision 轮
# （"重跑（retry）"/"放弃（abandon）"标签，feed_owner_message P3b）。
assert any(c.strip() == "重试" for c in contents), f"缺属主'重试'拍板轮: {contents}"
assert any("重跑（retry）" in c for c in contents), f"缺 panel.decision retry 轮: {contents}"
assert any("放弃（abandon）" in c for c in contents), f"缺 panel.decision abandon 轮: {contents}"
# Abandon 前置路由（P2b）不落 owner.message 轮——panel.decision 标签轮即拍板证据。
PY

pass step2 "chat 恢复 escalated → 重试（重入执行+执行审查再超时→再降级）→ 放弃 → Abandoned——会话全程可继续，零死锁"
stop_mock

# ============================================================================
# Step 3：磁盘重入直验（修复②核心）——把 state.json 回写为 exec_reviewing
#         （模拟旧死锁现场：进程在执行审查中被杀）→ chat 恢复 → 直接重入
#         执行审查 → 拍板放弃到终态。旧代码此处直接 bail 死锁。
# ============================================================================
echo "[deadlock] Step 3: 手工回写 state=exec_reviewing（模拟旧死锁现场）→ chat 磁盘重入 ..."
python3 - "$RUN1" <<'PY'
import json, os, sys
run = sys.argv[1]
p = os.path.join(run, "state.json")
s = json.load(open(p))
s["state_machine"]["state"] = "exec_reviewing"
json.dump(s, open(p, "w"), ensure_ascii=False, indent=2)
PY
[[ "$(state_of "$RUN1")" == "exec_reviewing" ]] || fail step3 "回写 exec_reviewing 失败"

start_mock "$RUNS/mock-step3.jsonl" "1"
write_config

# 重入执行审查（会再超时→治理降级 escalated）→ 拍板放弃 → Abandoned
printf '放弃\n' | ALFRED chat --run-dir "$RUN1" > "$LOG/step3.out" 2>&1
RC3=$?
[[ "$RC3" -eq 0 ]] || fail step3 "exec_reviewing 磁盘重入应可续跑（rc=${RC3}）"
[[ "$(state_of "$RUN1")" == "abandoned" ]] || fail step3 "重入→放弃后应到 abandoned（实际 $(state_of "$RUN1")）"
if grep -q "执行 outcome 不落盘无法续跑" "$LOG/step3.out" "$LOG/step3.err" 2>/dev/null; then
  fail step3 "磁盘重入不应再报'outcome 不落盘无法续跑'"
fi
grep -q "已升级属主" "$LOG/step3.out" || fail step3 "重入执行审查超时应再次治理降级（升级包）"
# 第二次执行审查证据：exec-review 侧 audit 再有宿主失败记录（时间上晚于 Step1）
python3 - "$RUN1" <<'PY' || fail step3 "exec-review 超时记录应 ≥2 次（初跑 + 重入各一次）"
import json, os, sys
run = sys.argv[1]
n = 0
with open(os.path.join(run, "exec-review", "audit.jsonl")) as f:
    for line in f:
        if "exec_review_host_failed" in line:
            n += 1
assert n >= 2, f"exec_review_host_failed 仅 {n} 次（期望 ≥2：Step1 初跑 + Step3 重入）"
PY
pass step3 "exec_reviewing 手工回写（旧死锁现场）→ chat 磁盘重入执行审查（再超时→再降级）→ 放弃 → Abandoned——修复②成立"

stop_mock
trap - EXIT

echo ""
echo "============================================="
echo "exec-review-deadlock e2e 全部通过："
echo "  Step 1: 执行审查超时 → 治理降级 escalated（不卡 exec_reviewing）"
echo "  Step 2: chat 恢复 → 重试/放弃拍板全程可继续（零死锁）"
echo "  Step 3: exec_reviewing 磁盘重入（废'outcome 不落盘'bail）→ 终态可达"
echo "  日志  : $LOG"
echo "============================================="
exit 0
