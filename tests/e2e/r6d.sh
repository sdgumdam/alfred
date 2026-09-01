#!/usr/bin/env bash
# ============================================================================
# R6d e2e：执行 eval 只出产物无审查（移除内嵌 exec_verdict_scorer）+ 治理环
# 执行审查切 reviewer 容器（离线回退升级属主）
#
# 四层：
#   Tier 0（默认，无外部依赖）：cargo test —— 全量离线单测。R6d 两处核心
#     断言在单测层已有覆盖：task_gen.rs `generates_valid_python_with_values`
#     （生成的 executor driver.py 无任何 scorer 残留）+ governance.rs
#     `exec_review_step_offline_falls_back_without_container`（ALFRED_OFFLINE=1
#     → 执行审查回退 → Escalated + escalation_source=Execution）。
#   Tier 1（离线回归；Tier1a 纯离线，Tier1b 需 docker 但无需真 LLM）：
#     a) 生成 executor 任务 py，断言不含 exec_verdict_scorer /
#        _collect_artifact_summary / [BEGIN DATA] / scorer= /
#        ACCEPTANCE_CRITERIA（R6d 核心验收：执行 eval 只出产物无审查）——
#        模板静态检查 + task_gen.rs 真实 token 注入生成断言；
#     b) 治理环 ALFRED_OFFLINE=1 → 计划审查（mock provider 返回 pass）→
#        真实执行（mockllm，docker 沙箱）→ 执行审查离线回退 →
#        Escalated + escalation_source=Execution（§六继承项，不悄悄放行），
#        且 exec-1/driver.py 无 scorer、exec-1/state.json 无 verdict 字段、
#        不建 exec-review 目录。
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
#   无需 key）+ 本地 mock OpenAI 兼容 provider（reviewer，返回 {"pass":true}，
#   无需真 LLM——真 kuaizi 走旧 eval 直判路径会撞 45s scoring 超时，见交付
#   文档）；Tier 2 无模型；Tier 3 走 config.yml 真实模型。
# 驱动：alfred CLI 已删（08-31），黑盒经库驱动示例 `examples/driver.rs`（r6d 以
#   `cargo run --example driver -- run` 驱动治理环——run 初始化 + 推进）。非 CLI 子命令。
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
  echo "SKIP(Tier1b): $TIER1B_SKIP。治理环离线回退由 Tier0 单测
    exec_review_step_offline_falls_back_without_container 覆盖。"
else
  echo ""
  echo "============================================="
  echo "R6d Tier 1b：治理环 ALFRED_OFFLINE=1 → 执行审查回退升级属主"
  echo "  （无需真 LLM：reviewer=mock provider，executor=mockllm）"
  echo "============================================="
  export ALFRED_INSPECT="$INSPECT"

  T1B="$R6D_RUNS/r6d-tier1"
  rm -rf "$T1B"
  mkdir -p "$T1B"

  # mock provider：本地 OpenAI 兼容端点，恒返回 {"pass":true}
  # （真 kuaizi 走旧 eval 直判会撞 plan_review.py.tmpl 45s scoring 超时，见交付文档）
  MOCK_PORT="${ALFRED_MOCK_PORT:-18731}"
  MOCK_PID=""
  cleanup_mock() {
    [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  }
  trap cleanup_mock EXIT
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
  export ALFRED_EXECUTOR_MODEL="mockllm/model"
  export ALFRED_PLANNER_MODEL="mockllm/model"
  export ALFRED_OFFLINE=1
  export ALFRED_OFFLINE_PLAN_FILE="$T1B/plan-faithful.json"

  echo "[r6d] tier1b: driver run（离线规划 → mock 计划审查 PASS → 真实执行 → 执行审查离线回退） ..."
  cargo run --quiet -p alfred-cli --example driver -- run \
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
# 3) 执行审查离线回退不跑容器：不得建 exec-review 目录
assert not os.path.exists(os.path.join(run, "exec-review")), "离线回退不应创建 exec-review 目录"
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
PY
  echo "PASS(tier1b): 治理环 ALFRED_OFFLINE=1 执行审查回退升级属主（escalation_source=execution）"
  echo "PASS(tier1b): 执行 eval 只出产物无审查（exec-1/driver.py 无 scorer + state.json 无 verdict）"

  unset ALFRED_CONFIG ALFRED_REVIEWER_MODEL ALFRED_EXECUTOR_MODEL ALFRED_PLANNER_MODEL ALFRED_OFFLINE ALFRED_OFFLINE_PLAN_FILE
  trap - EXIT
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
  MOCK_PID=""
fi

# ============================================================================
# Tier 2：容器可见性实测（对齐 §二.6，需 docker 镜像，无需 LLM）
# ============================================================================
if command -v docker >/dev/null 2>&1; then
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  if docker image inspect "$IMAGE" >/dev/null 2>&1 || docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$IMAGE"
    fi
    echo ""
    echo "============================================="
    echo "R6d Tier 2：容器可见性实测（reviewer 执行审查挂载矩阵，无 LLM）"
    echo "============================================="
    MT="$R6D_RUNS/r6d-mount-matrix"
    rm -rf "$MT"
    mkdir -p "$MT/ws" "$MT/inputs" "$MT/outputs" "$MT/agt/audit"
    printf 'Hello\n' > "$MT/ws/hello.txt"
    printf '{"id":"req"}' > "$MT/inputs/request.json"
    cp tests/e2e/agt/agt-policy.ts "$MT/agt/agt-policy.ts"
    cp tests/e2e/agt/reviewer-policy.json "$MT/agt/policy.json"
    # reviewer 执行审查挂载矩阵（对齐 §二.6 + R6a §1.1 reviewer 行）：
    # ws 全量 ro + /inputs 文件 ro + /outputs rw + AGT 策略 ro + 审计子目录 rw
    docker run --rm --network none \
      -v "$MT/ws":/workspace:ro \
      -v "$MT/inputs/request.json":/inputs/request.json:ro \
      -v "$MT/outputs":/outputs \
      -v "$MT/agt":/tmp/.agt:ro \
      -v "$MT/agt/audit":/tmp/.agt/audit:rw \
      "$IMAGE" bash -c '
        set -e
        # ws 全量 ro：可见 + 写被拒（物理 ro 挂载，双保险之一）
        test -f /workspace/hello.txt || { echo "FAIL: ws 不可见" >&2; exit 1; }
        if touch /workspace/probe.txt 2>/dev/null; then echo "FAIL: ws 可写（应 ro）" >&2; exit 1; fi
        # /inputs 文件 ro：request 可见 + 内容不可改（文件级 ro；目录层可建新文件非本矩阵断言）
        test -f /inputs/request.json || { echo "FAIL: /inputs request 不可见" >&2; exit 1; }
        if echo "tampered" > /inputs/request.json 2>/dev/null; then echo "FAIL: /inputs/request.json 可写（应 ro）" >&2; exit 1; fi
        # /outputs rw：verdict 落宿主
        echo "{}" > /outputs/verdict.json
        test -f /outputs/verdict.json || { echo "FAIL: /outputs 写失败" >&2; exit 1; }
        # AGT 策略 ro + 审计子目录 rw（agent 可写审计但不可改策略，R6a 拆分挂载语义）
        test -f /tmp/.agt/policy.json || { echo "FAIL: AGT 策略不可见" >&2; exit 1; }
        if touch /tmp/.agt/probe.txt 2>/dev/null; then echo "FAIL: AGT 策略可写（应 ro）" >&2; exit 1; fi
        echo "{\"ts\":\"x\"}" > /tmp/.agt/audit/audit.jsonl
        echo "mount-matrix-ok"
      ' 2>&1 | grep -v "Read-only file system" | tail -5
    if ! docker run --rm --network none \
      -v "$MT/ws":/workspace:ro \
      -v "$MT/inputs/request.json":/inputs/request.json:ro \
      -v "$MT/outputs":/outputs \
      -v "$MT/agt":/tmp/.agt:ro \
      -v "$MT/agt/audit":/tmp/.agt/audit:rw \
      "$IMAGE" bash -c '
        set -e
        test -f /workspace/hello.txt || exit 1
        touch /workspace/probe.txt 2>/dev/null && exit 1
        test -f /inputs/request.json || exit 1
        echo "tampered" > /inputs/request.json 2>/dev/null && exit 1
        echo "{}" > /outputs/verdict.json
        test -f /tmp/.agt/policy.json || exit 1
        touch /tmp/.agt/probe.txt 2>/dev/null && exit 1
        echo "{\"ts\":\"x\"}" > /tmp/.agt/audit/audit.jsonl
      ' >/dev/null 2>&1; then
      echo "FAIL(Tier2): 容器可见性实测" >&2; exit 1
    fi
    # 断言产物回宿主：/outputs/verdict.json + AGT 审计
    test -f "$MT/outputs/verdict.json" || { echo "FAIL(Tier2): outputs 未回宿主" >&2; exit 1; }
    test -f "$MT/agt/audit/audit.jsonl" || { echo "FAIL(Tier2): AGT 审计未回宿主" >&2; exit 1; }
    echo "PASS(Tier2): 容器可见性实测（ws 全量 ro / inputs 文件 ro / outputs rw / AGT 策略 ro + 审计 rw）"
  else
    echo "SKIP(Tier2): 无沙箱镜像（$IMAGE / r0-lab-pi:latest）。容器可见性实测跳过。"
  fi
else
  echo "SKIP(Tier2): 无 docker。容器可见性实测跳过。"
fi

# ============================================================================
# Tier 3：真容器全链（验方跑，需 docker + 真模型；config.yml 三角色）
# ============================================================================
if [[ "${R6D_REAL:-0}" == "1" ]]; then
  echo ""
  echo "============================================="
  echo "R6d Tier 3：真容器全链（需 docker 镜像 + 真模型，config.yml 三角色）"
  echo "============================================="
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$IMAGE"
    else
      docker build -t "$IMAGE" -f docker/Dockerfile docker/
    fi
  fi
  # AGT 拦写层：reviewer deny-write 策略（写 /workspace 被拒、/outputs 放行）
  AGT_DIR="$R6D_RUNS/r6d-agt"
  rm -rf "$AGT_DIR"
  mkdir -p "$AGT_DIR"
  cp tests/e2e/agt/agt-policy.ts "$AGT_DIR/agt-policy.ts"
  cp tests/e2e/agt/reviewer-policy.json "$AGT_DIR/policy.json"
  export ALFRED_AGT_DIR="$AGT_DIR"
  # Tier 3 真容器：unset Tier 1 的 mockllm/mock 覆盖，走 config.yml 真实模型
  unset ALFRED_REVIEWER_MODEL ALFRED_EXECUTOR_MODEL ALFRED_PLANNER_MODEL LLM_REVIEWER_MODEL 2>/dev/null || true

  # ---- Tier 3：真容器全链（规划 → 计划审查容器 → 执行 → 执行审查容器）----
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
  echo "[r6d] tier3: driver run（真容器 converse → 计划审查容器 → 执行 → 执行审查容器判 verdict） ..."
  cargo run --quiet -p alfred-cli --example driver -- run \
    --request "$CASE/request.json" \
    --run-dir "$CASE" \
    --time-limit 900 \
    --review-time-limit 300 \
    --planner-time-limit 900 \
    --image "$IMAGE"

  python3 - "$CASE" <<'PY' || { echo "FAIL(tier3): 真容器全链未完成" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "completed", f"state={state['state_machine']['state']}"
# 计划审查走了容器：plan-review/inputs/conversation.json（reviewer 独有挂载输入）存在
pr = os.path.join(run, "plan-review")
assert os.path.exists(os.path.join(pr, "inputs", "conversation.json")), "容器计划审查缺 conversation.json 输入"
assert os.path.exists(os.path.join(pr, "outputs", "verdict.json")), "容器计划审查 outputs/verdict.json 缺失"
assert os.path.exists(os.path.join(pr, "compose.yaml")), "容器计划审查 compose.yaml 缺失"
# R6d 核心：执行审查走 reviewer 容器（非内嵌 scorer）——exec-review/outputs/verdict.json
er = os.path.join(run, "exec-review")
assert os.path.exists(os.path.join(er, "outputs", "verdict.json")), "容器执行审查 outputs/verdict.json 缺失"
assert os.path.exists(os.path.join(er, "compose.yaml")), "容器执行审查 compose.yaml 缺失"
assert os.path.exists(os.path.join(er, "inputs", "conversation.json")), "容器执行审查缺 conversation.json 输入"
PY
  echo "PASS(tier3): 真容器全链（规划 → 计划审查容器 → 执行 → 执行审查容器判 verdict）→ Completed"

  unset ALFRED_AGT_DIR
fi

echo ""
echo "============================================="
echo "R6d e2e 完成"
echo "  Tier 0 : cargo test 全绿（离线单测，含 R6d 核心断言）"
echo "  Tier 1a: executor 任务 py 只出产物无审查（模板 + 生成断言）"
if [[ -n "$TIER1B_SKIP" ]]; then
  echo "  Tier 1b: SKIP（$TIER1B_SKIP）"
else
  echo "  Tier 1b: 治理环 ALFRED_OFFLINE=1 执行审查回退升级属主 PASS"
fi
echo "  Tier 2 : 容器可见性实测（挂载矩阵；docker 缺失 SKIP）"
echo "  Tier 3 : ${R6D_REAL:-0}（R6D_REAL=1 时真容器全链，需 config.yml provider 可达）"
echo "============================================="
exit 0
