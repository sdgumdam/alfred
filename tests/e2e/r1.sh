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
#   6. 校验 exec-N/workspace/hello.txt 存在且内容为 Hello（R3 布局：执行子 run）
#   7. 校验 exec-N/evals/ 证据归档（*.eval 与 *.dump.json，P9）
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

# --- 6. 校验产物（R6e 布局：run 级单一 ws，产物在 ws/src/——executor 首个
#    workspace_subdirs 挂 /workspace） ---
EXEC_DIR="$(ls -d "$RUN_DIR"/exec-* 2>/dev/null | sort -V | tail -1 || true)"
if [[ -z "$EXEC_DIR" ]]; then
  echo "FAIL: no exec-* dir in $RUN_DIR" >&2
  echo "--- run_dir 内容 ---" >&2
  find "$RUN_DIR" -maxdepth 2 -type f | sed "s|$REPO_ROOT/||" >&2
  exit 1
fi
WS_HELLO="$RUN_DIR/ws/src/hello.txt"
if [[ ! -f "$WS_HELLO" ]]; then
  echo "FAIL: $WS_HELLO not found" >&2
  echo "--- run_dir 内容 ---" >&2
  find "$RUN_DIR" -maxdepth 3 -type f | sed "s|$REPO_ROOT/||" >&2
  exit 1
fi
CONTENT="$(cat "$WS_HELLO")"
if [[ "$CONTENT" != "Hello" ]]; then
  echo "FAIL: hello.txt content is '$CONTENT', expected 'Hello'" >&2
  exit 1
fi

# --- 7. 校验证据归档（P9）：PASS 前断言 exec-N/evals/ 有 *.eval 与 *.dump.json ---
shopt -s nullglob
EVAL_FILES=("$EXEC_DIR"/evals/*.eval)
DUMP_FILES=("$EXEC_DIR"/evals/*.dump.json)
shopt -u nullglob
if [[ ${#EVAL_FILES[@]} -eq 0 ]] || [[ ${#DUMP_FILES[@]} -eq 0 ]]; then
  echo "FAIL: exec-N/evals/ 缺少证据归档 (*.eval=${#EVAL_FILES[@]}, *.dump.json=${#DUMP_FILES[@]})" >&2
  echo "--- exec-N/evals/ 内容 ---" >&2
  find "$EXEC_DIR/evals" -maxdepth 1 -type f 2>/dev/null | sed "s|$REPO_ROOT/||" >&2
  exit 1
fi
echo "[r1] evals arch : *.eval x${#EVAL_FILES[@]}, *.dump.json x${#DUMP_FILES[@]}"

echo ""
echo "PASS: 容器内 pi 完成小需求，产物落宿主 run 目录"
echo "  exec_dir: $EXEC_DIR"
echo "  content : $CONTENT"
echo "  evals   : $(ls "$EXEC_DIR/evals/" 2>/dev/null | tr '\n' ' ')"
exit 0
