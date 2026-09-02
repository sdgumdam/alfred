#!/usr/bin/env bash
# ============================================================================
# AGT 原型实机演示（R5/P10）：沙箱容器内 pi + AGT 策略扩展（tool_call 拦截）
#
# 两层验证：
#   1. 确定性策略求值（无 LLM、无容器）：node tests/e2e/agt/agt-policy.test.mjs
#      —— 直接 import policy-core.ts 模拟 tool_call 断言 allow/deny。
#   2. 真容器拦截（真 pi + 真 LLM，经 sandbox_agent_bridge）：本脚本主体
#      —— pi 在 network-none 容器内经桥调 glm-5.2；prompt 让 pi 先 write
#        allowed.txt（应 allow）再 `rm -rf /workspace/tmp-cleanup`（应 deny）；
#      断言审计 JSONL（AGT audit trail）含 ≥1 条 deny + ≥1 条 allow。
#
# 环境：inspect CLI（ALFRED_INSPECT 或 .plans/r0-lab/venv）+ 沙箱镜像 +
#       config.yml 的 providers.zhipucoding（真实 key，宿主侧，不进容器）。
# 依赖 LLM 行为，属"演示"；失败不阻塞骨架验收（确定性层 1 是原型证明）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO_ROOT"
# --- 容器驱动 Python（P1）：ALFRED_PYTHON 优先，否则本仓 venv（Rust python_binary() 兜底 PATH） ---
if [[ -z "${ALFRED_PYTHON:-}" && -x "$REPO_ROOT/.plans/r0-lab/venv/bin/python" ]]; then
  export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"
fi

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
PYTHON="$(dirname "$INSPECT")/python"
echo "[agt] inspect CLI : $INSPECT"

# --- 2. 沙箱镜像 ---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    docker tag r0-lab-pi:latest "$IMAGE"
  else
    echo "[agt] build $IMAGE ..."
    docker build -t "$IMAGE" -f docker/Dockerfile docker/
  fi
fi
echo "[agt] sandbox image : $IMAGE"

# --- 3. demo 目录（必须在 ~ 之下——E3）---
RUN_DIR="${AGT_DEMO_RUN_DIR:-$REPO_ROOT/tests/e2e/.runs/agt-demo}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR/agt"
cp docker/agt/agt-policy.ts "$RUN_DIR/agt/agt-policy.ts"
cp docker/agt/executor/policy.json "$RUN_DIR/agt/policy.json"

# --- 4. 生成 compose：network none + 挂载 demo 目录到 /workspace ---
cat > "$RUN_DIR/demo.compose.yaml" <<YAML
services:
  default:
    image: "$IMAGE"
    command: "tail -f /dev/null"
    init: true
    network_mode: none
    stop_grace_period: 1s
    volumes:
      - "$RUN_DIR/agt:/workspace"
YAML
echo "[agt] compose       : $RUN_DIR/demo.compose.yaml"

# --- 5. 实机演示（真容器 + 真 LLM）---
echo "[agt] running pi in sandbox with AGT extension (bridge -> zhipu glm-5.2) ..."
set +e
"$PYTHON" tests/e2e/agt/demo_probe.py "$RUN_DIR/demo.compose.yaml" "$RUN_DIR/eval-logs"
DEMO_RC=$?
set -e
echo "[agt] demo probe rc : $DEMO_RC"

# --- 6. 断言审计（AGT audit trail）：≥1 deny + ≥1 allow ---
AUDIT="$RUN_DIR/agt/.agt/audit.jsonl"
if [[ ! -f "$AUDIT" ]]; then
  echo "FAIL(agt): audit.jsonl not found at $AUDIT" >&2
  echo "  （LLM 未触发工具调用或扩展未加载；见 $RUN_DIR/eval-logs）" >&2
  exit 1
fi
python3 - "$AUDIT" <<'PY' || { echo "FAIL(agt): audit assertions" >&2; exit 1; }
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
assert lines, "audit is empty"
denies = [d for d in lines if d["decision"] == "deny"]
allows = [d for d in lines if d["decision"] == "allow"]
assert denies, f"no deny in audit: {lines}"
assert allows, f"no allow in audit: {lines}"
print(f"audit total={len(lines)} allow={len(allows)} deny={len(denies)}")
for d in denies:
    print(f"  DENY  tool={d['tool_name']} rule={d['rule']} cmd={d.get('command')!r}")
for a in allows:
    print(f"  ALLOW tool={a['tool_name']} rule={a['rule']} cmd={a.get('command')!r}")
PY

echo ""
echo "PASS(agt): 沙箱容器内 pi 工具调用被 AGT 策略扩展拦截（审计含 deny + allow）"
exit 0
