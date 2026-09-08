#!/usr/bin/env bash
# ============================================================================
# R4 e2e：属主决策经库驱动 feed 续跑闭环（决策面板 RPC 已删，owner 交互走 codux
# 终端 → feed_owner_message）——真跑
#
#   1. 升级闭环（escalated → feed abandon → Abandoned）：
#      planner 离线注入忠实计划（ALFRED_PLANNER_OFFLINE=1，计划审查在线）+ 执行
#      --time-limit 1 → 机械预算耗尽 → Escalated →
#   2. 打回续跑闭环（plan_rejected → feed retry → 重规划 → 执行）：
#      planner 离线注入不忠实计划（ALFRED_PLANNER_OFFLINE=1）→ 计划审查容器在线判
#      不忠实 → 打回 PlanRejected →
#      `ALFRED_PLANNER_OFFLINE=1 ALFRED_EXEC_REVIEW_OFFLINE=1 ...
#      driver feed --decision retry`（离线注入忠实计划）→ 重规划 → 计划审查
#      容器在线判过 → 执行 → 执行审查离线回退 → Escalated（R6d 不悄悄放行）。
#
# 断言：state.json 状态推进（abandoned / escalated）；case2 retry 产物落点强制断言
#   （ws/<sub0>/hello.txt 内容 Hello）+ 前轮残留 ws/<sub0>/<sub0>/ 不被新产物覆盖
#   （A4 挂载锚：锚前误嵌套写入等价物，不做运行时清理——清理有覆盖产物风险，
#   断言兜底）。旧决策面板 RPC（panel-session.jsonl extension_ui_request/response）
#   已随 CLI 删除归档。
#
# 模型：glm-5.3-flash 省钱（config.yml 或 env 覆盖）。
# 驱动：黑盒经真实 `alfred` bin（codux 可调度 CLI driver：run/feed/status）驱动——
#   r4 以 `cargo run --bin alfred -- run|feed` 驱动治理环（run 初始化+推进；feed 喂
#   属主决策 → `governance::feed_owner_message`，Abandon/Retry 消息可选）。
# 验收：cargo test 全绿 + 本脚本两用例 PASS。
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
echo "[r4] inspect CLI : $INSPECT"

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

# --- 模型（glm-5.3-flash 省钱）---
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-5.3-flash}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-5.3-flash}"
export ALFRED_PLANNER_MODEL="${ALFRED_PLANNER_MODEL:-glm-5.3-flash}"

# --- cargo build + test ---
echo "[r4] cargo build ..."
cargo build --quiet
echo "[r4] cargo test ..."
cargo test --quiet

R4_RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$R4_RUNS"
EXEC_TL="${R4_EXEC_TIME_LIMIT:-300}"
REVIEW_TL="${R4_REVIEW_TIME_LIMIT:-300}"

# 通用 Python 断言：读 state.json 的状态
assert_state() { # <run_dir> <expected_state>
  python3 - "$1" "$2" <<'PY' || { echo "FAIL: state != $2" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1] + "/state.json"))
st = d["state_machine"]["state"]
assert st == sys.argv[2], f"state={st}, expected {sys.argv[2]}"
PY
}

# （决策面板 RPC 已随 CLI 删除归档：panel-session.jsonl 结构化断言随之移除，
#   属主决策现经 driver feed → feed_owner_message，断言只读 state.json。）

# 产物路径解析（R6f）：不硬编码 workspace_subdirs 名——真规划器按 R6fPlannerNaming
# 约束自由选具体子目录名（本机实测 ['output']，非固定 'src'）；离线注入计划也以
# 其声明的 workspace_subdirs 为准（dagspec.json 落 run 根，单一真源）。从
# dagspec.json 读首个执行节点 workspace_subdirs[0]（executor 挂载语义：首个子目录
# = 该节点工作区根 /workspace，产物落 ws/<sub>/）拼 hello.txt 路径；dagspec 缺失/
# 无声明（计划审查闸门应拦截，防御性回退）→ ws/ 任意子目录找 hello.txt（排除 .git）。
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
# Case 1：升级闭环（Escalated → feed abandon → Abandoned）
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
echo "[r4] case1: driver run（planner 离线注入忠实计划 → 计划审查在线 → 执行 --time-limit 1 强制机械超时 → Escalated） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE1_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE1_DIR/request.json" \
  --run-dir "$CASE1_DIR" \
  --time-limit 1 \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"
assert_state "$CASE1_DIR" "escalated"
echo "PASS(case1a): 机械预算耗尽 → Escalated"

echo "[r4] case1: driver feed abandon（属主放弃，无消息） ..."
cargo run --quiet -p alfred-cli --bin alfred -- feed \
  --run-dir "$CASE1_DIR" \
  --decision abandon \
  --message ""
assert_state "$CASE1_DIR" "abandoned"
echo "PASS(case1): 升级闭环 → feed abandon → Abandoned（决策面板 RPC 已删归档）"

# ============================================================================
# Case 2：打回续跑闭环（PlanRejected → feed retry → 重规划 → 执行审查离线回退）
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
echo "[r4] case2: driver run（planner 离线注入不忠实计划 → 计划审查容器在线判打回 → PlanRejected） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE2_DIR/plan-unfaithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE2_DIR/request.json" \
  --run-dir "$CASE2_DIR" \
  --time-limit "$EXEC_TL" \
  --review-time-limit "$REVIEW_TL" \
  --image "$IMAGE"
assert_state "$CASE2_DIR" "plan_rejected"
echo "PASS(case2a): 不忠实计划被计划审查打回 → PlanRejected"

# --- A4 挂载锚：前轮残留注入 --------------------------------------------
# 模拟锚前执行者误嵌套写入（/workspace/<sub>/hello.txt → 宿主
# ws/<sub>/<sub>/hello.txt，即挂载锚要治的「多嵌套一层」历史产物）。round1 计划
# 审查打回不执行（ws 尚不存在），这里人工落残留等价生产事实：retry 重跑透过挂载
# 可见 /workspace/<sub>/，与锚「不存在同名嵌套子目录」矛盾——断言兜底机械重跑
# 产物仍落正确锚位，且残留不被新产物覆盖（不做运行时清理）。
SUB0="$(python3 -c "import json;print(json.load(open('$CASE2_DIR/plan-faithful.json'))['nodes'][0]['sandbox']['workspace_subdirs'][0])")"
RESIDUE="$CASE2_DIR/ws/$SUB0/$SUB0"
mkdir -p "$RESIDUE"
printf 'stale' > "$RESIDUE/hello.txt"
echo "[r4] case2: 注入前轮残留 $RESIDUE/hello.txt（锚前误嵌套写入等价物，声明子目录=${SUB0}）"

echo "[r4] case2: driver feed retry（属主重跑：planner 离线注入忠实计划 → 计划审查在线判过 → 执行 → 执行审查离线回退） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_EXEC_REVIEW_OFFLINE=1 \
ALFRED_OFFLINE_PLAN_FILE="$CASE2_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- feed \
  --run-dir "$CASE2_DIR" \
  --decision retry \
  --message ""
assert_state "$CASE2_DIR" "escalated"
# R6d：retry 重规划（离线注入忠实计划）→ 计划审查容器在线判过 → 真实执行 →
# 执行审查离线回退（ALFRED_EXEC_REVIEW_OFFLINE=1）→ 升级挂起（escalated）——
# 执行审查离线只出产物无审查结论，不悄悄放行。执行已真跑（产物落
# ws/<workspace_subdirs[0]>），内容须正确。
# A4 挂载锚：产物落点强制断言（升格原条件断言，r2 case1 / r3 case1 同范式）——
# retry 真实执行产物必须落挂载锚定位 ws/<workspace_subdirs[0]>/hello.txt 且内容
# 正确；前轮残留 ws/<sub0>/<sub0>/hello.txt 不得被新产物覆盖（执行者把产物写进
# 嵌套残留路径 = 挂载锚语义破坏，直接 FAIL）。
HELLO2="$(run_ws_hello "$CASE2_DIR" || true)"
if [[ -z "$HELLO2" || ! -f "$HELLO2" ]]; then
  echo "FAIL(case2): hello.txt not found under $CASE2_DIR/ws/ (declared workspace_subdirs[0]; resolved: ${HELLO2:-<none>})" >&2
  exit 1
fi
[[ "$(cat "$HELLO2")" == "Hello" ]] || { echo "FAIL(case2): hello.txt content wrong" >&2; exit 1; }
[[ "$(cat "$RESIDUE/hello.txt" 2>/dev/null)" == "stale" ]] || {
  echo "FAIL(case2): 前轮残留 $RESIDUE/hello.txt 被改动（执行者把产物写进嵌套残留路径——挂载锚语义破坏）" >&2
  exit 1
}
echo "PASS(case2): 打回续跑闭环 → feed retry → 重规划 → 执行审查离线回退升级 + 产物落点/残留兜底断言（决策面板 RPC 已删归档）"

echo ""
echo "============================================="
echo "R4 e2e 全部通过：属主决策 feed 续跑闭环真跑 PASS"
echo "  case1 升级闭环(abandon): $CASE1_DIR/state.json"
echo "  case2 打回续跑(retry)  : $CASE2_DIR/state.json"
echo "============================================="
exit 0
