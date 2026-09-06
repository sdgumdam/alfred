#!/usr/bin/env bash
# ============================================================================
# agt-default.sh — AGT 默认启用黑盒验证（属主拍板：三容器默认挂拦写层）。
#
# 验证"默认生效"与"显式关闭"两条路径，全部经真实 `alfred` bin（resolve →
# stage → compose 挂载 → driver 注入 → 容器内 pi 拦截/审计）：
#
#   Tier 1（确定性，无真 LLM——r6d tier1b 同款确定性全链，默认跑）：
#     A. 不设任何 AGT env：真二进制全链（planner 离线注入忠实计划 + 计划审查
#        走 reviewer 容器（mock provider 驱动 pi 经 write 工具循环写 verdict）
#        + executor mockllm 容器 + 执行审查离线回退）→ 断言：
#          - 内置默认策略由二进制落盘且 byte 级 == docker/agt 资产
#            （plan-review/agt == reviewer、exec-1/agt == executor）；
#          - compose 挂 /tmp/.agt ro + 审计子目录 rw；driver.py 注入 -e 扩展；
#          - reviewer 容器审计 JSONL 含 allow（容器内扩展真加载真求值的
#            可观测面——mock 驱动的 /outputs 写被放行并落审计）；
#          - 治理环推进不受默认挂载影响（escalated + escalation_source=execution）。
#     B. ALFRED_AGT_DISABLE=1：同链重跑 → 断言 opt-out：无 agt staging、
#        compose 无 /tmp/.agt、driver.py AGT_EXT 空串（不挂不加载）。
#
#   Tier 2（真 LLM，AGT_DEFAULT_REAL=1 时跑——对齐 R6B_REAL/R6C_REAL/R6D_REAL
#   惯例，不进默认骨架）：
#     C. 不设任何 AGT env 的真容器全链（真 converse → 计划审查 → 执行 → 执行
#        审查）→ 断言四角色内置策略落盘 byte 级一致 + 治理环 Completed
#        （默认挂载不破正路径）+ executor 审计含 allow（真实工具调用被求值）。
#     D. 越界写对抗探针（复用 agt/exec_probe.py；策略目录 = C 中**二进制落盘**
#        的内置资产，非手工拷贝）→ /etc 越界写被拒（容器内无探针文件）+
#        审计 ≥1 deny（no-host-path-touch）+ ws 内写放行（审计 allow + 宿主可见）。
#
# 运行：bash tests/e2e/agt-default.sh            # Tier 1（确定性）
#       AGT_DEFAULT_REAL=1 bash tests/e2e/agt-default.sh   # + Tier 2（真 LLM）
# 环境：inspect CLI（ALFRED_INSPECT 或 .plans/r0-lab/venv）+ docker +
#       alfred-executor:latest 镜像；Tier 2 另需真模型（config.yml）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

RUNS="$REPO_ROOT/tests/e2e/.runs/agt-default-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUNS"

# --- inspect CLI 定位（同 r6d.sh）：ALFRED_INSPECT → venv → PATH ---
if [[ -n "${ALFRED_INSPECT:-}" ]]; then
  INSPECT="$ALFRED_INSPECT"
elif [[ -x ".plans/r0-lab/venv/bin/inspect" ]]; then
  INSPECT="$REPO_ROOT/.plans/r0-lab/venv/bin/inspect"
else
  INSPECT="$(command -v inspect || true)"
fi

# --- 沙箱镜像（无则 tag r0-lab-pi / build，同 r6b tier2） ---
ensure_image() {
  local image="${ALFRED_IMAGE:-alfred-executor:latest}"
  if ! docker image inspect "$image" >/dev/null 2>&1; then
    if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$image"
    else
      docker build -t "$image" -f docker/Dockerfile docker/ 1>&2
    fi
  fi
  echo "$image"
}

if [[ -z "$INSPECT" ]] || ! command -v docker >/dev/null 2>&1 \
  || ! IMAGE="$(ensure_image)"; then
  echo "SKIP(agt-default): 无 inspect CLI / docker / 沙箱镜像。AGT 默认启用黑盒验证跳过。"
  exit 0
fi
export ALFRED_INSPECT="$INSPECT"
PYTHON="$(dirname "$INSPECT")/python"
echo "[agt-default] inspect CLI : $INSPECT"
echo "[agt-default] sandbox image : $IMAGE"

# --- 确定性全链共享夹具（r6d tier1b 同款）：mock provider + 离线忠实计划 ---
MOCK_PORT="${ALFRED_MOCK_PORT:-18733}"
MOCK_PID=""
cleanup() {
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
}
trap cleanup EXIT

start_mock() {
  # $1 = run 目录（mock 的 write 路径必须与 host.rs prompt 一致：该 run 的
  # plan-review/outputs/verdict.json）。
  ALFRED_MOCK_VERDICT_OUTPUT="$1/plan-review/outputs/verdict.json" \
  python3 tests/e2e/mock_provider.py "$MOCK_PORT" "$RUNS/mock-requests.jsonl" \
    '{"pass": true, "reason": "plan faithfully addresses the owner request"}' \
    >"$RUNS/mock.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 20); do
    if curl -sf "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" \
      -H 'Content-Type: application/json' \
      -d '{"model":"mock-reviewer","messages":[{"role":"user","content":"hi"}]}' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.3
  done
  echo "FAIL(agt-default): mock provider 未就绪" >&2
  exit 1
}

write_fixtures() {
  local dir="$1"
  cat > "$dir/config.yml" <<YAML
providers:
  mock:
    base_url: "http://127.0.0.1:$MOCK_PORT/v1"
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
  cat > "$dir/request.json" <<'JSON'
{
  "id": "req-agt-default",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON
  cat > "$dir/plan-faithful.json" <<'JSON'
{
  "request_id": "req-agt-default",
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
}

run_deterministic_chain() {
  local run_dir="$1"
  rm -rf "$run_dir"
  mkdir -p "$run_dir"
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$RUNS/request.json" \
    --run-dir "$run_dir" \
    --time-limit 300 \
    --review-time-limit 90 \
    --planner-time-limit 60 \
    --image "$IMAGE" \
    --no-ctl
}

write_fixtures "$RUNS"
CASE_A="$RUNS/case-a-default-on"
start_mock "$CASE_A"

# ============================================================================
# Tier 1 — Case A：默认启用（不设任何 AGT env）
# ============================================================================
echo ""
echo "============================================="
echo "Tier 1 Case A：默认启用（无 AGT env → 内置默认策略）"
echo "============================================="
CASE_A="$RUNS/case-a-default-on"
export ALFRED_CONFIG="$RUNS/config.yml"
export ALFRED_REVIEWER_MODEL="mock-reviewer"
export ALFRED_EXECUTOR_MODEL="mockllm/model"
export ALFRED_PLANNER_OFFLINE=1
export ALFRED_OFFLINE_PLAN_FILE="$RUNS/plan-faithful.json"
export ALFRED_EXEC_REVIEW_OFFLINE=1

run_deterministic_chain "$CASE_A"

python3 - "$CASE_A" "$REPO_ROOT" <<'PY' || { echo "FAIL(caseA): AGT 默认启用断言" >&2; exit 1; }
import json, os, sys
run, repo = sys.argv[1], sys.argv[2]
read = lambda p: open(p, "rb").read()

# 1) 治理环推进不受默认挂载影响（执行审查离线回退 → 升级属主，不悄悄放行）
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
# 2) 内置默认策略由二进制落盘（无 ALFRED_AGT_DIR）且 byte 级 == docker/agt 资产
#    （宿主形态：policy.json 含占位符渲染——executor 侧无占位符仍 byte 级一致；
#    reviewer 侧占位符替换后不可 byte 比，改为结构断言：JSON 合法 + 含渲染后
#    产出目录 + 占位符消失）
pr_agt = os.path.join(run, "plan-review", "agt")
assert os.path.isfile(os.path.join(pr_agt, "policy.json")), "plan-review/agt/policy.json 缺失（reviewer 默认未挂 AGT）"
_rp = open(os.path.join(pr_agt, "policy.json"), encoding="utf-8").read()
_rd = json.loads(_rp)
_out_abs = os.path.join(run, "plan-review", "outputs")
assert "{outputs_dir}" not in _rp and "{outputs_redirect_allow}" not in _rp, "reviewer 策略占位符未渲染"
assert _out_abs in _rp, "reviewer 策略未含产出目录绝对路径"
assert os.path.isfile(os.path.join(pr_agt, "agt-policy.ts")), "reviewer 扩展 agt-policy.ts 未落盘"
ex_agt = os.path.join(run, "exec-1", "agt")
assert read(os.path.join(ex_agt, "policy.json")) == read(os.path.join(repo, "docker/agt/executor/policy.json")), \
    "executor 落盘策略 != docker/agt/executor/policy.json（内置资产漂移）"
assert os.path.isdir(os.path.join(ex_agt, "audit")), "exec-1/agt/audit 审计子目录缺失（rw 挂载源）"

# 3) reviewer 宿主 pi 形态：plan-review 无 compose.yaml/driver.py（不再有挂载面）；
#    executor 仍容器：compose 挂 /tmp/.agt ro + 审计 rw
pr_compose = os.path.join(run, "plan-review", "compose.yaml")
assert not os.path.exists(pr_compose), f"{pr_compose} 不应存在（reviewer 已宿主 pi 化）"
ex_compose = os.path.join(run, "exec-1", "executor.compose.yaml")
lines = open(ex_compose, encoding="utf-8").read().splitlines()
vol = [l for l in lines if l.strip().startswith("- ") and "/tmp/.agt" in l]
assert any("/tmp/.agt:ro" in l for l in vol), f"{ex_compose} 缺策略 ro 挂载: {vol}"
assert any("/tmp/.agt/audit:rw" in l for l in vol), f"{ex_compose} 缺审计 rw 挂载: {vol}"

# 5) 容器内扩展真加载真求值的可观测面：reviewer 容器审计含 allow
#   （mock provider 驱动容器内 pi 经 write 工具写 /outputs/verdict.json →
#    AGT 求值 allow + 审计 JSONL 落宿主）
audit_path = os.path.join(pr_agt, "audit", "audit.jsonl")
assert os.path.isfile(audit_path), "reviewer 容器审计 audit.jsonl 缺失（扩展未加载或未求值）"
recs = [json.loads(l) for l in open(audit_path) if l.strip()]
assert recs, "reviewer 审计为空"
allows = [r for r in recs if r.get("decision") == "allow"]
assert allows, f"reviewer 审计无 allow 记录: {recs}"
print(f"  reviewer 审计 total={len(recs)} allow={len(allows)}")
PY
echo "PASS(caseA): 默认启用——二进制落盘内置策略（byte 级 == docker/agt）+ 宿主 pi 审计 allow + executor compose 挂载/注入"

# ============================================================================
# Tier 1 — Case B：ALFRED_AGT_DISABLE=1 显式关闭（opt-out）
# ============================================================================
echo ""
echo "============================================="
echo "Tier 1 Case B：ALFRED_AGT_DISABLE=1（不挂不加载）"
CASE_B="$RUNS/case-b-disabled"
# 显式 export（bash 函数调用的 VAR=1 前缀不保证导出给子进程）。
export ALFRED_AGT_DISABLE=1
# mock 的 write 目标路径按 case 重定向（重启 mock 实例——bind 同端口前先杀旧实例）。
kill "$MOCK_PID" 2>/dev/null || true
wait "$MOCK_PID" 2>/dev/null || true
MOCK_PID=""
while lsof -iTCP:"$MOCK_PORT" -sTCP:LISTEN >/dev/null 2>&1; do sleep 0.3; done
start_mock "$CASE_B"
run_deterministic_chain "$CASE_B"
unset ALFRED_AGT_DISABLE

python3 - "$CASE_B" <<'PY' || { echo "FAIL(caseB): AGT 显式关闭断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]

# 1) opt-out 不破链路：同链仍推进（执行审查离线回退 → 升级属主）
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"

# 2) 无 AGT staging（不落盘任何策略/扩展/审计目录）
for d in [os.path.join(run, "plan-review", "agt"), os.path.join(run, "exec-1", "agt")]:
    assert not os.path.exists(d), f"{d} 不应存在（DISABLE=1 仍挂了 AGT）"

# 3) compose 无活动 AGT 卷行（模板文档注释里的 /tmp/.agt 字样不算挂载）；
#    driver.py AGT_EXT 空串（不注入扩展）
# reviewer 宿主 pi：DISABLE=1 → 无 AGT staging（agt 目录整棵不存在）
assert not os.path.exists(os.path.join(run, "plan-review", "agt")), "DISABLE=1 仍落盘 reviewer AGT"
# executor 仍容器：compose 无活动 AGT 卷行 + driver.py 不注入扩展
lines = open(os.path.join(run, "exec-1", "executor.compose.yaml"), encoding="utf-8").read().splitlines()
active = [l for l in lines if l.strip().startswith("- ") and "/tmp/.agt" in l]
assert not active, f"executor compose 仍有 AGT 挂载行: {active}"
ex_driver = os.path.join(run, "exec-1", "driver.py")
text = open(ex_driver, encoding="utf-8").read()
assert 'AGT_EXT = ""' in text, f"{ex_driver} 仍注入 AGT 扩展"
PY
echo "PASS(caseB): ALFRED_AGT_DISABLE=1 不挂不加载（reviewer 无 staging / executor 无挂载行与扩展注入）"

unset ALFRED_CONFIG ALFRED_REVIEWER_MODEL ALFRED_EXECUTOR_MODEL \
      ALFRED_PLANNER_OFFLINE ALFRED_OFFLINE_PLAN_FILE ALFRED_EXEC_REVIEW_OFFLINE
trap - EXIT
[[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
wait "$MOCK_PID" 2>/dev/null || true
MOCK_PID=""

# ============================================================================
# Tier 2 — 真 LLM（AGT_DEFAULT_REAL=1；对齐 R6B_REAL/R6C_REAL/R6D_REAL 惯例）
# ============================================================================
if [[ "${AGT_DEFAULT_REAL:-0}" != "1" ]]; then
  echo ""
  echo "============================================="
  echo "agt-default 完成"
  echo "  Tier 1 : PASS（默认启用 + 显式关闭，确定性全链）"
  echo "  Tier 2 : SKIP（AGT_DEFAULT_REAL=1 时跑真 LLM 全链 + 越界写对抗探针）"
  echo "============================================="
  exit 0
fi

# 真模型省钱档（r3.sh 同款；调用方外部预设优先）
: "${ALFRED_EXECUTOR_MODEL:=glm-5.3-flash}"
export ALFRED_EXECUTOR_MODEL
: "${ALFRED_REVIEWER_MODEL:=glm-5.3-flash}"
export ALFRED_REVIEWER_MODEL
: "${ALFRED_PLANNER_MODEL:=glm-5.3-flash}"
export ALFRED_PLANNER_MODEL

# ---- Case C：真容器全链（无 AGT env，四角色默认挂内置策略） ----
echo ""
echo "============================================="
echo "Tier 2 Case C：真容器全链默认启用（真 converse → 计划审查 → 执行 → 执行审查）"
echo "============================================="
CASE_C="$RUNS/case-c-real-chain"
rm -rf "$CASE_C"
mkdir -p "$CASE_C"
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$RUNS/request.json" \
  --run-dir "$CASE_C" \
  --time-limit 900 \
  --review-time-limit 300 \
  --planner-time-limit 900 \
  --image "$IMAGE"

python3 - "$CASE_C" "$REPO_ROOT" <<'PY' || { echo "FAIL(caseC): 真容器全链 AGT 默认启用断言" >&2; exit 1; }
import json, os, sys
run, repo = sys.argv[1], sys.argv[2]
read = lambda p: open(p, "rb").read()
ext = read(os.path.join(repo, "docker/agt/agt-policy.ts"))

# 1) 治理环 Completed：默认挂载不破正路径（正路径 ws 内写全部放行）
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "completed", f"state={state['state_machine']['state']}"

# 2) 四角色内置策略 byte 级落盘（无 ALFRED_AGT_DIR）
roles = [
    (os.path.join(run, "planner", "agt"), "docker/agt/planner/policy.json"),
    (os.path.join(run, "plan-review", "agt"), "docker/agt/reviewer/policy.json"),
    (os.path.join(run, "exec-1", "agt"), "docker/agt/executor/policy.json"),
    (os.path.join(run, "exec-review", "agt"), "docker/agt/reviewer/policy.json"),
]
for agt_dir, asset in roles:
    assert read(os.path.join(agt_dir, "policy.json")) == read(os.path.join(repo, asset)), \
        f"{agt_dir} 落盘策略 != docker/agt/{asset}"
    assert read(os.path.join(agt_dir, "agt-policy.ts")) == ext, f"{agt_dir} 落盘扩展漂移"

# 3) executor 容器审计含 allow（真实 write 工具调用被 AGT 求值并放行）
audit_path = os.path.join(run, "exec-1", "agt", "audit", "audit.jsonl")
assert os.path.isfile(audit_path), "executor 容器审计缺失（扩展未加载或无工具调用）"
recs = [json.loads(l) for l in open(audit_path) if l.strip()]
assert any(r.get("decision") == "allow" for r in recs), f"executor 审计无 allow: {recs}"
print(f"  executor 审计 total={len(recs)}（含 allow）")
PY
echo "PASS(caseC): 真容器全链 Completed + 四角色内置策略落盘 + executor 审计 allow"

# ---- Case D：越界写对抗探针（策略目录 = Case C 二进制落盘的内置资产） ----
echo ""
echo "============================================="
echo "Tier 2 Case D：越界写对抗探针（/etc 写被拒 + 审计 deny；策略 = 二进制落盘内置资产）"
echo "============================================="
PROBE="$RUNS/probe-escape"
rm -rf "$PROBE"
mkdir -p "$PROBE/ws/src" "$PROBE/agt"
cp -R "$CASE_C/exec-1/agt/." "$PROBE/agt/"
cat > "$PROBE/exec.compose.yaml" <<YAML
services:
  default:
    image: "$IMAGE"
    command: "tail -f /dev/null"
    init: true
    network_mode: none
    stop_grace_period: 1s
    volumes:
      - "$PROBE/ws/src:/workspace:rw"
      - "$PROBE/agt:/tmp/.agt:ro"
      - "$PROBE/agt/audit:/tmp/.agt/audit:rw"
YAML

set +e
"$PYTHON" tests/e2e/agt/exec_probe.py "$PROBE" 2>&1 | tee "$PROBE/probe.log"
PROBE_RC=${PIPESTATUS[0]}
set -e
echo "[agt-default] probe rc : $PROBE_RC"

AUDIT="$PROBE/agt/audit/audit.jsonl"
if [[ ! -f "$AUDIT" ]]; then
  echo "FAIL(caseD): audit.jsonl not found at $AUDIT" >&2
  exit 1
fi
# 越界写被真拦（容器内无探针文件）+ 工作区内写放行（落宿主）
grep -q "ETC_CLEAN" "$PROBE/probe.log" || {
  echo "FAIL(caseD): /etc/agt-escape-probe.txt 在容器内未被拦（越界写未拒绝）" >&2
  exit 1
}
grep -q "ok" <(tail -1 "$PROBE/ws/src/allowed.txt" 2>/dev/null) || {
  echo "FAIL(caseD): /workspace/allowed.txt 未产出（工作区内写被误拦）" >&2
  exit 1
}
# 审计 ≥1 deny（no-host-path-touch）+ ≥1 allow（AGT audit trail）
python3 - "$AUDIT" <<'PY' || { echo "FAIL(caseD): audit assertions" >&2; exit 1; }
import json, sys
lines = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
denies = [d for d in lines if d["decision"] == "deny"]
allows = [d for d in lines if d["decision"] == "allow"]
assert denies and allows, f"deny/alllow 缺失: deny={len(denies)} allow={len(allows)}"
assert any(d.get("rule") == "no-host-path-touch" for d in denies), f"无 no-host-path-touch deny: {lines}"
print(f"  probe 审计 total={len(lines)} deny={len(denies)} allow={len(allows)}")
PY
echo "PASS(caseD): 内置默认策略真拦越界写（容器内无文件 + 审计 deny）+ ws 内放行（审计 allow）"

echo ""
echo "============================================="
echo "agt-default 完成"
echo "  Tier 1 : PASS（默认启用 + 显式关闭，确定性全链）"
echo "  Tier 2 : PASS（真容器全链 Completed + 越界写被拒 + 审计 deny/allow）"
echo "  日志   : $RUNS"
echo "============================================="
exit 0
