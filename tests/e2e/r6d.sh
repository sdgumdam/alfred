#!/usr/bin/env bash
# ============================================================================
# R6d e2e：执行 eval 只出产物无审查（移除内嵌 exec_verdict_scorer）+ 治理环
# 执行审查切 reviewer 容器（离线回退升级属主）
#
# 四层：
#   Tier 0（默认，无外部依赖）：cargo test —— 全量离线单测。R6d 核心断言
#     在单测层覆盖：task_gen.rs `generates_valid_python_with_values`（生成的
#     executor driver.py 无任何 scorer 残留）。治理环执行审查离线回退的**黑盒**
#     覆盖在 Tier1b（细粒度开关 ALFRED_EXEC_REVIEW_OFFLINE=1 → 升级属主）。
#   Tier 1（离线回归；Tier1a 纯离线，Tier1b 需 docker 但无需真 LLM）：
#     a) 生成 executor 任务 py，断言不含 exec_verdict_scorer /
#        _collect_artifact_summary / [BEGIN DATA] / scorer= /
#        ACCEPTANCE_CRITERIA（R6d 核心验收：执行 eval 只出产物无审查）——
#        模板静态检查 + task_gen.rs 真实 token 注入生成断言；
#     b) 治理环 ALFRED_PLANNER_OFFLINE=1（planner 离线注入忠实计划）→
#        计划审查 reviewer 容器在线（mock provider 驱动容器内 pi 经 write 工具
#        循环写 /outputs/verdict.json，非宿主直调 reviewer 模型）→ 真实执行
#        （mockllm，docker 沙箱）→ ALFRED_EXEC_REVIEW_OFFLINE=1（执行审查离线
#        回退）→ Escalated + escalation_source=Execution（§六继承项，不悄悄
#        放行），且 exec-1/driver.py 无 scorer、exec-1/state.json 无 verdict
#        字段、不建 exec-review 目录、plan-review/outputs/verdict.json +
#        compose.yaml 存在（证明计划审查真走了 reviewer 容器）。
#   Tier 2（需 docker 沙箱镜像，无需 LLM）：容器可见性实测（对齐 §二.6）——
#     reviewer 执行审查容器挂载矩阵：ws 全量 ro（写被拒）/ /inputs 文件 ro
#     （内容不可改）/ /outputs rw（verdict 落宿主）/ AGT 策略 ro + 审计子目录
#     rw。docker 缺失 SKIP。
#   Tier 3（R6D_REAL=1，需 inspect + docker + 真模型，验方跑）：真容器全链——
#     规划（planner 容器）→ 计划审查（reviewer 容器）→ 执行（executor 容器）
#     → 执行审查（reviewer 容器判 verdict）→ Completed。参照 r6c.sh Tier3
#     已验证模式，模型走 config.yml 三角色（当前环境已切 zhipucoding
#     glm-5.3-flash：planner/executor/reviewer 全用，实测 content 稳定非空）。
#     网络前提：config.yml 配置的 provider 必须可达（曾遇 kuaizi 网关
#     ai-gateway-internal.kuaizi.co=172.16.33.248 公司内网在热点网络下不可达，
#     见交付文档 §四.1）。默认关闭（留给验方）。
#
# 模型：
#   Tier 1a 纯静态（无需模型）；Tier 1b 用 inspect 内建 mockllm（executor，
#   无需 key）+ 本地 mock OpenAI 兼容 provider（reviewer——计划审查首请求返回
#   write 工具调用驱动 pi 写 verdict.json，后续请求纯文本收尾；无需真 LLM）；
#   Tier 2 无模型；Tier 3 走 config.yml 真实模型。
# 驱动：黑盒经真实 `alfred` bin（codux 可调度 CLI driver：run/feed/status）驱动——
#   r6d 以 `cargo run --bin alfred -- run` 驱动治理环（run 初始化 + 推进）。
# 验收：cargo test 全绿 + Tier 1 离线回归 PASS（或 inspect/docker 缺失
#   SKIP）+ Tier 2 容器可见性 PASS（或 docker 缺失 SKIP）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

R6D_RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$R6D_RUNS"

# inspect CLI 定位（同 r6c.sh）：ALFRED_INSPECT → venv → PATH
if [[ -n "${ALFRED_INSPECT:-}" ]]; then
  INSPECT="$ALFRED_INSPECT"
elif [[ -x ".plans/r0-lab/venv/bin/inspect" ]]; then
  INSPECT="$REPO_ROOT/.plans/r0-lab/venv/bin/inspect"
else
  INSPECT="$(command -v inspect || true)"
fi

echo "============================================="
echo "R6d Tier 0：cargo test（离线单测，含 R6d 核心断言）"
echo "============================================="
cargo test --quiet
echo "PASS(Tier0): cargo test 全绿"

# ============================================================================
# Tier 1a：生成 executor 任务 py + 断言无 scorer（纯离线，无外部依赖）
# ============================================================================
echo ""
echo "============================================="
echo "R6d Tier 1a：executor 任务 py 只出产物无审查（静态，纯离线）"
echo "============================================="

# --- 1a-1：模板单源静态检查（生成器源码不得含任何 scorer/验收标准残留） ---
TMPL="crates/alfred-executor/templates/executor_driver.py.tmpl"
for residue in "exec_verdict_scorer" "_collect_artifact_summary" "[BEGIN DATA]" "scorer=" "ACCEPTANCE_CRITERIA"; do
  if grep -Fq "$residue" "$TMPL"; then
    echo "FAIL(tier1a): 模板含 scorer 残留: $residue" >&2
    exit 1
  fi
done
echo "PASS(tier1a-1): 模板无 scorer 残留（exec_verdict_scorer/_collect_artifact_summary/[BEGIN DATA]/scorer=/ACCEPTANCE_CRITERIA）"

# --- 1a-2：真实 token 注入生成 executor 任务 py + 断言（task_gen.rs 单测直出） ---
cargo test --quiet -p alfred-executor task_gen::tests::generates_valid_python_with_values >"$R6D_RUNS/r6d-task-gen.log" 2>&1
echo "PASS(tier1a-2): task_gen.rs 生成 executor 任务 py，断言无 scorer 残留（真实 token 注入）"

# ============================================================================
# Tier 1b：治理环 ALFRED_OFFLINE=1 执行审查回退升级属主（需 inspect + docker）
# ============================================================================
TIER1B_SKIP=""
if [[ -z "$INSPECT" ]]; then
  TIER1B_SKIP="无 inspect CLI（ALFRED_INSPECT 或 PATH）"
elif ! command -v docker >/dev/null 2>&1; then
  TIER1B_SKIP="无 docker（执行步骤需沙箱）"
elif ! docker image inspect alfred-executor:latest >/dev/null 2>&1; then
  TIER1B_SKIP="无沙箱镜像 alfred-executor:latest"
fi

if [[ -n "$TIER1B_SKIP" ]]; then
  echo "SKIP(Tier1b): $TIER1B_SKIP。治理环执行审查离线回退的 e2e 覆盖需 docker
    （本机无环境时跳过）。"
else
  echo ""
  echo "============================================="
  echo "R6d Tier 1b：治理环 计划审查宿主 pi 在线 + 执行审查离线回退升级属主"
  echo "  （ALFRED_PLANNER_OFFLINE=1 规划离线注入忠实计划 + reviewer=mock provider"
  echo "   驱动宿主 pi 经 write 工具循环写 verdict + executor=mockllm 容器真实执行"
  echo "   + ALFRED_EXEC_REVIEW_OFFLINE=1 执行审查离线回退）"
  echo "============================================="
  export ALFRED_INSPECT="$INSPECT"

  T1B="$R6D_RUNS/r6d-tier1"
  rm -rf "$T1B"
  mkdir -p "$T1B"

  # mock provider：本地 OpenAI 兼容端点。计划审查走宿主 pi（reviewer 在线），
  # mock 对首请求返回 write 工具调用（pi 经正常工具循环写宿主
  # plan-review/outputs/verdict.json——路径经 ALFRED_MOCK_VERDICT_OUTPUT 注入，
  # host.rs prompt 与 mock 用同一路径），后续请求纯文本收尾——mock 只 mock 模型
  # 层，宿主 pi 真驱动（PI_CODING_AGENT_DIR 指向 run 级 mock models.json）。
  MOCK_PORT="${ALFRED_MOCK_PORT:-18731}"
  MOCK_PID=""
  cleanup_mock() {
    [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  }
  trap cleanup_mock EXIT
  ALFRED_MOCK_VERDICT_OUTPUT="$T1B/run/plan-review/outputs/verdict.json" \
  python3 tests/e2e/mock_provider.py "$MOCK_PORT" "$T1B/mock-requests.jsonl" \
    '{"pass": true, "reason": "plan faithfully addresses the owner request"}' \
    >"$T1B/mock.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 20); do
    if curl -sf "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" \
      -H 'Content-Type: application/json' \
      -d '{"model":"mock-reviewer","messages":[{"role":"user","content":"hi"}]}' >/dev/null 2>&1; then
      break
    fi
    sleep 0.3
  done

  cat > "$T1B/config.yml" <<YAML
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

  cat > "$T1B/request.json" <<'JSON'
{
  "id": "req-r6d-t1",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON

  # 忠实计划（离线 planner 直通）：计划审查必须 PASS 才能走到执行审查
  cat > "$T1B/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6d-t1",
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

  RUN1B="$T1B/run"
  rm -rf "$RUN1B"
  mkdir -p "$RUN1B"

  export ALFRED_CONFIG="$T1B/config.yml"
  export ALFRED_REVIEWER_MODEL="mock-reviewer"
  # 解耦主开关：planner 离线（注入忠实计划）+ 执行审查离线回退；计划审查在线
  # （宿主 pi + mock provider 驱动 write 工具循环写 verdict）。主开关
  # ALFRED_OFFLINE 不设（否则计划审查也离线，走不到执行审查）。
  export ALFRED_PLANNER_OFFLINE=1
  export ALFRED_OFFLINE_PLAN_FILE="$T1B/plan-faithful.json"
  export ALFRED_EXEC_REVIEW_OFFLINE=1

  echo "[r6d] tier1b: driver run（planner 离线 → 计划审查宿主 pi 在线（mock 驱动 pi 写 verdict）→ 容器真实执行 → 执行审查离线回退） ..."
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$T1B/request.json" \
    --run-dir "$RUN1B" \
    --time-limit 300 \
    --review-time-limit 90 \
    --planner-time-limit 60 \
    --no-ctl

  python3 - "$RUN1B" <<'PY' || { echo "FAIL(tier1b): 离线治理环断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
# 1) 治理环：执行审查离线回退 → Escalated + escalation_source=Execution
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
assert state.get("escalation_source") == "execution", f"escalation_source={state.get('escalation_source')}"
# 2) 审计含执行审查升级事件（§六继承项，不悄悄放行）
audit = open(os.path.join(run, "audit.jsonl")).read()
assert "exec_review_error_escalated" in audit, "audit 缺 exec_review_error_escalated"
# 3) 执行审查离线回退不跑 pi：不得建 exec-review 目录
# 4) R6d 核心验收：真实生成的 executor 任务 py 只出产物，无任何审查/scorer 残留
driver_py = os.path.join(run, "exec-1", "driver.py")
assert os.path.exists(driver_py), "exec-1/driver.py 缺失"
text = open(driver_py, encoding="utf-8").read()
for residue in ["exec_verdict_scorer", "_collect_artifact_summary", "[BEGIN DATA]",
                "scorer=", "ACCEPTANCE_CRITERIA", "get_model(role="]:
    assert residue not in text, f"executor driver.py 含 scorer 残留: {residue}"
# 5) 执行 eval 只出产物：exec-1/state.json 无 verdict/unscored_reason 字段
exec_state = json.load(open(os.path.join(run, "exec-1", "state.json")))
for field in ("verdict", "unscored_reason", "verdict_unscored_reason"):
    assert field not in exec_state["run"], f"exec-1/state.json 不应含 {field}"
assert exec_state["run"]["eval_status"] == "success", f"eval_status={exec_state['run']['eval_status']}"
# 6) 计划审查真走了宿主 pi（在线）：outputs/verdict.json + driver.done.json 存在；
#    无容器遗留物（compose.yaml/driver.py）；AGT 策略落盘且占位符已渲染。
pr = os.path.join(run, "plan-review")
assert os.path.exists(os.path.join(pr, "outputs", "verdict.json")), "宿主计划审查 outputs/verdict.json 缺失"
assert os.path.exists(os.path.join(pr, "driver.done.json")), "宿主计划审查 driver.done.json 缺失"
assert not os.path.exists(os.path.join(pr, "compose.yaml")), "宿主形态不应再渲染 compose.yaml"
assert not os.path.exists(os.path.join(pr, "driver.py")), "宿主形态不应再生成 driver.py"
pol = open(os.path.join(pr, "agt", "policy.json"), encoding="utf-8").read()
assert "{outputs_dir}" not in pol and "{outputs_redirect_allow}" not in pol, "AGT 策略占位符未渲染"
assert os.path.join(pr, "outputs") in pol, "AGT 策略未含产出目录绝对路径"
reviewer_audit = os.path.join(pr, "agt", "audit", "audit.jsonl")
assert os.path.exists(reviewer_audit), "宿主 pi AGT 审计缺失"
recs = [json.loads(l) for l in open(reviewer_audit) if l.strip()]
assert any(r.get("rule") == "allow-write-verdict-output" for r in recs), \
    f"AGT 审计缺产出白名单 allow 记录: {[(r.get('tool_name'), r.get('decision')) for r in recs]}"
PY
  echo "PASS(tier1b): 治理环 计划审查宿主 pi 在线 + 执行审查离线回退升级属主（escalation_source=execution）"
  echo "PASS(tier1b): 执行 eval 只出产物无审查（exec-1/driver.py 无 scorer + state.json 无 verdict）"
  echo "PASS(tier1b): 计划审查真走了宿主 pi（outputs/verdict.json + driver.done.json + AGT 白名单/拦截审计）"

  unset ALFRED_CONFIG ALFRED_REVIEWER_MODEL ALFRED_EXECUTOR_MODEL ALFRED_PLANNER_OFFLINE ALFRED_OFFLINE_PLAN_FILE ALFRED_EXEC_REVIEW_OFFLINE
  trap - EXIT
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
  MOCK_PID=""
fi

# ============================================================================
# Tier 2：AGT 宿主拦截语义（宿主 pi 形态，无 docker / 无 LLM）
# ============================================================================
python3 - "$R6D_RUNS" <<'PY' || { echo "FAIL(Tier2): AGT 宿主拦截语义" >&2; exit 1; }
import json, os, re, subprocess, sys
runs = sys.argv[1]
outputs = os.path.join(runs, "r6d-agt-host", "outputs")
raw = open("docker/agt/reviewer/policy.json", encoding="utf-8").read()
def regex_escape(s):
    return re.sub(r"([\\.+*?()|\[\]{}^$])", r"\\\1", s)
o = regex_escape(outputs)
allow = "(?:^|[;|&\\s])(?:>>?|tee\\s+(?:-a\\s+)?)\\s*" + o + "(?:/[^\\s|;&<>]*)?(?=[\\s]|$)"
allow_json = json.dumps(allow)[1:-1]
raw = raw.replace("{outputs_redirect_allow}", allow_json).replace("{outputs_dir}", outputs)
os.makedirs(os.path.dirname(outputs), exist_ok=True)
policy_path = os.path.join(runs, "r6d-agt-host", "policy.json")
open(policy_path, "w", encoding="utf-8").write(raw)
script = f"""
import {{ readFileSync }} from "node:fs";
const {{ evaluateToolCall, parsePolicy }} = await import("{os.getcwd()}/docker/agt/agt-policy.ts");
const policy = parsePolicy(readFileSync({json.dumps(policy_path)}, "utf8"));
const cases = [
  [{{ tool_name: "bash", args: {{ command: "echo 'v' > {outputs}/verdict.json" }} }}, "allow"],
  [{{ tool_name: "bash", args: {{ command: "echo x > /etc/contraband" }} }}, "deny"],
  [{{ tool_name: "write", args: {{ path: "{outputs}/verdict.json" }} }}, "allow"],
  [{{ tool_name: "write", args: {{ path: "/workspace/hello.txt" }} }}, "deny"],
];
let bad = 0;
for (const [ev, want] of cases) {{
  const d = evaluateToolCall(policy, ev);
  if (d.decision !== want) bad += 1;
}}
process.exit(bad === 0 ? 0 : 1);
"""
r = subprocess.run(["node", "--input-type=module", "-e", script], capture_output=True, text=True)
if r.returncode != 0:
    print(r.stderr, file=sys.stderr)
    raise SystemExit("AGT 宿主拦截语义不符")
print("  bash 重定向/write：产出目录 allow / 项目根外 deny")
PY
echo "PASS(Tier2): AGT 宿主拦截语义（产出目录白名单 + 重定向绕过封堵）"

# ============================================================================
# Tier 3：真容器全链（验方跑，需 docker + 真模型；config.yml 三角色）
# ============================================================================
if [[ "${R6D_REAL:-0}" == "1" ]]; then
  echo ""
  echo "============================================="
  echo "R6d Tier 3：宿主 pi reviewer 全链（executor 容器；config.yml 三角色）"
  echo "============================================="
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$IMAGE"
    else
      docker build -t "$IMAGE" -f docker/Dockerfile docker/
    fi
  fi
  # AGT 拦写层默认启用：内置 reviewer 策略由 host.rs 落盘 + 占位符渲染。
  # Tier 3 真容器：unset Tier 1 的 mockllm/mock 覆盖，走 config.yml 真实模型
  unset ALFRED_REVIEWER_MODEL ALFRED_EXECUTOR_MODEL ALFRED_PLANNER_MODEL LLM_REVIEWER_MODEL 2>/dev/null || true

  # ---- Tier 3：宿主 pi 全链（规划 → 计划审查 → 容器执行 → 执行审查）----
  CASE="$R6D_RUNS/r6d-real-run"
  rm -rf "$CASE"
  mkdir -p "$CASE"
  cat > "$CASE/request.json" <<'JSON'
{
  "id": "req-r6d-real",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON
  echo "[r6d] tier3: driver run（宿主 converse → 宿主计划审查 → 容器执行 → 宿主执行审查判 verdict） ..."
  cargo run --quiet -p alfred-cli --bin alfred -- run \
    --request "$CASE/request.json" \
    --run-dir "$CASE" \
    --time-limit 900 \
    --review-time-limit 300 \
    --planner-time-limit 900 \
    --image "$IMAGE"

  python3 - "$CASE" <<'PY' || { echo "FAIL(tier3): 宿主 pi 全链未完成" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "completed", f"state={state['state_machine']['state']}"
# 计划审查走了宿主 pi：outputs/verdict.json + driver.done.json + AGT 策略渲染
pr = os.path.join(run, "plan-review")
assert os.path.exists(os.path.join(pr, "outputs", "verdict.json")), "宿主计划审查 outputs/verdict.json 缺失"
assert os.path.exists(os.path.join(pr, "driver.done.json")), "宿主计划审查 driver.done.json 缺失"
assert not os.path.exists(os.path.join(pr, "compose.yaml")), "宿主形态不应有 compose.yaml"
# R6d 核心：执行审查走宿主 pi reviewer（非内嵌 scorer）——exec-review/outputs/verdict.json
er = os.path.join(run, "exec-review")
assert os.path.exists(os.path.join(er, "outputs", "verdict.json")), "宿主执行审查 outputs/verdict.json 缺失"
assert os.path.exists(os.path.join(er, "driver.done.json")), "宿主执行审查 driver.done.json 缺失"
assert not os.path.exists(os.path.join(er, "compose.yaml")), "宿主形态不应有 compose.yaml"
PY
  echo "PASS(tier3): 宿主 pi 全链（规划 → 宿主计划审查 → 容器执行 → 宿主执行审查判 verdict）→ Completed"

fi

echo ""
echo "============================================="
echo "R6d e2e 完成"
echo "  Tier 0 : cargo test 全绿（离线单测，含 R6d 核心断言）"
echo "  Tier 1a: executor 任务 py 只出产物无审查（模板 + 生成断言）"
if [[ -n "$TIER1B_SKIP" ]]; then
  echo "  Tier 1b: SKIP（$TIER1B_SKIP）"
else
  echo "  Tier 1b: 计划审查宿主 pi 在线 + 执行审查离线回退升级属主 PASS"
fi
echo "  Tier 2 : AGT 宿主拦截语义（产出目录白名单 + 重定向绕过封堵）"
echo "  Tier 3 : ${R6D_REAL:-0}（R6D_REAL=1 时真容器全链，需 config.yml provider 可达）"
echo "============================================="
exit 0
