#!/usr/bin/env bash
# ============================================================================
# R1 e2e：容器内真跑 pi → 产物落宿主 run 目录
#
# 流程：
#   1. 定位 inspect CLI（ALFRED_INSPECT，或 .plans/r0-lab/venv，或 PATH）
#   2. 确保沙箱镜像 alfred-executor:latest（无则 tag r0-lab-pi / docker build）
#   3. cargo build
#   4. 写 OwnerRequest（创建 hello.txt，内容 Hello）
#   5. cargo run -p alfred-cli -- run ...   （R3 起为完整治理环：规划→计划审查→
#      执行→执行审查；执行产物/证据在最新 exec-N/ 下）
#   6. 校验 ws/<workspace_subdirs[0]>/hello.txt 存在且内容为 Hello（R6f 布局：
#      run 级单一 ws，子目录名不硬编码——真规划器按 R6fPlannerNaming 自由选）
#   7. 校验 exec-N/驱动证据归档（driver.done.json + driver.stdout/stderr.log，P9）
#
# 验收（施工清单 S2）："容器里真跑出文件且变化符合预期"
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

# --- 1. inspect CLI ---
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
echo "[r1] inspect CLI : $INSPECT"
"$INSPECT" --version

# --- 2. 沙箱镜像 ---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    echo "[r1] tag r0-lab-pi:latest -> $IMAGE"
    docker tag r0-lab-pi:latest "$IMAGE"
  else
    echo "[r1] build $IMAGE from docker/Dockerfile ..."
    docker build -t "$IMAGE" -f docker/Dockerfile docker/
  fi
fi
echo "[r1] sandbox image : $IMAGE"

# --- 3. cargo build ---
echo "[r1] cargo build ..."
cargo build --quiet

# --- 3.5 产物路径解析（R6f）---
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

# --- 4. 写 OwnerRequest ---
RUN_DIR="${RUN_DIR:-$REPO_ROOT/tests/e2e/.runs/run-r1-hello}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
REQUEST="$RUN_DIR/request.json"
cat > "$REQUEST" <<'JSON'
{
  "id": "req-r1-hello",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-25T00:00:00Z"
}
JSON
echo "[r1] request      : $REQUEST"

# --- 5. 真跑 ---
echo "[r1] alfred run ..."
cargo run --quiet -p alfred-cli -- run \
  --request "$REQUEST" \
  --run-dir "$RUN_DIR" \
  --time-limit "${R1_TIME_LIMIT:-600}" \
  --image "$IMAGE" \

# --- 6. 校验产物（R6f 布局：run 级单一 ws，产物在 ws/<workspace_subdirs[0]>/——
#    executor 首个 workspace_subdirs 挂 /workspace；子目录名真规划器自由选，不硬编码） ---
EXEC_DIR="$(ls -d "$RUN_DIR"/exec-[0-9]* 2>/dev/null | sort -V | tail -1 || true)"
if [[ -z "$EXEC_DIR" ]]; then
  echo "FAIL: no exec-N dir in $RUN_DIR" >&2
  echo "--- run_dir 内容 ---" >&2
  find "$RUN_DIR" -maxdepth 2 -type f | sed "s|$REPO_ROOT/||" >&2
  exit 1
fi
WS_HELLO="$(run_ws_hello "$RUN_DIR" || true)"
if [[ -z "$WS_HELLO" || ! -f "$WS_HELLO" ]]; then
  echo "FAIL: hello.txt not found under $RUN_DIR/ws/ (declared workspace_subdirs[0]; resolved: ${WS_HELLO:-<none>})" >&2
  echo "--- run_dir 内容 ---" >&2
  find "$RUN_DIR" -maxdepth 3 -type f | sed "s|$REPO_ROOT/||" >&2
  exit 1
fi
CONTENT="$(cat "$WS_HELLO")"
if [[ "$CONTENT" != "Hello" ]]; then
  echo "FAIL: hello.txt content is '$CONTENT', expected 'Hello'" >&2
  exit 1
fi

# --- 7. 校验驱动证据归档（P9）：PASS 前断言 exec-N/ 有 driver.done.json 与
#    driver.stdout.log / driver.stderr.log（去 eval 后证据，替代 evals/*.eval） ---
shopt -s nullglob
DONE_FILES=("$EXEC_DIR"/driver.done.json)
STDOUT_FILES=("$EXEC_DIR"/driver.stdout.log)
STDERR_FILES=("$EXEC_DIR"/driver.stderr.log)
shopt -u nullglob
if [[ ${#DONE_FILES[@]} -eq 0 ]] || [[ ${#STDOUT_FILES[@]} -eq 0 ]] || [[ ${#STDERR_FILES[@]} -eq 0 ]]; then
  echo "FAIL: exec-N/ 缺少驱动证据归档 (driver.done.json=${#DONE_FILES[@]}, driver.stdout.log=${#STDOUT_FILES[@]}, driver.stderr.log=${#STDERR_FILES[@]})" >&2
  echo "--- exec-N/ 内容 ---" >&2
  find "$EXEC_DIR" -maxdepth 1 -type f 2>/dev/null | sed "s|$REPO_ROOT/||" >&2
  exit 1
fi
echo "[r1] driver evidence : driver.done.json x${#DONE_FILES[@]}, driver.stdout.log x${#STDOUT_FILES[@]}, driver.stderr.log x${#STDERR_FILES[@]}"

echo ""
echo "PASS: 容器内 pi 完成小需求，产物落宿主 run 目录"
echo "  exec_dir: $EXEC_DIR"
echo "  ws_hello: $WS_HELLO"
echo "  content : $CONTENT"
echo "  driver  : driver.done.json + driver.stdout.log + driver.stderr.log"
exit 0
