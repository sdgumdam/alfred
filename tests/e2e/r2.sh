#!/usr/bin/env bash
# ============================================================================
# R2 e2e：审查侧四用例（真跑）
#
#   1. 执行审查正路径：alfred run（pi 真容器 + 内嵌 ExecVerdict scorer 判 C）
#   1b. 执行审查部分兑现：alfred run（prompt 只建 hello.txt，验收要 hello+world
#       → grader 判 P）
#   2. 注定不忠实计划：alfred plan-review（需求 A 计划做 B → pass=false 打回）
#   3. 解析失败 → unscored：alfred plan-review（reviewer=mockllm → 解析失败）
#
# 模型：默认 glm-4.7（省钱；zhipu key 经 ~/.config/alfred/config.yml 或
# ALFRED_CONFIG 提供）。可用 ALFRED_EXECUTOR_MODEL / ALFRED_REVIEWER_MODEL
# 覆盖（config 缺失的模型 id 会沿用基础角色 provider——见 config.rs）。
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

# --- 模型（glm-4.7 省钱；e2e 用 env 覆盖，config 缺失时沿用 zhipucoding provider）---
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-4.7}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-4.7}"

# --- cargo build + test ---
echo "[r2] cargo build ..."
cargo build --quiet
echo "[r2] cargo test ..."
cargo test --quiet

R2_RUNS="$REPO_ROOT/tests/e2e/.runs"

# ============================================================================
# Case 1：执行审查正路径（scorer 判 C）
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
echo "[r2] case1: alfred run (exec review, scorer 判 C) ..."
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE1_DIR/request.json" \
  --run-dir "$CASE1_DIR" \
  --time-limit "${R2_TIME_LIMIT:-900}" \
  --image "$IMAGE"

# 断言：hello.txt 内容 + verdict.value == C
if [[ ! -f "$CASE1_DIR/workspace/hello.txt" ]]; then
  echo "FAIL(case1): workspace/hello.txt not found" >&2
  exit 1
fi
[[ "$(cat "$CASE1_DIR/workspace/hello.txt")" == "Hello" ]] || { echo "FAIL(case1): hello.txt content wrong" >&2; exit 1; }
python3 - "$CASE1_DIR/state.json" <<'PY' || { echo "FAIL(case1): exec verdict not C" >&2; exit 1; }
import json, sys
st = json.load(open(sys.argv[1]))
v = st["run"]["verdict"]
assert v is not None, f"verdict is None (unscored={st['run'].get('verdict_unscored_reason')})"
assert v["value"] == "C", f"expected C, got {v['value']} ({v})"
assert v.get("failure_class") is None, f"C must have failure_class None, got {v.get('failure_class')}"
PY
echo "PASS(case1): exec review 正路径 scorer 判 C"

# ============================================================================
# Case 1b：执行审查部分通过（scorer 判 P——部分兑现）
#   prompt 只让 pi 建 hello.txt；acceptance_criteria 要求 hello+world 两个，
#   并显式声明"只满足其一 = P"。grader 只见 acceptance_criteria + 产物摘要
#   （不见 prompt），应判 P（R2Audit2：P 档补通）。
# ============================================================================
CASE1B_DIR="$R2_RUNS/run-r2-exec-partial"
rm -rf "$CASE1B_DIR"
mkdir -p "$CASE1B_DIR"
cat > "$CASE1B_DIR/request.json" <<'JSON'
{
  "id": "req-r2-exec-partial",
  "title": "create hello.txt only (partial vs acceptance)",
  "description": "Create exactly ONE file named hello.txt in the workspace. Its content must be exactly: Hello. Do NOT create any other file.",
  "acceptance_criteria": "Acceptance requires BOTH files to exist in the workspace: (1) hello.txt, AND (2) world.txt. Satisfying ONLY requirement (1) — hello.txt exists but world.txt does not — counts as PARTIAL fulfillment: grade P, not C. Satisfying neither counts as I.",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
echo "[r2] case1b: alfred run (exec review, scorer 判 P 部分兑现) ..."
cargo run --quiet -p alfred-cli -- run \
  --request "$CASE1B_DIR/request.json" \
  --run-dir "$CASE1B_DIR" \
  --time-limit "${R2_TIME_LIMIT:-900}" \
  --image "$IMAGE"

python3 - "$CASE1B_DIR/state.json" <<'PY' || { echo "FAIL(case1b): exec verdict not P" >&2; exit 1; }
import json, sys
st = json.load(open(sys.argv[1]))
v = st["run"]["verdict"]
assert v is not None, f"verdict is None (unscored={st['run'].get('verdict_unscored_reason')})"
assert v["value"] == "P", f"expected P, got {v['value']} ({v})"
assert v.get("failure_class") is not None, f"P must have failure_class, got {v}"
PY
echo "PASS(case1b): 部分兑现 → P"

# ============================================================================
# Case 2：注定不忠实计划（需求 A 计划做 B → pass=false 打回）
# ============================================================================
CASE2_DIR="$R2_RUNS/run-r2-plan-fail"
rm -rf "$CASE2_DIR"
mkdir -p "$CASE2_DIR"
cat > "$CASE2_DIR/request.json" <<'JSON'
{
  "id": "req-r2-plan",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt with content Hello",
  "acceptance_criteria": "hello.txt exists with content Hello",
  "created_at": "2026-08-26T00:00:00Z"
}
JSON
# 计划做的是 world.txt —— 与需求 A 不符（注定不忠实）
cat > "$CASE2_DIR/dagspec.json" <<'JSON'
{
  "request_id": "req-r2-plan",
  "nodes": [
    {
      "id": "task-1",
      "summary": "create world.txt with content World",
      "contract": {
        "prompt": "Create a file named world.txt with content World",
        "acceptance_criteria": "world.txt exists with content World",
        "reviewer_models": []
      }
    }
  ]
}
JSON
echo "[r2] case2: alfred plan-review (注定不忠实 → pass=false) ..."
cargo run --quiet -p alfred-cli -- plan-review \
  --request "$CASE2_DIR/request.json" \
  --dagspec "$CASE2_DIR/dagspec.json" \
  --run-dir "$CASE2_DIR" \
  --time-limit "${R2_REVIEW_TIME_LIMIT:-300}"

python3 - "$CASE2_DIR/verdict.json" <<'PY' || { echo "FAIL(case2): plan verdict not pass=false" >&2; exit 1; }
import json, sys
v = json.load(open(sys.argv[1]))
vd = v["verdict"]
assert vd is not None, f"verdict is None (unscored={v.get('unscored_reason')})"
assert vd["pass"] is False, f"expected pass=false, got {vd}"
assert vd["reason"], "reason must be non-empty"
PY
echo "PASS(case2): 注定不忠实计划被打回 (pass=false)"

# ============================================================================
# Case 3：解析失败 → unscored（reviewer=mockllm，返回非 JSON → 解析失败）
# ============================================================================
CASE3_DIR="$R2_RUNS/run-r2-plan-unscored"
rm -rf "$CASE3_DIR"
mkdir -p "$CASE3_DIR"
cp "$CASE2_DIR/request.json" "$CASE3_DIR/request.json"
cp "$CASE2_DIR/dagspec.json" "$CASE3_DIR/dagspec.json"
echo "[r2] case3: alfred plan-review (mockllm → 解析失败 unscored) ..."
ALFRED_REVIEWER_MODEL="mockllm/model" cargo run --quiet -p alfred-cli -- plan-review \
  --request "$CASE3_DIR/request.json" \
  --dagspec "$CASE3_DIR/dagspec.json" \
  --run-dir "$CASE3_DIR" \
  --time-limit 120

python3 - "$CASE3_DIR/verdict.json" <<'PY' || { echo "FAIL(case3): expected unscored" >&2; exit 1; }
import json, sys
v = json.load(open(sys.argv[1]))
assert v["verdict"] is None, f"expected unscored, got {v['verdict']}"
assert v["unscored_reason"] == "plan_verdict_parse_failure", f"unexpected unscored_reason {v.get('unscored_reason')}"
PY
echo "PASS(case3): 解析失败 → unscored"

echo ""
echo "============================================="
echo "R2 e2e 全部通过：四用例真跑 PASS"
echo "  case1  执行审查 C  : $CASE1_DIR/state.json"
echo "  case1b 执行审查 P  : $CASE1B_DIR/state.json"
echo "  case2  计划打回    : $CASE2_DIR/verdict.json"
echo "  case3  unscored    : $CASE3_DIR/verdict.json"
echo "============================================="
exit 0
