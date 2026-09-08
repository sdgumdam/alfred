#!/usr/bin/env bash
# ============================================================================
# R3 e2e：治理环全链回归（离线计划注入 / 属主决策 feed 续跑）
#
#   1. 正路径全环：driver run（离线忠实计划 → 计划审查 → 执行 → 验收 C → Completed；
#      真规划正路径全环由 r6c.sh Tier3b / r6d.sh Tier3 覆盖）
#   2. 失败升级闭环：driver run（离线忠实计划 + 执行 --time-limit 1 → 机械重跑2次 →
#      预算耗尽 → Escalated）；case2b 已归档（旧 decide retry --time-limit 覆盖续跑，
#      driver feed / feed_owner_message 用 run.options 持久配置，无覆盖参数）
#   3. 计划打回伪装闭环：离线注入不忠实计划 → 计划审查打回 → PlanRejected →
#      driver feed retry → 伪装消息进 planner（断言无结构化否决词）→ 重规划
#      （离线注入忠实计划）→ 计划审查过 → 执行 → 执行审查离线回退 → Escalated
#   4. 多轮会话文档：打回 → 属主补充新需求（driver feed revise）→ converse 引用
#      会话文档关键结论（从 llm-calls/ 记录断言）→ 执行审查离线回退 → Escalated
#
# 模型：默认 glm-5.3-flash（省钱；zhipu key 经 ~/.config/alfred/config.yml 或
# ALFRED_CONFIG 提供）。可用 ALFRED_EXECUTOR_MODEL / ALFRED_REVIEWER_MODEL /
# ALFRED_PLANNER_MODEL 覆盖。
# 驱动：黑盒经真实 `alfred` bin（codux 可调度 CLI driver：run/feed/status）驱动——
#   r3 以 `cargo run --bin alfred -- run|feed` 驱动治理环（run 初始化+推进；feed 喂
#   属主消息 → `governance::feed_owner_message` 续跑）。
# 验收：cargo test 全绿 + 本脚本用例 PASS（case2b 归档 SKIP）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
# --- 容器驱动 Python（P1）：ALFRED_PYTHON 优先，否则本仓 venv（Rust python_binary() 兜底 PATH） ---
if [[ -z "${ALFRED_PYTHON:-}" && -x "$REPO_ROOT/.plans/r0-lab/venv/bin/python" ]]; then
  export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"
fi

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

# --- 模型（glm-5.3-flash 省钱）---
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-5.3-flash}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-5.3-flash}"
export ALFRED_PLANNER_MODEL="${ALFRED_PLANNER_MODEL:-glm-5.3-flash}"

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

# R6f：产物路径解析——不硬编码 workspace_subdirs 名。真规划器按 R6fPlannerNaming
# 约束自由选具体子目录名（本机实测 ['output']，非固定 'src'）；离线注入计划也以
# 其声明的 workspace_subdirs 为准（dagspec.json 落 run 根，单一真源）。从
# dagspec.json 读首个执行节点 workspace_subdirs[0]（executor 挂载语义：首个子目录
# = 该节点工作区根 /workspace，产物落 `<run>/ws/<sub>`）拼 hello.txt 路径；dagspec
# 缺失/无声明（计划审查闸门应拦截，防御性回退）→ ws/ 任意子目录找 hello.txt
# （排除 .git）。
run_ws_hello() { # <run_dir> → stdout hello.txt 绝对路径；找不到 → 非零退出
  local run_dir="$1"
  local sub hello
  if [[ -f "$run_dir/dagspec.json" ]]; then
    sub="$(python3 - "$run_dir/dagspec.json" <<'PY' 2>/dev/null || true
import json, sys
try:
    d = json.load(open(sys.argv[1]))
    for n in d.get("nodes", []):
        subs = n.get("sandbox", {}).get("workspace_subdirs") or []
        if subs:
            print(subs[0])
            break
except Exception:
    pass
PY
)"
    if [[ -n "$sub" && -d "$run_dir/ws/$sub" ]]; then
      echo "$run_dir/ws/$sub/hello.txt"
      return 0
    fi
  fi
  hello="$(find "$run_dir/ws" -mindepth 2 -maxdepth 2 -name hello.txt -not -path '*/.git/*' 2>/dev/null | head -1 || true)"
  if [[ -n "$hello" ]]; then
    echo "$hello"
    return 0
  fi
  return 1
}

# ============================================================================
# Case 1：正路径全环（离线忠实计划 → 计划审查 → 执行 → 验收 C → Completed；
#   真规划正路径全环见 r6c.sh Tier3b / r6d.sh Tier3）
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
# 离线注入忠实计划（ALFRED_PLANNER_OFFLINE=1：规划器离线确定性直通，省真规划不稳定
#   ——glm-4.7 对 "hello.txt 在 ws 根" 偶发 workspace_subdirs/根目录错位致
#   plan_rejected / planning escalated，R6f §四 规划质量边界；计划审查/执行/执行
#   审查仍在线真模型）。contract 用请求中性原文（无"根目录/绝对路径"措辞）。
cat > "$CASE1_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r3-c1",
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
echo "[r3] case1: driver run（离线忠实计划 → 计划审查 → 执行 → 验收） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE1_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE1_DIR/request.json" \
  --run-dir "$CASE1_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"

assert_state "$CASE1_DIR" "completed"
# 产物 + 执行审查 C（R6f：产物在 run 级 ws/<workspace_subdirs[0]>）
HELLO="$(run_ws_hello "$CASE1_DIR" || true)"
if [[ -z "$HELLO" || ! -f "$HELLO" ]] || [[ "$(cat "$HELLO")" != "Hello" ]]; then
  echo "FAIL(case1): hello.txt missing/wrong content (resolved: ${HELLO:-<none>})" >&2
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
echo "PASS(case1): 离线忠实计划 → 计划审查 → 执行 → 验收 C → Completed"

# ============================================================================
# Case 2：失败升级闭环（机械失败 → 重跑2次 → Escalated → case2b 归档）
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
echo "[r3] case2: driver run（执行 --time-limit 1 强制机械超时） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE2_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
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
# C：timed_out 自适应放大轨迹——机械重跑 #1 把节点上限 1s→2s、#2 把 2s→4s
# （×2、cap 3600）；放大写回 dagspec 后 exec-N/driver.py 重渲染新 TIME_LIMIT；
# 耗尽升级 audit 带最终预算（属主决策信息充分）。
records = [json.loads(l) for l in open(sys.argv[2])]
adjusts = [r["data"].get("time_limit_adjusted") for r in records if r["event"] == "mechanical_retry"]
assert adjusts == [{"from": 1, "to": 2}, {"from": 2, "to": 4}], f"amplify trajectory: {adjusts}"
for r in records:
    if r["event"] == "mechanical_retry":
        assert r["data"]["failure_status"] == "timed_out", r["data"]
exhausted = [r for r in records if r["event"] == "mechanical_budget_exhausted_escalated"][0]
assert exhausted["data"]["time_limit_secs"] == 4, f"final budget: {exhausted['data']}"
run_dir = sys.argv[1].rsplit("/", 1)[0]
d2 = open(run_dir + "/exec-2/driver.py", encoding="utf-8").read()
assert "TIME_LIMIT_SECS = float(2)" in d2, "exec-2/driver.py 未渲染放大后的 TIME_LIMIT=2"
d3 = open(run_dir + "/exec-3/driver.py", encoding="utf-8").read()
assert "TIME_LIMIT_SECS = float(4)" in d3, "exec-3/driver.py 未渲染放大后的 TIME_LIMIT=4"
PY
echo "PASS(case2a): 机械重跑2次 → 预算耗尽 → Escalated"

# ---- case2b（归档）：decide retry --time-limit 覆盖续跑 ---
#   旧 `alfred decide` 支持 --time-limit 覆盖（重跑用更长时限真重跑）；driver
#   feed / feed_owner_message 用 run.options 持久配置（无覆盖参数），无法表达
#   "超时升级 → 属主 retry 带更长时限 → 完成"。本段归档。等价覆盖：
#   - run → Completed（case1）；
#   - 属主 retry 续跑（driver feed retry）→ r4 case2。
echo "[r3] case2b: 归档 SKIP（decide retry --time-limit 覆盖已删；等价覆盖见 r3 case1 / r4 case2）"

# ============================================================================
# Case 3：计划打回伪装闭环（不忠实计划 → PlanRejected → driver feed retry → 伪装消息 → 重规划 → 过）
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
echo "[r3] case3: driver run（离线注入不忠实计划 → 计划审查打回） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE3_DIR/plan-unfaithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
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

echo "[r3] case3: driver feed retry（伪装消息重规划） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_EXEC_REVIEW_OFFLINE=1 \
ALFRED_OFFLINE_PLAN_FILE="$CASE3_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- feed \
  --run-dir "$CASE3_DIR" \
  --decision retry
# （--message "" = Retry 消息可选：feed_owner_message 无消息跳过消息轮、不落
#   owner.message；--image 沿用 run.options 持久配置，无需重复传）

assert_state "$CASE3_DIR" "escalated"
# 断言：伪装消息无结构化否决词；llm-calls/0001.json（重规划）引用伪装消息
python3 - "$CASE3_DIR/state.json" "$CASE3_DIR/llm-calls" <<'PY' || { echo "FAIL(case3): 伪装消息含禁词或 llm 记录缺失" >&2; exit 1; }
import json, sys, glob, os
d = json.load(open(sys.argv[1]))
# R6d：离线 feed retry 走到执行审查时离线回退 → 升级挂起（escalated）——执行
# eval 只出产物无审查结论，不悄悄放行。打回→retry→重规划闭环本身已完成。
assert d["state_machine"]["state"] == "escalated", f"state={d['state_machine']['state']}"
msg = d.get("owner_message") or ""
assert msg, "owner_message (disguised) missing"
# 禁词检查（P7）：reject/verdict/否决/review/审查/scorer/grader/eval/打回/评审/评分/评估
forbidden = ["reject", "rejected", "verdict", "review", "reviewer", "scorer", "score",
             "grader", "eval", "unscored", "否决", "审查", "评审", "评分", "评估", "打分", "打回", "判定"]
low = msg.lower()
hits = [w for w in forbidden if w in low]
assert not hits, f"disguised message contains forbidden words {hits}: {msg}"
# 重规划 converse 记录存在（维护者重做后 llm-calls 含 role=maintain 记录——
# P1 起仅 ALFRED_PLANNER_OFFLINE 模式维护者也离线直通落记录；按 role 过滤取
# 最后一条 converse，r6b 同范式）
files = sorted(glob.glob(os.path.join(sys.argv[2], "*.json")))
assert len(files) >= 2, f"expect >=2 llm-calls records, got {len(files)}"
conv = [f for f in files if json.load(open(f))["role"] == "converse"]
assert len(conv) >= 2, f"expect >=2 converse records, got {len(conv)}"
rec = json.load(open(conv[-1]))
user = rec["messages"][-1]["content"]
assert "属主本轮消息" in user, "replan record missing owner message"
# 伪装消息确实喂给了 planner
assert msg in user, "disguised message not in replan prompt"
# P1/方案B 防回归：converse 实际输入无 review_summary 字段名、无禁词（含伪装消息与投影）
for f in files:
    r = json.load(open(f))
    if r.get("role") != "converse":
        continue
    for m in r["messages"]:
        content = m["content"]
        assert "review_summary" not in content, f"converse leaked field name review_summary: {content}"
        low = content.lower()
        h = [w for w in forbidden if w in low]
        assert not h, f"converse input contains forbidden signal {h}: {content}"
    u = r["messages"][-1]["content"]
    assert "owner_feedback" in u, f"converse projection missing owner_feedback: {u}"
PY
echo "PASS(case3): 打回伪装 → feed retry → 伪装消息进 planner（无结构化否决词）→ 重规划 → 执行 → 执行审查离线回退升级"

# ============================================================================
# Case 4：多轮会话（打回 → 属主补充 → 重规划；维护者已回退——会话文档保持空文档）
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
# 属主补充的新需求
cat > "$CASE4_DIR/supplement.txt" <<'TXT'
技术选型：内容必须是英文单词 Hello（大小写敏感），且文件必须位于工作区根目录。
TXT
echo "[r3] case4: driver run（打回 → 挂起 PlanRejected） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE4_DIR/plan-unfaithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE4_DIR/request.json" \
  --run-dir "$CASE4_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"
assert_state "$CASE4_DIR" "plan_rejected"

echo "[r3] case4: driver feed revise（属主补充新需求） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_EXEC_REVIEW_OFFLINE=1 \
ALFRED_OFFLINE_PLAN_FILE="$CASE4_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- feed \
  --run-dir "$CASE4_DIR" \
  --decision revise \
  --message "$CASE4_DIR/supplement.txt"

assert_state "$CASE4_DIR" "escalated"
# 断言：feed revise 设 owner_message；会话文档保持空文档语义（维护者已回退待重做）
python3 - "$CASE4_DIR/state.json" "$CASE4_DIR/llm-calls" <<'PY' || { echo "FAIL(case4): 会话文档空文档语义断言" >&2; exit 1; }
import json, sys, glob, os
d = json.load(open(sys.argv[1]))
# R6d：离线 feed revise 走到执行审查时离线回退 → 升级挂起（escalated）。
# 维护者重做后 llm-calls 含 role=maintain 记录（P1 起仅 ALFRED_PLANNER_OFFLINE
# 模式维护者也离线直通落记录）——按 role 过滤取最后一条 converse（r6b 同范式）
files = sorted(glob.glob(os.path.join(sys.argv[2], "*.json")))
assert len(files) >= 2, f"expect >=2 llm-calls, got {len(files)}"
conv = [f for f in files if json.load(open(f))["role"] == "converse"]
assert len(conv) >= 2, f"expect >=2 converse records, got {len(conv)}"
rec = json.load(open(conv[-1]))
user = rec["messages"][-1]["content"]
assert d["owner_message"] and "技术选型" in d["owner_message"], \
    f"owner_message missing supplement: {d.get('owner_message')}"
# 第二次 converse 的 prompt 含属主补充（owner_message 语义）
assert "技术选型" in user, "converse 未引用属主补充（owner_message）"
# P1/方案B 防回归：converse 实际输入无 review_summary 字段名、无禁词（投影改名+中性化）
forbidden = ["reject", "rejected", "verdict", "review", "reviewer", "scorer", "score",
             "grader", "eval", "unscored", "否决", "审查", "评审", "评分", "评估", "打分", "打回", "判定"]
for f in files:
    r = json.load(open(f))
    if r.get("role") != "converse":
        continue
    for m in r["messages"]:
        content = m["content"]
        assert "review_summary" not in content, f"converse leaked field name review_summary: {content}"
        low = content.lower()
        h = [w for w in forbidden if w in low]
        assert not h, f"converse input contains forbidden signal {h}: {content}"
    u = r["messages"][-1]["content"]
    assert "owner_feedback" in u, f"converse projection missing owner_feedback: {u}"
PY
echo "PASS(case4): 打回→属主补充→重规划（会话文档保持空文档）→ 执行审查离线回退升级（从 llm 调用记录断言）"

echo ""
echo "============================================="
echo "R3 e2e 全部通过：用例真跑 PASS（case2b 归档 SKIP）"
echo "  case1 正路径全环（离线忠实计划注入；真规划全环见 r6c.sh Tier3b / r6d.sh Tier3）: $CASE1_DIR/state.json"
echo "  case2 机械升级闭环    : $CASE2_DIR/state.json（case2b 归档：decide retry --time-limit 覆盖已删）"
echo "  case3 打回伪装闭环    : $CASE3_DIR/state.json"
echo "  case4 多轮会话文档    : $CASE4_DIR/state.json"
echo "============================================="
exit 0
