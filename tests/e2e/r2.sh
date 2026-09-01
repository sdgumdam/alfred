#!/usr/bin/env bash
# ============================================================================
#   1. 执行审查正路径：driver run（ALFRED_PLANNER_OFFLINE=1 离线忠实计划注入 →
#      计划审查在线 → pi 真容器执行 + reviewer 容器判 C）
#   1b. 计划审查离线升级：driver run（ALFRED_OFFLINE=1 主开关 → 计划审查不跑容器、
#       无 verdict → plan_review_error_escalated → §3.3 升级挂起 escalated，
#       escalation_source=plan_review）。执行审查离线回退的覆盖已移交 r6d Tier1b
#       （ALFRED_PLANNER_OFFLINE + ALFRED_EXEC_REVIEW_OFFLINE，计划审查容器在线）。
#   2. 注定不忠实计划：已归档（独立 alfred plan-review CLI 已删；等价覆盖见 r3 case3）
#   3. 解析失败 → unscored：已归档（独立 alfred plan-review CLI 已删；等价覆盖见
#      r6b caseA / r6c tier1c）
#
# 模型：默认 glm-5.3-flash（省钱；zhipu key 经 ~/.config/alfred/config.yml 或
# ALFRED_CONFIG 提供）。可用 ALFRED_EXECUTOR_MODEL / ALFRED_REVIEWER_MODEL
# 覆盖（config 缺失的模型 id 会沿用基础角色 provider——见 config.rs）。
# R3 起 `driver run` 是完整治理环：case1/1b 断言读治理环 state.json
# （state_machine/exec_verdicts），执行产物在 exec-N/workspace/。
# 驱动：黑盒经真实 `alfred` bin（codux 可调度 CLI driver：run/feed/status）驱动——
#   r2 以 `cargo run --bin alfred -- run` 驱动治理环。
# 验收：cargo test 全绿 + 本脚本两用例 PASS + 两用例归档 SKIP。
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
echo "[r2] inspect CLI : $INSPECT"

# --- 沙箱镜像（case 1 需要）---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    echo "[r2] tag r0-lab-pi:latest -> $IMAGE"
    docker tag r0-lab-pi:latest "$IMAGE"
  else
    echo "[r2] build $IMAGE ..."
    docker build -t "$IMAGE" -f docker/Dockerfile docker/
  fi
fi

# --- 模型（glm-5.3-flash 省钱；e2e 用 env 覆盖，config 缺失时沿用 zhipucoding provider）---
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-5.3-flash}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-5.3-flash}"

# --- cargo build + test ---
echo "[r2] cargo build ..."
cargo build --quiet
echo "[r2] cargo test ..."
cargo test --quiet

R2_RUNS="$REPO_ROOT/tests/e2e/.runs"

# --- 产物路径解析（R6f）---
# 不硬编码 workspace_subdirs 名：真规划器按 R6fPlannerNaming 约束自由选具体子目录
# 名（本机实测 ['output']，非固定 'src'）。从 run 根 dagspec.json 读首个执行节点
# 声明的 workspace_subdirs[0]（executor 挂载语义：首个子目录 = 该节点工作区根
# /workspace，产物落 ws/<sub>/）拼 hello.txt 路径；dagspec 缺失/无声明（计划审查
# 闸门应拦截，防御性回退）→ ws/ 任意子目录找 hello.txt（排除 .git）。
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
# Case 1：正路径全环（离线忠实计划 → 计划审查 → 执行 → 执行审查 C → Completed；
#   真规划正路径全环见 r6c.sh Tier3b / r6d.sh Tier3）
# ============================================================================
CASE1_DIR="$R2_RUNS/run-r2-exec"
rm -rf "$CASE1_DIR"
mkdir -p "$CASE1_DIR"
cat > "$CASE1_DIR/request.json" <<'JSON'
{
  "id": "req-r2-exec",
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
  "request_id": "req-r2-exec",
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
echo "[r2] case1: driver run（离线忠实计划 → 计划审查 → 执行 → 执行审查判 C） ..."
ALFRED_PLANNER_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE1_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE1_DIR/request.json" \
  --run-dir "$CASE1_DIR" \
  --time-limit "${R2_TIME_LIMIT:-900}" \
  --image "$IMAGE"

# 断言：hello.txt 内容 + exec verdict C（R6f：产物在 run 级
# ws/<workspace_subdirs[0]>/，state.json 为治理环状态）
HELLO1="$(run_ws_hello "$CASE1_DIR" || true)"
if [[ -z "$HELLO1" || ! -f "$HELLO1" ]]; then
  echo "FAIL(case1): hello.txt not found under $CASE1_DIR/ws/ (declared workspace_subdirs[0]; resolved: ${HELLO1:-<none>})" >&2
  exit 1
fi
[[ "$(cat "$HELLO1")" == "Hello" ]] || { echo "FAIL(case1): hello.txt content wrong" >&2; exit 1; }
python3 - "$CASE1_DIR/state.json" <<'PY' || { echo "FAIL(case1): exec verdict not C" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1]))
assert d["state_machine"]["state"] == "completed", f"state={d['state_machine']['state']}"
vs = d["exec_verdicts"]
assert len(vs) >= 1, "no exec verdict"
v = vs[-1]
assert v["value"] == "C", f"expected C, got {v['value']} ({v})"
assert v.get("failure_class") is None, f"C must have failure_class None, got {v.get('failure_class')}"
PY
echo "PASS(case1): 离线忠实计划 → 计划审查 → 执行 → 执行审查 C → Completed"

# ============================================================================
# Case 1b：计划审查离线升级（诚实重述，非执行审查回退）
#   ALFRED_OFFLINE=1 主开关同时让计划审查离线（不跑 reviewer 容器）→
#   plan_review_error_escalated → §3.3 升级挂起 escalated + escalation_source=
#   plan_review（不走到执行）。部分兑现 P 档由 reviewer 容器在线判定（见
#   r6d.sh Tier3 / R6f 交付文档）。执行审查离线回退覆盖移交 r6d Tier1b。
# ============================================================================
CASE1B_DIR="$R2_RUNS/run-r2-exec-partial"
rm -rf "$CASE1B_DIR"
mkdir -p "$CASE1B_DIR"
cat > "$CASE1B_DIR/request.json" <<'JSON'
{
  "id": "req-r2-exec-partial",
  "title": "create hello.txt only (partial vs acceptance)",
  "description": "Create exactly ONE file named hello.txt in the workspace. Its content must be exactly: Hello. Do NOT create any other file. Note: the acceptance criteria below is a GRADING RUBRIC that this submission will only PARTIALLY satisfy — that is intended; the plan must still only create hello.txt.",
  "acceptance_criteria": "Acceptance requires BOTH files to exist in the workspace: (1) hello.txt, AND (2) world.txt. Satisfying ONLY requirement (1) — hello.txt exists but world.txt does not — counts as PARTIAL fulfillment: grade P, not C. Satisfying neither counts as I.",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
# 离线注入忠实计划：只建 hello.txt（确定性，避免规划器自作主张建 world.txt）
cat > "$CASE1B_DIR/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r2-exec-partial",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create hello.txt with content Hello",
      "contract": {
        "prompt": "Create exactly ONE file named hello.txt in the workspace. Its content must be exactly: Hello. Do NOT create any other file.",
        "acceptance_criteria": "Acceptance requires BOTH files to exist in the workspace: (1) hello.txt, AND (2) world.txt. Satisfying ONLY requirement (1) — hello.txt exists but world.txt does not — counts as PARTIAL fulfillment: grade P, not C. Satisfying neither counts as I.",
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
echo "[r2] case1b: driver run (ALFRED_OFFLINE=1 → 计划审查离线 → §3.3 升级挂起 escalated) ..."
ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE1B_DIR/plan-faithful.json" \
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE1B_DIR/request.json" \
  --run-dir "$CASE1B_DIR" \
  --time-limit "${R2_TIME_LIMIT:-900}" \
  --image "$IMAGE"

python3 - "$CASE1B_DIR/state.json" "$CASE1B_DIR/audit.jsonl" <<'PY' || { echo "FAIL(case1b): 计划审查离线升级" >&2; exit 1; }
import json, sys
d = json.load(open(sys.argv[1]))
# 诚实重述：ALFRED_OFFLINE=1 主开关让计划审查也离线（不跑 reviewer 容器）→
# plan_review_error_escalated → 升级属主（escalation_source=plan_review），不走到
# 执行。执行审查离线回退覆盖移交 r6d Tier1b（ALFRED_PLANNER_OFFLINE +
# ALFRED_EXEC_REVIEW_OFFLINE，计划审查容器在线）。
assert d["state_machine"]["state"] == "escalated", f"state={d['state_machine']['state']}"
assert d.get("escalation_source") == "plan_review", f"escalation_source={d.get('escalation_source')}"
vs = d["exec_verdicts"]
assert len(vs) == 0, f"计划审查离线升级不产生 exec verdict, got {vs}"
events = [json.loads(l)["event"] for l in open(sys.argv[2])]
assert "plan_review_error_escalated" in events, f"audit 缺 plan_review_error_escalated: {events}"
PY
echo "PASS(case1b): ALFRED_OFFLINE=1 计划审查离线升级（escalation_source=plan_review）"

# ============================================================================
# Case 2：注定不忠实计划（需求 A 计划做 B → pass=false 打回）——已归档
#   独立 `alfred plan-review` 子命令已删（08-31 删 CLI 六命令），driver 只提供
#   run/feed/status，无独立 plan-review 入口。等价覆盖在治理环路径：
#   r3 case3（离线注入不忠实计划 → 计划审查打回 → PlanRejected）。
# ============================================================================
echo "[r2] case2: 归档 SKIP（独立 plan-review CLI 已删；等价覆盖见 r3 case3）"

# ============================================================================
# Case 3：解析失败 → unscored（reviewer=mockllm，返回非 JSON → 解析失败）——已归档
#   同 case2：独立 `alfred plan-review` CLI 已删。等价覆盖在治理环路径：
#   r6b caseA / r6c tier1c（ALFRED_OFFLINE=1 + mockllm → 计划审查 unscored →
#   escalated；r6c 另断言 plan-review/verdict.json 的 unscored_reason）。
# ============================================================================
echo "[r2] case3: 归档 SKIP（独立 plan-review CLI 已删；等价覆盖见 r6b caseA / r6c tier1c）"

echo ""
echo "============================================="
echo "R2 e2e 全部通过：两用例真跑 PASS + 两用例归档 SKIP"
echo "  case1  正路径全环（离线忠实计划注入）: $CASE1_DIR/state.json"
  echo "  case1b 计划审查离线升级 : $CASE1B_DIR/state.json"
echo "  case2  归档（独立 plan-review CLI 已删，等价覆盖见 r3 case3）"
echo "  case3  归档（独立 plan-review CLI 已删，等价覆盖见 r6b caseA / r6c tier1c）"
echo "============================================="
exit 0
