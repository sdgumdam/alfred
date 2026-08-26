#!/usr/bin/env bash
# ============================================================================
# R5 越界写自动化用例：沙箱隔离边界两向验证（纯 docker，无 LLM）
#
# 边界语义（P2 容器边界，实施计划 E1/E3）：network none + 只挂 workspace 的
# 沙箱容器——容器内除挂载点外的一切路径都在容器自有 overlay 层上。
#   A. 容器内写 /tmp/escape-probe.txt：容器内成功（容器自有 /tmp），
#      宿主 /tmp 对应路径不存在（含 macOS /private/tmp 软链面）→ 越界写不落宿主。
#   B. 容器内写 /workspace/escape-workspace-probe.txt：落宿主挂载目录 →
#      工作区内写穿透宿主（合法产物通道，r1/r2/r3/r4 的产物采集依赖此通道）。
#
# 对应施工清单 S2 验收："执行者试图写容器外面，必须被系统拦住"——宿主文件系统
# 是唯一判据（容器内 /tmp 是容器 overlay，不是宿主 /tmp）。不依赖 LLM。
#
# 运行：bash tests/e2e/escape.sh
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

# --- 沙箱镜像 ---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    echo "[escape] tag r0-lab-pi:latest -> $IMAGE"
    docker tag r0-lab-pi:latest "$IMAGE"
  else
    echo "[escape] build $IMAGE ..."
    docker build -t "$IMAGE" -f docker/Dockerfile docker/
  fi
fi
echo "[escape] sandbox image : $IMAGE"

# --- workspace（必须在 ~ 之下——E3；macOS /tmp 软链 & colima 只共享 ~）---
RUN_DIR="${RUN_DIR:-$REPO_ROOT/tests/e2e/.runs/escape}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR/workspace"

CONTAINER="alfred-escape-$$"
# 容器内探测路径：容器自有 /tmp（非宿主 /tmp）
CT_PROBE="/tmp/escape-probe-$$.txt"
# 宿主侧对应路径（macOS /tmp → /private/tmp 软链，两侧都查）
HOST_PROBE="$CT_PROBE"
HOST_PROBE_PRIVATE="/private${CT_PROBE}"
# 工作区内探测文件（合法产物通道）
WS_PROBE="escape-workspace-probe.txt"

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- 起沙箱容器：network none + 只挂 workspace ---
if ! docker run -d --rm --network none \
  --name "$CONTAINER" \
  -v "$RUN_DIR/workspace:/workspace" \
  "$IMAGE" tail -f /dev/null >/dev/null 2>&1; then
  echo "ERROR(escape): 容器启动失败（docker 守护进程/镜像可用性）" >&2
  exit 1
fi
echo "[escape] container  : $CONTAINER (network none, only /workspace mounted)"

# --- 方向 A：容器内写 /tmp（容器自有 overlay），宿主对应路径不得存在 ---
docker exec "$CONTAINER" sh -c "echo escape-probe > '$CT_PROBE'"
echo "[escape] A: 容器内写 $CT_PROBE 成功（rc=0）"

if [[ -e "$HOST_PROBE" ]] || [[ -e "$HOST_PROBE_PRIVATE" ]]; then
  echo "FAIL(escape-A): 宿主发现越界文件 $HOST_PROBE / $HOST_PROBE_PRIVATE" >&2
  exit 1
fi
echo "[escape] A: 宿主 $HOST_PROBE / $HOST_PROBE_PRIVATE 不存在 → 越界写未落宿主"

# 容器内确实有该文件（"容器内写入成功"的正面证据）
docker exec "$CONTAINER" sh -c "test -f '$CT_PROBE'"
echo "[escape] A: 容器内 $CT_PROBE 存在（容器内写入成功确认）"

# --- 方向 B：容器内写 /workspace（合法产物通道），宿主挂载目录必须可见 ---
docker exec "$CONTAINER" sh -c "echo workspace-probe > /workspace/$WS_PROBE"
echo "[escape] B: 容器内写 /workspace/$WS_PROBE 成功（rc=0）"

HOST_WS_PROBE="$RUN_DIR/workspace/$WS_PROBE"
if [[ ! -f "$HOST_WS_PROBE" ]]; then
  echo "FAIL(escape-B): 宿主 $HOST_WS_PROBE 不存在（工作区写未穿透宿主）" >&2
  exit 1
fi
CONTENT="$(cat "$HOST_WS_PROBE")"
if [[ "$CONTENT" != "workspace-probe" ]]; then
  echo "FAIL(escape-B): 宿主 $HOST_WS_PROBE 内容 '$CONTENT' != workspace-probe" >&2
  exit 1
fi
echo "[escape] B: 宿主 $HOST_WS_PROBE 存在且内容正确 → 工作区内写穿透宿主"

# --- 附加：容器内 /etc 写不落宿主（第三向，防"挂载外写入漏判"） ---
CT_ETC="/etc/escape-etc-probe-$$.txt"
docker exec "$CONTAINER" sh -c "echo x > '$CT_ETC'" && echo "[escape] C: 容器内写 $CT_ETC 成功（容器自有 /etc）" || true
if [[ -e "$CT_ETC" ]] || [[ -e "/private${CT_ETC}" ]]; then
  echo "FAIL(escape-C): 宿主发现容器 /etc 写入的越界文件 $CT_ETC" >&2
  exit 1
fi
echo "[escape] C: 宿主 $CT_ETC 不存在 → /etc 写入未落宿主"

echo ""
echo "PASS: 沙箱隔离边界两向验证（容器内 /tmp 写入不落宿主 + 工作区写入落宿主）"
exit 0
