#!/usr/bin/env bash
# ============================================================================
# 等价 harness（重构方案 v2 步骤①）：同一离线确定性场景在 pre/post 两个 alfred
# 二进制上各跑一遍，规范化比对 audit/state/conversation/llm-calls/verdicts 全等
# + 每场景事件-副作用断言表（audit 事件序列 + 终态 + 升级来源）。
#
# 用法：
#   bash tests/e2e/equiv.sh [PRE_BIN] [POST_BIN] [场景...]
#     PRE_BIN / POST_BIN 缺省自动构建：
#       POST_BIN = 工作区 cargo build（被测/重构后代码）
#       PRE_BIN  = git worktree HEAD（重构前基线；worktree 复用
#                  /tmp/alfred-equiv-pre，已存在 HEAD 匹配 bin 则不重建）
#     场景缺省全部（SCENARIOS_ALL）。
#
# 判定（行为等价判据，方案 §4）：
#   - pre/post canonical dump 逐字节相等（规范化已剥 ts/updated_at/绝对路径）
#   - 每轮 audit 事件序列 == 场景断言表（两轮都断言——表即预期）
#   - 终态 state + escalation_source 符合场景断言
#
# 退出码：全 PASS=0；任一 FAIL=1；S5 环境缺失（docker/inspect）跳过不影响判定。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"
source "$REPO_ROOT/tests/e2e/harness/scenarios.sh"

PRE_BIN_ARG="${1:-}"
POST_BIN_ARG="${2:-}"
shift 2 2>/dev/null || true
SCENARIOS=("$@")
[[ ${#SCENARIOS[@]} -eq 0 ]] && SCENARIOS=("${SCENARIOS_ALL[@]}")

fail() { echo "FAIL(equiv): $*" >&2; exit 1; }
pass() { echo "PASS(equiv): $*"; }

# ---------------------------------------------------------------------------
# 二进制定位/构建
# ---------------------------------------------------------------------------
build_bin() { # <dir> → 输出 bin 路径
  (cd "$1" && cargo build --quiet -p alfred-cli --bin alfred) || fail "cargo build 失败（$1）"
  echo "$1/target/debug/alfred"
}

PRE_BIN="$PRE_BIN_ARG"
if [[ -z "$PRE_BIN" ]]; then
  WT=/tmp/alfred-equiv-pre
  BASE_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  git -C "$REPO_ROOT" worktree prune >/dev/null 2>&1 || true
  if git -C "$REPO_ROOT" worktree list --porcelain | grep -q "^worktree $WT"; then
    git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
    git -C "$REPO_ROOT" worktree add --detach "$WT" "$BASE_SHA" >/dev/null || fail "worktree add 失败"
  else
    rm -rf "$WT"
    git -C "$REPO_ROOT" worktree add --detach "$WT" "$BASE_SHA" >/dev/null || fail "worktree add 失败"
  fi
  echo "[equiv] 构建 HEAD 基线 bin（worktree ${WT}）..."
  PRE_BIN="$(build_bin "$WT")"
fi
[[ -x "$PRE_BIN" ]] || fail "PRE_BIN 不可执行: $PRE_BIN"

POST_BIN="$POST_BIN_ARG"
if [[ -z "$POST_BIN" ]]; then
  echo "[equiv] 构建被测 bin（工作区）..."
  POST_BIN="$(build_bin "$REPO_ROOT")"
fi
[[ -x "$POST_BIN" ]] || fail "POST_BIN 不可执行: $POST_BIN"
echo "[equiv] PRE : $PRE_BIN"
echo "[equiv] POST: $POST_BIN"

# ---------------------------------------------------------------------------
# mock provider（S4/S5 用；启动一次，两轮共用端口——config 在 run 目录外，
# canonical 不含；写 config 时按轮内根路径渲染 verdict 输出？不需要——verdict
# 输出路径经 env 注入的是绝对路径，pre/post 根不同 → mock 需按轮区分。
# 做法：mock 不看路径，verdict 输出路径由 pi 宿主侧决定；两轮各自 env 注入。）
# ---------------------------------------------------------------------------
MOCK_PORT="${ALFRED_MOCK_PORT:-18733}"
MOCK_PID=""
start_mock() {
  python3 tests/e2e/mock_provider.py "$MOCK_PORT" "$EQUIV_ROOT/mock-requests.jsonl" \
    '{"pass": true, "reason": "plan faithfully addresses the owner request"}' \
    >"$EQUIV_ROOT/mock.log" 2>&1 &
  MOCK_PID=$!
  for _ in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$MOCK_PORT/v1/chat/completions" \
      -H 'Content-Type: application/json' \
      -d '{"model":"mock-reviewer","messages":[{"role":"user","content":"hi"}]}' >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.3
  done
  fail "mock provider 未就绪（port=$MOCK_PORT）"
}
stop_mock() {
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  wait "$MOCK_PID" 2>/dev/null || true
  MOCK_PID=""
}
trap stop_mock EXIT

NEEDS_MOCK=false
for s in "${SCENARIOS[@]}"; do
  [[ "$s" == s4_exec_hard_error || "$s" == s5_exec_review_cycle ]] && NEEDS_MOCK=true
done
NEEDS_DOCKER=false
for s in "${SCENARIOS[@]}"; do
  [[ "$s" == s5_exec_review_cycle ]] && NEEDS_DOCKER=true
done

# S5 容器执行需要 inspect_ai——executor 的 driver.py 由宿主侧 python 跑，
# 必须锚到本仓 venv（与 r6d.sh 同一前置；PATH python3 无 inspect_ai）。
export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"

write_mock_config() { # <out-config.yml>
  sed "s/%MOCK_PORT%/$MOCK_PORT/" > "$1" <<YAML
providers:
  mock:
    base_url: "http://127.0.0.1:%MOCK_PORT%/v1"
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
}

# ---------------------------------------------------------------------------
# 场景驱动原语（被 scenarios.sh 引用）
# ---------------------------------------------------------------------------
TS="$(date +%Y%m%d-%H%M%S)"
EQUIV_ROOT="$REPO_ROOT/tests/e2e/.runs/equiv-$TS"
mkdir -p "$EQUIV_ROOT"
LOG="$EQUIV_ROOT/logs"
mkdir -p "$LOG"

run_run() { # <run_dir> <request.json> — alfred run（driver 黑盒）
  local rd="$1" req="$2" tag
  tag="$(basename "$(dirname "$rd")")"
  local log="$LOG/$tag.run.out"
  ( cd "$SCEN_ROOT" && "$ALFRED_BIN" run --request "$req" --run-dir "$rd" \
    --time-limit 300 --review-time-limit 60 --planner-time-limit 60 --no-ctl \
  ) > "$log" 2>&1 || fail "run 失败（${tag}；见 ${log}）"
}
run_feed() { # <run_dir> <decision> <message>
  local rd="$1" d="$2" m="$3" tag seq_no="${4:-0}"
  tag="$(basename "$(dirname "$rd")").feed-$d-$seq_no"
  local args=(--run-dir "$rd" --decision "$d")
  [[ -n "$m" ]] && args+=(--message "$m")
  (cd "$SCEN_ROOT" && "$ALFRED_BIN" feed "${args[@]}") > "$LOG/$tag.out" 2>&1 \
    || fail "feed ${d} 失败（${tag}；见 $LOG/${tag}.out）"
}

expect_audit() { # <run_dir> <tag> <空格分隔期望序列>
  local rd="$1" tag="$2"; shift 2
  local expected="$*"
  python3 - "$rd" "$expected" <<'PY' || fail "audit 序列断言失败（${tag}；${rd}）"
import json, sys
run_dir, expected = sys.argv[1], sys.argv[2].split()
events = []
with open(run_dir + "/audit.jsonl", encoding="utf-8") as f:
    for line in f:
        line = line.strip()
        if line:
            events.append(json.loads(line)["event"])
if events != expected:
    print(f"  期望: {expected}\n  实际: {events}", file=sys.stderr)
    sys.exit(1)
PY
}

expect_state() { # <run_dir> <expected_state> <expected_escalation_source|"">
  local rd="$1" want="$2" want_src="$3"
  python3 - "$rd" "$want" "$want_src" <<'PY' || fail "state 断言失败（${rd}）"
import json, sys
run_dir, want, want_src = sys.argv[1], sys.argv[2], sys.argv[3]
st = json.load(open(run_dir + "/state.json", encoding="utf-8"))
got = st["state_machine"]["state"]
src = st.get("escalation_source")
if got != want:
    print(f"  state={got}（期望 {want}）", file=sys.stderr); sys.exit(1)
if want_src and src != want_src:
    print(f"  escalation_source={src}（期望 {want_src}）", file=sys.stderr); sys.exit(1)
PY
}

expect_field() { # <run_dir> <json-key> <expected>
  local rd="$1" key="$2" want="$3"
  python3 - "$rd" "$key" "$want" <<'PY' || fail "field 断言失败（${rd}.${key}）"
import json, sys
run_dir, key, want = sys.argv[1], sys.argv[2], sys.argv[3]
st = json.load(open(run_dir + "/state.json", encoding="utf-8"))
got = str(st.get(key))
if got != want:
    print(f"  {key}={got}（期望 {want}）", file=sys.stderr); sys.exit(1)
PY
}

canon() { # <run_dir> <scenario>
  python3 "$REPO_ROOT/tests/e2e/harness/normalize.py" "$1" "$REPO_ROOT" \
    > "$SCEN_ROOT/$2.canonical.json" || fail "canonical 生成失败（$2）"
}

export -f run_run run_feed expect_audit expect_state expect_field canon 2>/dev/null || true
export EQUIV_ROOT LOG

# S5 环境门槛
if $NEEDS_DOCKER; then
  if [[ ! -x ".plans/r0-lab/venv/bin/inspect" ]] && ! command -v inspect >/dev/null 2>&1; then
    echo "SKIP(equiv): S5 需 inspect CLI（缺失）；其余场景继续。"
    SCENARIOS=("${SCENARIOS[@]/s5_exec_review_cycle/}")
    SCENARIOS=("${SCENARIOS[@]}")
    NEEDS_DOCKER=false
  elif ! docker info >/dev/null 2>&1; then
    echo "SKIP(equiv): S5 需 docker（不可用）；其余场景继续。"
    NEEDS_DOCKER=false
  fi
fi
export ALFRED_INSPECT="${ALFRED_INSPECT:-$REPO_ROOT/.plans/r0-lab/venv/bin/inspect}"

if $NEEDS_MOCK && [[ -z "$MOCK_PID" ]]; then
  start_mock
fi

# ---------------------------------------------------------------------------
# 主循环：每场景 pre/post 各跑一轮 + canonical diff
# ---------------------------------------------------------------------------
FAILED=()
for s in "${SCENARIOS[@]}"; do
  [[ -z "$s" ]] && continue
  echo "[equiv] 场景 $s ..."
  ok=true
  for round in pre post; do
    bin="$PRE_BIN"; [[ "$round" == post ]] && bin="$POST_BIN"
    SCEN_ROOT="$EQUIV_ROOT/$round/$s"
    mkdir -p "$SCEN_ROOT"
    write_fixtures "$SCEN_ROOT"
    ALFRED_BIN="$bin"
    export ALFRED_BIN SCEN_ROOT
    "$s" || ok=false
    unset ALFRED_BIN SCEN_ROOT
    # 清 env 防跨场景泄漏（场景函数自身也 unset，双保险）
    unset ALFRED_OFFLINE ALFRED_OFFLINE_PLAN_FILE ALFRED_OFFLINE_REPLY_FILE \
          ALFRED_MAINTAIN_OFFLINE_FILE ALFRED_PLANNER_OFFLINE ALFRED_EXEC_REVIEW_OFFLINE \
          ALFRED_CONFIG ALFRED_REVIEWER_MODEL 2>/dev/null || true
  done
  if diff -u "$EQUIV_ROOT/pre/$s/$s.canonical.json" "$EQUIV_ROOT/post/$s/$s.canonical.json" \
      > "$EQUIV_ROOT/$s.diff"; then
    rm -f "$EQUIV_ROOT/$s.diff"
    $ok && pass "$s 等价（canonical 逐字节一致 + 断言表全过）" || FAILED+=("$s(断言)")
  else
    echo "  canonical diff（前 40 行）:"; sed -n '1,40p' "$EQUIV_ROOT/$s.diff"
    FAILED+=("$s(canonical)")
  fi
done

echo "[equiv] 产物根: $EQUIV_ROOT"
if [[ ${#FAILED[@]} -gt 0 ]]; then
  echo "FAIL(equiv): ${FAILED[*]}" >&2
  exit 1
fi
pass "全部场景等价（${SCENARIOS[*]}）"
