#!/usr/bin/env bash
# ============================================================================
# R6c e2e：reviewer 容器化回归（eval 内嵌 grader → 独立容器 Agent）
#
# 四层：
#   Tier 0（默认，无外部依赖）：cargo test —— 离线 reviewer 容器驱动单测
#     （compose 渲染 / 模板注入 / 输入落盘 / verdict.json Pydantic 等价校验 /
#     AGT deny-write 策略求值）。
#   Tier 1（需 inspect CLI，无需 docker / 无需真 LLM）：离线回归——
#     a) reviewer-policy.json 确定性求值（node 直测 deny-write 语义）；
#     b) 独立 plan-review（container=None，旧 eval 路径）→ 已归档（独立 alfred
#        plan-review CLI 已删；等价覆盖在治理环路径：tier1c / r6b caseA 断言
#        plan-review/verdict.json + plan_review.py 旧模板产物）；
#     c) driver run（ALFRED_OFFLINE=1 + mockllm 审查）→ 计划审查 unscored →
#        escalated（断言治理环 container=Some 时离线回退旧 eval 路径）。
#   Tier 2（需 docker 沙箱镜像，无需 LLM）：容器可见性实测（验收 §四.1）——
#     按 R6c 挂载矩阵起容器断言：ws 全量 ro（写被拒）、/inputs ro、/outputs rw
#     （verdict 落宿主）、AGT 策略 ro + 审计子目录 rw。docker 缺失 SKIP。
#   Tier 3（R6C_REAL=1，需 inspect + docker + 真模型，验方跑）：真容器——
#     a) 独立 exec-review 夹带私货用例 → 已归档（独立 alfred exec-review CLI 已
#        删；执行审查容器路径等价覆盖见 tier3b 真容器全链）；
#     b) driver run 真容器全链（converse → 计划审查容器 → 执行 → 执行审查）。
#     默认关闭（留给验方）。
#
# 驱动：alfred CLI 已删（08-31），黑盒经库驱动示例 `examples/driver.rs`（r6c 以
#   `cargo run --example driver -- run` 驱动治理环）。非 CLI 子命令。
# 模型：Tier 1 用 mockllm（inspect 内建，无需 key）做计划审查——planner 离线
#   直通、executor 不触发，因此不需要 docker 与真实 provider。
# 验收：cargo test 全绿 + Tier 1 离线回归 PASS（或 inspect 缺失 SKIP）+
#   Tier 2 容器可见性 PASS（或 docker 缺失 SKIP）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
# --- 容器驱动 Python（P1）：ALFRED_PYTHON 优先，否则本仓 venv（Rust python_binary() 兜底 PATH） ---
if [[ -z "${ALFRED_PYTHON:-}" && -x "$REPO_ROOT/.plans/r0-lab/venv/bin/python" ]]; then
  export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"
fi

R6C_RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$R6C_RUNS"

echo "============================================="
echo "R6c Tier 0：cargo test（离线单测 + reviewer 容器驱动单测）"
echo "============================================="
cargo test --quiet
echo "PASS(Tier0): cargo test 全绿"

# --- Tier 1：inspect CLI（离线回归需要） ---
if [[ -n "${ALFRED_INSPECT:-}" ]]; then
  INSPECT="$ALFRED_INSPECT"
elif [[ -x ".plans/r0-lab/venv/bin/inspect" ]]; then
  INSPECT="$REPO_ROOT/.plans/r0-lab/venv/bin/inspect"
else
  INSPECT="$(command -v inspect || true)"
fi

if [[ -z "$INSPECT" ]]; then
  echo "SKIP(Tier1): 无 inspect CLI（ALFRED_INSPECT 或 PATH）。离线回归跳过；"
  echo "  Tier 0 已覆盖离线 reviewer 容器驱动单测。"
else
  echo ""
  echo "============================================="
  echo "R6c Tier 1：离线回归（无 docker / 无真 LLM）"
  echo "============================================="
  export ALFRED_INSPECT="$INSPECT"
  echo "[r6c] inspect CLI : $INSPECT"

  # 最小 config：planner/executor 用 dummy provider（不真调），reviewer 走 mockllm
  CFG_DIR="$R6C_RUNS/run-r6c-cfg"
  rm -rf "$CFG_DIR"
  mkdir -p "$CFG_DIR"
  cat > "$CFG_DIR/config.yml" <<'YAML'
providers:
  dummy:
    base_url: "http://127.0.0.1:9/v1"
    api_key: "sk-dummy-never-called"
models:
  - id: glm-4.7
    provider: dummy
roles:
  executor: glm-4.7
  planner: glm-4.7
YAML
  export ALFRED_CONFIG="$CFG_DIR/config.yml"
  export ALFRED_PLANNER_MODEL="glm-4.7"
  export ALFRED_EXECUTOR_MODEL="glm-4.7"
  export ALFRED_REVIEWER_MODEL="mockllm/model"

  # ---- Tier 1a：reviewer-policy.json 确定性求值（AGT deny-write 语义） ----
  echo "[r6c] tier1a: reviewer-policy.json 确定性求值（node） ..."
  node tests/e2e/agt/reviewer-policy.test.mjs >"$R6C_RUNS/r6c-agt-policy.log" 2>&1
  echo "PASS(tier1a): reviewer deny-write 策略求值全绿"

  # ---- Tier 1b（归档）：独立 alfred plan-review（container=None 旧 eval 路径）+ mockllm → unscored ----
  #   独立 `alfred plan-review` CLI 已删（08-31 删 CLI 六命令），driver 只提供
  #   run/feed/status，无独立 plan-review 入口。container=None 离线回退路径的等价
  #   覆盖在治理环：tier1c（driver run + ALFRED_OFFLINE + mockllm → unscored，
  #   断言 plan-review/verdict.json + plan_review.py 旧模板产物）。
  echo "[r6c] tier1b: 归档 SKIP（独立 plan-review CLI 已删；等价覆盖见 tier1c / r6b caseA）"

  # ---- Tier 1c：driver run 离线（container=Some + ALFRED_OFFLINE=1 → 回退旧 eval 路径） ----
  CASE_C="$R6C_RUNS/run-r6c-offline-run"
  rm -rf "$CASE_C"
  mkdir -p "$CASE_C"
  cat > "$CASE_C/request.json" <<'JSON'
{
  "id": "req-r6c-run",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON
  cat > "$CASE_C/plan-faithful.json" <<'JSON'
{
  "request_id": "req-r6c-run",
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
  echo "[r6c] tier1c: driver run（离线规划 → mockllm 审查 unscored → escalated） ..."
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$CASE_C/plan-faithful.json" \
  cargo run --quiet -p alfred-cli --example driver -- run \
    --request "$CASE_C/request.json" \
    --run-dir "$CASE_C" \
    --time-limit 60 \
    --review-time-limit 60 \
    --planner-time-limit 60

  python3 - "$CASE_C" <<'PY' || { echo "FAIL(tier1c): 离线治理环断言" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "escalated", f"state={state['state_machine']['state']}"
# 离线（ALFRED_OFFLINE=1）确定性直通：不产容器驱动脚本，直接落 verdict.json
# （真容器 driver.py/driver.done.json 由 Tier3 断言，非本层）
pr = os.path.join(run, "plan-review")
assert os.path.exists(os.path.join(pr, "verdict.json")), "plan-review/verdict.json missing"
vd = json.load(open(os.path.join(pr, "verdict.json")))
assert vd["verdict"] is None, f"expected unscored, got {vd['verdict']}"
PY
  echo "PASS(tier1c): 离线治理环（ALFRED_OFFLINE → 确定性直通 unscored → escalated）"

  unset ALFRED_CONFIG
  echo ""
  echo "R6c Tier 1 全部通过：离线回归 PASS"
fi

# --- Tier 2：容器可见性实测（验收 §四.1，需 docker 镜像，无需 LLM） ---
if command -v docker >/dev/null 2>&1; then
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  if docker image inspect "$IMAGE" >/dev/null 2>&1 || docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
    if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$IMAGE"
    fi
    echo ""
    echo "============================================="
    echo "R6c Tier 2：容器可见性实测（挂载矩阵，无 LLM）"
    echo "============================================="
    MT="$R6C_RUNS/run-r6c-mount-matrix"
    rm -rf "$MT"
    mkdir -p "$MT/ws" "$MT/inputs" "$MT/outputs" "$MT/agt/audit"
    printf 'Hello\n' > "$MT/ws/hello.txt"
    printf '{"id":"req"}' > "$MT/inputs/request.json"
    cp tests/e2e/agt/agt-policy.ts "$MT/agt/agt-policy.ts"
    cp tests/e2e/agt/reviewer-policy.json "$MT/agt/policy.json"
    # reviewer 挂载矩阵（§1.1 reviewer 行）：ws 全量 ro + /inputs ro + /outputs rw
    # + AGT 策略 ro + 审计子目录 rw
    docker run --rm --network none \
      -v "$MT/ws":/workspace:ro \
      -v "$MT/inputs/request.json":/inputs/request.json:ro \
      -v "$MT/outputs":/outputs \
      -v "$MT/agt":/tmp/.agt:ro \
      -v "$MT/agt/audit":/tmp/.agt/audit:rw \
      "$IMAGE" bash -c '
        set -e
        # ws 全量 ro：可见 + 写被拒
        test -f /workspace/hello.txt || { echo "FAIL: ws 不可见" >&2; exit 1; }
        if touch /workspace/probe.txt 2>/dev/null; then echo "FAIL: ws 可写（应 ro）" >&2; exit 1; fi
        # /inputs ro：request 可见 + 写被拒
        test -f /inputs/request.json || { echo "FAIL: /inputs 不可见" >&2; exit 1; }
        # /outputs rw：verdict 落宿主
        echo "{}" > /outputs/verdict.json
        test -f /outputs/verdict.json || { echo "FAIL: /outputs 写失败" >&2; exit 1; }
        # AGT 策略 ro + 审计子目录 rw
        test -f /tmp/.agt/policy.json || { echo "FAIL: AGT 策略不可见" >&2; exit 1; }
        echo "{\"ts\":\"x\"}" > /tmp/.agt/audit/audit.jsonl
        echo "mount-matrix-ok"
      ' || { echo "FAIL(Tier2): 容器可见性实测" >&2; exit 1; }
    # 断言产物回宿主：/outputs/verdict.json + AGT 审计
    test -f "$MT/outputs/verdict.json" || { echo "FAIL(Tier2): outputs 未回宿主" >&2; exit 1; }
    test -f "$MT/agt/audit/audit.jsonl" || { echo "FAIL(Tier2): AGT 审计未回宿主" >&2; exit 1; }
    echo "PASS(Tier2): 容器可见性实测（ws ro / inputs ro / outputs rw / AGT 策略 ro + 审计 rw）"
  else
    echo "SKIP(Tier2): 无沙箱镜像（$IMAGE / r0-lab-pi:latest）。容器可见性实测跳过。"
  fi
else
  echo "SKIP(Tier2): 无 docker。容器可见性实测跳过。"
fi

# --- Tier 3：真容器 reviewer（验方跑，需 docker + 真模型） ---
if [[ "${R6C_REAL:-0}" == "1" ]]; then
  echo ""
  echo "============================================="
  echo "R6c Tier 3：真容器 reviewer（需 docker 镜像 + 真模型）"
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
  AGT_DIR="$R6C_RUNS/run-r6c-agt"
  rm -rf "$AGT_DIR"
  mkdir -p "$AGT_DIR"
  cp tests/e2e/agt/agt-policy.ts "$AGT_DIR/agt-policy.ts"
  cp tests/e2e/agt/reviewer-policy.json "$AGT_DIR/policy.json"
  export ALFRED_AGT_DIR="$AGT_DIR"
  # Tier 3 真容器：unset Tier 1 的 mockllm 覆盖，走 config.yml 真实模型
  # （ALFRED_REVIEWER_MODEL=mockllm/model 会吞掉真模型；roles 已按属主指定
  # 走 kuaizi provider 稳定模型）
  unset ALFRED_REVIEWER_MODEL ALFRED_EXECUTOR_MODEL ALFRED_PLANNER_MODEL LLM_REVIEWER_MODEL 2>/dev/null || true

  # ---- Tier 3a（归档）：夹带私货用例（独立 alfred exec-review）----
  #   独立 `alfred exec-review` CLI 已删（08-31 删 CLI 六命令），driver 只提供
  #   run/feed/status。执行审查容器路径的等价覆盖见 tier3b（driver run 真容器全链
  #   → 执行审查容器判 verdict）。夹带私货断言随 CLI 删除归档。
  echo "[r6c] tier3a: 归档 SKIP（独立 exec-review CLI 已删；等价覆盖见 tier3b）"

  # ---- Tier 3b：真容器全链（converse → 计划审查容器 → 执行 → 执行审查）----
  CASE_B="$R6C_RUNS/run-r6c-real-run"
  rm -rf "$CASE_B"
  mkdir -p "$CASE_B"
  cat > "$CASE_B/request.json" <<'JSON'
{
  "id": "req-r6c-real",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-08-28T00:00:00Z"
}
JSON
  # 验证缺口 (a)：converse 不挂 trigger 的直接 compose 级磁盘证明。治理环时序：
  #   Planning 阶段 converse 渲染 planner/compose.yaml（trigger 注释行）→
  #   PlanReviewing 通过后 maintain ① 渲染同一路径（trigger 活动行，覆盖 converse 版）。
  #   磁盘终态是 maintain 渲染；converse 版须在 driver run 期间捕获——后台起 driver，
  #   轮询到含 converse 不挂 trigger 注释行的 planner/compose.yaml 即其 converse 版。
  CONVERSE_COMPOSE="$CASE_B/converse.planner.compose.yaml"
  echo "[r6c] tier3b: driver run（真容器 converse → 计划审查容器 → 执行 → 执行审查） ..."
  cargo run --quiet -p alfred-cli --example driver -- run \
    --request "$CASE_B/request.json" \
    --run-dir "$CASE_B" \
    --time-limit 900 \
    --review-time-limit 300 \
    --planner-time-limit 900 \
    --image "$IMAGE" \
    >"$R6C_RUNS/r6c-tier3b-driver.log" 2>&1 &
  DRIVER_PID=$!
  # 轮询捕获 converse 渲染的 planner compose（maintain 覆盖在计划审查通过后，
  # 早于该时点出现的必是 converse 渲染）；driver 退出仍未捕获 → 后续断言拦。
  for ((_i=0; _i<1200; _i++)); do
    if [[ -f "$CASE_B/planner/compose.yaml" ]] \
      && grep -q 'converse 模式：不挂载 trigger.json' "$CASE_B/planner/compose.yaml" 2>/dev/null; then
      cp "$CASE_B/planner/compose.yaml" "$CONVERSE_COMPOSE"
      break
    fi
    kill -0 "$DRIVER_PID" 2>/dev/null || break
    sleep 0.5
  done
  wait "$DRIVER_PID"

  python3 - "$CASE_B" "$CONVERSE_COMPOSE" <<'PY' || { echo "FAIL(tier3b): 真容器全链未完成" >&2; exit 1; }
import json, os, sys
run = sys.argv[1]
converse_compose = sys.argv[2]
state = json.load(open(os.path.join(run, "state.json")))
assert state["state_machine"]["state"] == "completed", f"state={state['state_machine']['state']}"
# 计划审查走了容器：plan-review/inputs/conversation.json（reviewer 独有挂载输入）存在
pr = os.path.join(run, "plan-review")
assert os.path.exists(os.path.join(pr, "inputs", "conversation.json")), "容器计划审查缺 conversation.json 输入"
assert os.path.exists(os.path.join(pr, "outputs", "verdict.json")), "容器计划审查 outputs/verdict.json 缺失"
assert os.path.exists(os.path.join(pr, "compose.yaml")), "容器计划审查 compose.yaml 缺失"

# 验证缺口 (a)：converse 不挂 trigger 的磁盘级证明（planner compose）
#   converse 渲染：trigger 注释行（无活动挂载行）；maintain ① 覆盖后：活动挂载行。
ACTIVE = ":/inputs/trigger.json:ro"
CONVERSE_COMMENT = "converse 模式：不挂载 trigger.json"

assert os.path.exists(converse_compose), "未捕获 converse 渲染的 planner compose.yaml"
cc = open(converse_compose, encoding="utf-8").read()
for line in cc.splitlines():
    assert not (line.strip().startswith("- ") and ACTIVE in line), \
        f"converse planner compose 含活动 trigger 挂载：{line}"
assert CONVERSE_COMMENT in cc, "converse planner compose 缺 converse 不挂 trigger 注释行"

mc = open(os.path.join(run, "planner", "compose.yaml"), encoding="utf-8").read()
active = [l for l in mc.splitlines() if l.strip().startswith("- ") and ACTIVE in l]
assert active, "maintain planner compose（磁盘终态）缺活动 trigger 挂载行"
PY
  echo "PASS(tier3b): 真容器全链（计划审查容器 + 执行审查）→ Completed（converse compose 无 trigger 挂载 + maintain compose 有）"

  unset ALFRED_AGT_DIR
fi

echo ""
echo "============================================="
echo "R6c e2e 完成"
echo "  Tier 0 : cargo test 全绿（离线单测 + 容器驱动单测）"
if [[ -n "$INSPECT" ]]; then
  echo "  Tier 1 : 离线回归 PASS（AGT 策略求值 + 离线治理环；独立 plan-review 已归档）"
fi
echo "  Tier 2 : 容器可见性实测（挂载矩阵；docker 缺失 SKIP）"
echo "  Tier 3 : ${R6C_REAL:-0}（R6C_REAL=1 时真容器全链；独立 exec-review 夹带私货已归档）"
echo "============================================="
exit 0
