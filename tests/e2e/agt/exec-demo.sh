#!/usr/bin/env bash
# ============================================================================
# executor AGT 接入实机演示（属主钉死项：权限控制不让写文件）
#
# executor run 路径挂载语义（crates/alfred-executor run.rs + compose_gen.rs 落码）
# 下的真容器 tool_call 拦截：
#   - ws/src（契约声明 subdir）→ /workspace rw（执行者工作区根）；
#   - <run>/agt（agt-policy.ts + policy.json）→ /tmp/.agt ro（不可改策略）；
#   - <run>/agt/audit → /tmp/.agt/audit rw（审计 JSONL 落宿主）。
# pi 注入与 executor_driver.py.tmpl 逐字一致（env AGT_POLICY_PATH /
# AGT_AUDIT_PATH + -e /tmp/.agt/agt-policy.ts）。
#
# 黑盒断言（不依赖 LLM 输出文本，看容器内文件状态 + 宿主审计）：
#   A. 容器内 /etc/agt-escape-probe.txt 不存在——越界写被 AGT 在 tool_call 处
#      拦下（no-host-path-touch），审计 JSONL 有对应 deny 记录；
#   B. 容器内 /workspace/allowed.txt 存在且宿主 ws/src 可见——工作区内写放行；
#   C. 宿主 <run>/agt/audit/audit.jsonl 含 ≥1 deny + ≥1 allow（audit trail）。
#
# 环境：inspect CLI（ALFRED_INSPECT 或 .plans/r0-lab/venv）+ 沙箱镜像 +
#       ~/.config/alfred/config.yml 的 providers.zhipucoding（真 key，宿主侧）。
# 运行：bash tests/e2e/agt/exec-demo.sh
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
else
  INSPECT="$(dirname "$ALFRED_PYTHON")/inspect"
fi
if [[ ! -x "$INSPECT" ]]; then
  echo "SKIP(agt-exec): inspect CLI not found (set ALFRED_INSPECT)" >&2
  exit 0
fi
export ALFRED_INSPECT="$INSPECT"
PYTHON="$(dirname "$INSPECT")/python"
echo "[agt-exec] inspect CLI : $INSPECT"

# --- 2. 沙箱镜像 ---
IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "SKIP(agt-exec): sandbox image $IMAGE not found" >&2
  exit 0
fi
echo "[agt-exec] sandbox image : $IMAGE"

# --- 3. run 目录（必须在 ~ 之下——E3），executor run 路径布局 ---
RUN_DIR="${AGT_EXEC_DEMO_RUN_DIR:-$REPO_ROOT/tests/e2e/.runs/agt-exec-demo}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR/ws/src" "$RUN_DIR/agt/audit"
# prepare_agt_work 同款拷贝（executor 边界策略 = tests/e2e/agt/policy.json）
cp tests/e2e/agt/agt-policy.ts "$RUN_DIR/agt/agt-policy.ts"
cp tests/e2e/agt/policy.json "$RUN_DIR/agt/policy.json"

# --- 4. compose：executor 挂载语义（generate_executor_compose 同构）---
cat > "$RUN_DIR/exec.compose.yaml" <<YAML
services:
  default:
    image: "$IMAGE"
    command: "tail -f /dev/null"
    init: true
    network_mode: none
    stop_grace_period: 1s
    volumes:
      - "$RUN_DIR/ws/src:/workspace:rw"
      - "$RUN_DIR/agt:/tmp/.agt:ro"
      - "$RUN_DIR/agt/audit:/tmp/.agt/audit:rw"
YAML
echo "[agt-exec] compose       : $RUN_DIR/exec.compose.yaml"

# --- 5. 实机演示（真容器 + 真 LLM；失败不 fail 脚本，断言说了算）---
echo "[agt-exec] running pi in sandbox with AGT extension (bridge -> zhipu glm-5.2) ..."
set +e
"$PYTHON" tests/e2e/agt/exec_probe.py "$RUN_DIR" 2>&1 | tee "$RUN_DIR/probe.log"
PROBE_RC=${PIPESTATUS[0]}
set -e
echo "[agt-exec] probe rc : $PROBE_RC"

# --- 6. 宿主断言 A/B/C ---
AUDIT="$RUN_DIR/agt/audit/audit.jsonl"
if [[ ! -f "$AUDIT" ]]; then
  echo "FAIL(agt-exec): audit.jsonl not found at $AUDIT" >&2
  echo "  （LLM 未触发工具调用或扩展未加载；见 $RUN_DIR/probe.log / eval-logs）" >&2
  exit 1
fi

# A+B：容器内状态（探针回传 metadata 的兜底是宿主侧文件断言）
grep -q "ETC_CLEAN" "$RUN_DIR/probe.log" || {
  echo "FAIL(agt-exec): /etc/agt-escape-probe.txt 在容器内未被拦（越界写未拒绝）" >&2
  exit 1
}
grep -q "ok" <(tail -1 "$RUN_DIR/ws/src/allowed.txt" 2>/dev/null) || {
  echo "FAIL(agt-exec): /workspace/allowed.txt 未产出或未落宿主（工作区内写被误拦）" >&2
  exit 1
}
echo "[agt-exec] A: /etc 越界写被拦（容器内无探针文件）"
echo "[agt-exec] B: 工作区内写放行（ws/src/allowed.txt 落宿主）"

# C：审计 JSONL（AGT audit trail）≥1 deny（no-host-path-touch）+ ≥1 allow
python3 - "$AUDIT" <<'PY' || { echo "FAIL(agt-exec): audit assertions" >&2; exit 1; }
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
assert lines, "audit is empty"
denies = [d for d in lines if d["decision"] == "deny"]
allows = [d for d in lines if d["decision"] == "allow"]
assert denies, f"no deny in audit: {lines}"
assert allows, f"no allow in audit: {lines}"
escape_denies = [d for d in denies if d.get("rule") == "no-host-path-touch"]
assert escape_denies, f"no no-host-path-touch deny: {lines}"
print(f"audit total={len(lines)} allow={len(allows)} deny={len(denies)}")
for d in denies:
    print(f"  DENY  tool={d['tool_name']} rule={d['rule']} cmd={d.get('command')!r}")
for a in allows:
    print(f"  ALLOW tool={a['tool_name']} rule={a['rule']} path={a.get('path')!r}")
PY

echo ""
echo "PASS(agt-exec): executor run 路径 AGT 拦截——越界写被拒（容器内无文件 + 审计 deny）、工作区内写放行（审计 allow + 宿主可见）"
exit 0
