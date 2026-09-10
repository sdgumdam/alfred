#!/usr/bin/env bash
# ============================================================================
# multinode e2e（M4）：2 节点全流程固化——拓扑执行序 / 共享 ws 传递 / 执行审查
# 全节点契约进审查输入。
#
# M3 验证跑过的 2 节点场景（notes.md→report.md）固化为常驻用例；M4 补上执行
# 审查的多节点适配验收（全节点契约进审查输入——旧形态 nodes.first() 对全 ws
# 判产物 = 漏审下游节点）。
#
# 两层：
#   Tier 0（默认，无外部依赖）：cargo test —— 离线单测（含 M4 执行审查输入
#     投影单测 crates/alfred-reviewer/tests/exec_review_inputs.rs）。
#   Tier 1（需 inspect + docker + pi，无需真 LLM）：离线 2 节点全流程——
#     1. 指令注入 2 节点+edge（ALFRED_PLANNER_OFFLINE=1 + instructions.json，
#        故意先声明后继 task-2 再声明前置 task-1——拓扑序重排一并黑盒断言）；
#     2. 计划审查 pass 注入（mock provider 驱动宿主 pi 经 write 工具循环写
#        plan-review/outputs/verdict.json）；
#     3. 容器真实执行 ×2（executor=mock provider 脚本化：task-1 写
#        /workspace/notes.md；task-2 read notes.md → 从读到的真实内容派生
#        report.md——共享 ws 传递走真实数据流，非 mock 内常量）；
#     4. 执行审查（mock 写 grade=C → §3.3 路由 Advance）→ Completed。
#     断言（黑盒，只读 run 产物）：
#       a. 拓扑执行序：audit node_started 序列 task-1→task-2 + exec-1/exec-2
#          task_id 对应（执行顺序 ≠ 声明顺序）；
#       b. 共享 ws 传递：ws/src/notes.md 存在（task-1 产物）+ ws/src/report.md
#          由 notes 要点派生（task-2 产物基于 task-1 产物）；
#       c. 执行审查全节点契约（M4 核心）：exec-review/inputs/dagspec.json 含
#          两节点 prompt/验收标准 + edges；且无 inputs/contract.json（首节点
#          契约投影的漏审形态必须消失）；
#       d. verdict=C 路由 Advance → Completed + completed_nodes=[task-1,task-2]。
#
# 模型：mock provider（本地 OpenAI 兼容端点，无真 LLM）——planner 离线直通、
#   reviewer（计划/执行审查）与 executor 都走 mock（executor 经
#   ALFRED_MOCK_EXECUTOR_SCRIPT 脚本化，见 mock_provider.py 头注释）。
# 驱动：黑盒经真实 `alfred` bin（codux 可调度 CLI driver：run）。
# 运行：bash tests/e2e/multinode.sh（inspect/docker/沙箱镜像缺失时 Tier 1 SKIP）
# 验收：Tier 0 全绿 + Tier 1 PASS（或环境缺失 SKIP）。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
# --- 容器驱动 Python（P1）：ALFRED_PYTHON 优先，否则本仓 venv ---
if [[ -z "${ALFRED_PYTHON:-}" && -x "$REPO_ROOT/.plans/r0-lab/venv/bin/python" ]]; then
  export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"
fi

echo "============================================="
echo "multinode Tier 0：cargo test（离线单测，含 M4 执行审查输入投影）"
echo "============================================="
cargo test --quiet
echo "PASS(Tier0): cargo test 全绿"

# --- inspect CLI / docker / 沙箱镜像 / pi（Tier 1 前置） ---
INSPECT="${ALFRED_INSPECT:-}"
if [[ -z "$INSPECT" ]]; then
  if [[ -x "$REPO_ROOT/.plans/r0-lab/venv/bin/inspect" ]]; then
    INSPECT="$REPO_ROOT/.plans/r0-lab/venv/bin/inspect"
  else
    INSPECT="$(command -v inspect || true)"
  fi
fi

TIER1_SKIP=""
if [[ -z "$INSPECT" ]]; then
  TIER1_SKIP="无 inspect CLI（ALFRED_INSPECT 或 PATH）"
elif ! command -v docker >/dev/null 2>&1; then
  TIER1_SKIP="无 docker（执行步骤需沙箱）"
elif ! docker image inspect alfred-executor:latest >/dev/null 2>&1; then
  TIER1_SKIP="无沙箱镜像 alfred-executor:latest"
elif ! command -v pi >/dev/null 2>&1; then
  TIER1_SKIP="无 pi CLI（宿主审查驱动需要）"
fi

if [[ -n "$TIER1_SKIP" ]]; then
  echo ""
  echo "SKIP(Tier1): ${TIER1_SKIP}。离线 2 节点全流程 e2e 跳过（Tier 0 已覆盖单测）。"
  echo ""
  echo "============================================="
  echo "multinode e2e 完成"
  echo "  Tier 0 : cargo test 全绿"
  echo "  Tier 1 : SKIP（${TIER1_SKIP}）"
  echo "============================================="
  exit 0
fi

echo ""
echo "============================================="
echo "multinode Tier 1：离线 2 节点全流程（无真 LLM；mock 驱动审查与执行）"
echo "============================================="
export ALFRED_INSPECT="$INSPECT"
echo "[multinode] inspect CLI : $INSPECT"
echo "[multinode] pi CLI     : $(command -v pi)"

RUNS="$REPO_ROOT/tests/e2e/.runs"
CASE="$RUNS/run-multinode-m4"
rm -rf "$CASE"
mkdir -p "$CASE"

# --- mock provider（执行者脚本模式：task-1 写 notes / task-2 读 notes 派生 report；
#     执行审查 verdict 分流：/exec-review/ → grade C） ---
MOCK_PORT="${ALFRED_MOCK_PORT:-18941}"
MOCK_PID=""
cleanup_mock() {
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
}
trap cleanup_mock EXIT
ALFRED_MOCK_EXECUTOR_SCRIPT=1 \
python3 tests/e2e/mock_provider.py "$MOCK_PORT" "$CASE/mock-requests.jsonl" \
  '{"pass": true, "reason": "plan faithfully addresses the owner request"}' \
  >"$CASE/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 40); do
  if curl -sf "http://127.0.0.1:$MOCK_PORT/" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

cat > "$CASE/config.yml" <<YAML
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

# --- 夹具：属主请求（notes → report）+ 建图指令（2 节点 + edge；故意先声明
#     后继 task-2 再声明前置 task-1，拓扑序重排随断言黑盒覆盖） ---
cat > "$CASE/request.json" <<'JSON'
{
  "id": "req-m4-mn",
  "title": "notes then report",
  "description": "First create notes.md summarizing the key points as bullet items. Then, based on notes.md, create report.md covering the same points as full sentences.",
  "acceptance_criteria": "notes.md exists with bullet items; report.md exists and covers the same points as full sentences",
  "created_at": "2026-09-10T00:00:00Z"
}
JSON
cat > "$CASE/instructions.json" <<'JSON'
[
  {"op":"begin","request_id":"req-m4-mn"},
  {"op":"add_node","id":"task-2","summary":"write report.md from notes.md",
   "contract":{"prompt":"Based on the notes.md produced by the prerequisite task, create report.md in the workspace covering the same points as full sentences.","acceptance_criteria":"report.md exists and covers the same points as full sentences"},
   "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}},
  {"op":"add_node","id":"task-1","summary":"write notes.md bullet points",
   "contract":{"prompt":"Create notes.md in the workspace summarizing the key points as bullet items.","acceptance_criteria":"notes.md exists and contains bullet items"},
   "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}},
  {"op":"add_edge","from":"task-1","to":"task-2"},
  {"op":"commit"}
]
JSON

export ALFRED_CONFIG="$CASE/config.yml"
export ALFRED_REVIEWER_MODEL="mock-reviewer"
export ALFRED_PLANNER_OFFLINE=1
export ALFRED_OFFLINE_PLAN_FILE="$CASE/instructions.json"

echo "[multinode] driver run（离线指令注入 2 节点+edge → 计划审查 mock pass → 容器执行×2 → 执行审查 mock grade=C → Completed） ..."
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request "$CASE/request.json" \
  --run-dir "$CASE" \
  --time-limit 300 \
  --review-time-limit 90 \
  --planner-time-limit 60 \
  --no-ctl

unset ALFRED_CONFIG ALFRED_REVIEWER_MODEL ALFRED_PLANNER_OFFLINE ALFRED_OFFLINE_PLAN_FILE
trap - EXIT
kill "$MOCK_PID" 2>/dev/null || true
wait "$MOCK_PID" 2>/dev/null || true
MOCK_PID=""

# --- 黑盒断言（只读 run 产物） ---
python3 - "$CASE" <<'PY' || { echo "FAIL(Tier1): 多节点全流程断言" >&2; exit 1; }
import json, os, sys

run = sys.argv[1]
def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)

# ============ a. 拓扑执行序 ============
state = load(os.path.join(run, "state.json"))
assert state["state_machine"]["state"] == "completed", \
    f"state={state['state_machine']['state']}（执行审查 C 应路由 Advance → Completed）"
assert state["completed_nodes"] == ["task-1", "task-2"], \
    f"completed_nodes={state['completed_nodes']}"
assert state["execution_count"] == 2, f"execution_count={state['execution_count']}"

events = []
with open(os.path.join(run, "audit.jsonl"), encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if line:
            events.append(json.loads(line))

started = [e["data"]["node_id"] for e in events if e["event"] == "node_started"]
# 声明序是 task-2 在前——拓扑执行序必须 task-1 → task-2（依赖边 from 先于 to）。
assert started == ["task-1", "task-2"], f"node_started 序列={started}（应按拓扑序 task-1→task-2）"
completed = [e["data"]["node_id"] for e in events if e["event"] == "node_completed"]
assert completed == ["task-1", "task-2"], f"node_completed 序列={completed}"
assert sum(1 for e in events if e["event"] == "execution_succeeded") == 1, "execution_succeeded 应恰一次（全图完成门）"
assert any(e["event"] == "plan_review_passed" for e in events), "audit 缺 plan_review_passed（计划审查 pass 注入失败）"
assert any(e["event"] == "exec_review_passed" for e in events), "audit 缺 exec_review_passed（执行审查 C 路由失败）"

# exec-N 目录与节点的对应（执行顺序 exec-1=task-1 → exec-2=task-2）。
for n, (exec_dir, task_id) in enumerate(
    [("exec-1", "task-1"), ("exec-2", "task-2")], start=1
):
    es = load(os.path.join(run, exec_dir, "state.json"))["run"]
    assert es["task_id"] == task_id, f"{exec_dir} task_id={es['task_id']}（应 {task_id}）"
    assert es["eval_status"] == "success", f"{exec_dir} eval_status={es['eval_status']}"
print("  a. 拓扑执行序 PASS：node_started task-1→task-2（声明序 task-2 在前）+ exec-1/exec-2 对应")

# ============ b. 共享 ws 传递（task-2 产物基于 task-1 产物） ============
notes_path = os.path.join(run, "ws", "src", "notes.md")
report_path = os.path.join(run, "ws", "src", "report.md")
assert os.path.isfile(notes_path), f"task-1 产物缺失: {notes_path}"
assert os.path.isfile(report_path), f"task-2 产物缺失: {report_path}"
notes = open(notes_path, encoding="utf-8").read()
report = open(report_path, encoding="utf-8").read()
# task-1 产物 = mock 固定 bullet 要点（写进 /workspace = ws/src）。
expected_points = [
    "Alpha point about the shared workspace mount semantics.",
    "Beta point about the topological execution order.",
    "Gamma point about the multi-node artifact passing.",
]
for point in expected_points:
    assert point in notes, f"notes.md 缺要点: {point}"
    assert point in report, f"report.md 缺要点（task-2 未基于 task-1 产物派生）: {point}"
# task-2 的产物由 read 结果真实派生（mock 从工具结果提取要点改写——若 read 未
# 见 task-1 产物，mock 不会写出 report.md，上面 isfile 断言已挡）。
assert "# Report" in report and "full sentence" in report, \
    f"report.md 非派生形态: {report[:200]}"
# 节点产物归属：exec-N artifact changes 各含本节点产物路径。
for exec_dir, rel in [("exec-1", "src/notes.md"), ("exec-2", "src/report.md")]:
    es = load(os.path.join(run, exec_dir, "state.json"))["run"]
    changes = [c["path"] for c in (es.get("artifact") or {}).get("changes", [])]
    assert rel in changes, f"{exec_dir} artifact changes 缺 {rel}: {changes}"
print("  b. 共享 ws 传递 PASS：task-2 report.md 由 task-1 notes.md 要点派生（真实 read→write 数据流）")

# ============ c. 执行审查全节点契约进审查输入（M4 核心） ============
inputs_dir = os.path.join(run, "exec-review", "inputs")
dag_path = os.path.join(inputs_dir, "dagspec.json")
assert os.path.isfile(dag_path), f"执行审查输入缺 dagspec.json（多节点全节点契约）: {dag_path}"
dag = load(dag_path)
nodes = {n["id"]: n for n in dag["nodes"]}
assert set(nodes) == {"task-1", "task-2"}, f"审查输入节点={list(nodes)}"
# 两节点契约都进审查输入（旧形态只进首节点契约 = 漏审下游）。
assert nodes["task-1"]["contract"]["prompt"].startswith("Create notes.md"), \
    f"task-1 prompt 未进审查输入: {nodes['task-1']['contract']['prompt'][:80]}"
assert nodes["task-1"]["contract"]["acceptance_criteria"] == "notes.md exists and contains bullet items"
assert nodes["task-2"]["contract"]["prompt"].startswith("Based on the notes.md produced by the prerequisite task"), \
    f"task-2 prompt 未进审查输入: {nodes['task-2']['contract']['prompt'][:80]}"
assert nodes["task-2"]["contract"]["acceptance_criteria"] == \
    "report.md exists and covers the same points as full sentences"
# 产物归属随节点进审查输入（该节点 workspace_subdirs）。
assert nodes["task-1"]["sandbox"]["workspace_subdirs"] == ["src"]
assert nodes["task-2"]["sandbox"]["workspace_subdirs"] == ["src"]
# 依赖序随图进审查输入。
assert dag["edges"] == [{"from": "task-1", "to": "task-2"}], f"edges={dag['edges']}"
# 漏审形态必须消失：多节点输入不得再含首节点契约投影 / 单节点挂载语义。
assert not os.path.exists(os.path.join(inputs_dir, "contract.json")), \
    "inputs/contract.json 不应存在（首节点契约投影 = M4 修的漏审形态）"
assert not os.path.exists(os.path.join(inputs_dir, "sandbox.json")), \
    "inputs/sandbox.json 不应存在（产物归属已随 dagspec 节点进输入）"
assert os.path.isfile(os.path.join(inputs_dir, "request.json")), "inputs/request.json 缺失"
# run 级证据副本同源（P9 证据：审查主输入 = 全图）。
assert os.path.isfile(os.path.join(run, "exec-review", "dagspec.json")), \
    "exec-review/dagspec.json 证据副本缺失"
print("  c. 执行审查全节点契约 PASS：inputs/dagspec.json 含两节点契约+产物归属+edges；无首节点契约投影")

# ============ d. 执行审查 verdict=C → 路由 Advance → Completed ============
vd = load(os.path.join(run, "exec-review", "verdict.json"))
assert vd["verdict"] is not None and vd["verdict"]["value"] == "C", \
    f"exec verdict={vd['verdict']}"
assert vd["unscored_reason"] is None, f"unscored_reason={vd['unscored_reason']}"
archive = load(os.path.join(run, "exec-verdicts.json"))
assert any(v.get("value") == "C" for v in archive), f"exec-verdicts.json 无 C 归档: {archive}"
# 计划审查 pass 注入真走了宿主 pi。
assert os.path.isfile(os.path.join(run, "plan-review", "outputs", "verdict.json")), \
    "plan-review/outputs/verdict.json 缺失"
print("  d. 执行审查 verdict PASS：grade=C → 路由 Advance → Completed + exec-verdicts 归档")
PY

echo "PASS(Tier1): 离线 2 节点全流程——拓扑执行序 + 共享 ws 传递 + 执行审查全节点契约 + C 路由 Completed"

echo ""
echo "============================================="
echo "multinode e2e 完成"
echo "  Tier 0 : cargo test 全绿（含 M4 执行审查输入投影单测）"
echo "  Tier 1 : 离线 2 节点全流程 PASS（产物: ${CASE}）"
echo "============================================="
exit 0
