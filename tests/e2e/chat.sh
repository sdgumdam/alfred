#!/usr/bin/env bash
# ============================================================================
# chat e2e：alfred chat —— owner 持续会话（编排器 owner 接口层）黑盒验证
#
# 驱动：真实 `alfred chat` bin（codux 可调度的 owner 持续会话进程），printf 多行
# stdin 喂入，断言 stdout 阶段标记顺序 + run 目录产物（state.json /
# conversation.json / audit.jsonl）+ EOF 退出。一律黑盒：不掏内部实现。
#
# 默认全离线（ALFRED_OFFLINE=1 确定性直通，无 docker 无 LLM；模型配置只加载不
# 调用，前置与 r4 相同：~/.config/alfred/config.yml 或 env 覆盖）：
#
#   case1 需求收集：需求+验收标准("按需求") → OwnerRequest 确定性转写（id=chat-<ts>，
#         title 首行）→ 建 run → planner 答复分支（ALFRED_OFFLINE_REPLY_FILE）→
#         [pi] 答复 → Planning 停驻 → EOF 退出
#   case2 全流程对话（断点恢复 + 拍板多轮 + 终态新需求）：
#         恢复 Planning run → 属主消息 → 建图（[orchestrator] 流转状态行 +
#         [pi] 计划摘要）→ 挂起升级包 → 拍板：重试（Retry）/ "不要重试，改成X"
#         （边界：整行 Revise 不误路由）/ "别放弃…"（边界：Revise）/
#         "放弃吧，重试也没用"（边界：两词同现，精确匹配不命中 → Revise）/
#         "算了"（边界：Revise）→ bare "放弃"（Abandon）→ Abandoned 终态呈现 →
#         新需求回需求收集态 → 新 run 建立（离线注入 request_id 不匹配 →
#         planning_error 升级——后续 case3 以重试续跑闭环）
#   case3 run 发现 + 重试闭环：无 --run-dir 自动发现 updated_at 最新 run（run2，
#         escalated）→ 升级包 → 重试 → 重规划（补配 request_id 的离线计划）→
#         建图成功 → 挂起（audit 有 planning_done + Retry 轮）
#   case4 多挂起消歧：再造一个挂起 run → 无 --run-dir 时发现 2 个挂起 run →
#         列出清单要求 --run-dir（非零退出）
#
#   case5 Planning 态放弃出口（P1-2）：需求收集 → Planning → 整行精确"放弃" →
#         Abandoned（属主放弃恒可选，owner 唯一入口可达）
#   case6 PlanRejected 打回（R6e 结构闸门离线确定性命中）：打回意见呈现
#         （plan_verdicts.last 原始 reason + 产物摘要）→ 重试（P3a 伪装重规划，
#         断言 disguised + 禁词净化轮）→ 放弃

# CHAT_REAL=1 附加真容器真 LLM REPL 用例（照 r3 真容器模式分层，需 docker +
# 沙箱镜像 + config.yml 凭据）：stdin 喂需求 → [pi] 真答复/建图 → 改口 →
# 建图 → 挂起/终态断言 + conversation.json ConverseReply 真轮次 + llm-calls 证据。
# R6 教训对冲：形态修复切片必须与真 LLM 对话至少一次。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TS="$(date +%Y%m%d-%H%M%S)"
CHAT_RUNS="$REPO_ROOT/tests/e2e/.runs/chat-$TS"
LOG="$CHAT_RUNS/logs"
FIX="$CHAT_RUNS/fixtures"
mkdir -p "$LOG" "$FIX"
STATE="$CHAT_RUNS/state"          # ALFRED_STATE_DIR（runs 基目录，隔离本机真实 run）

# --- 模型（glm-5.3-flash 省钱；离线只加载不调用） ---
export ALFRED_PLANNER_MODEL="${ALFRED_PLANNER_MODEL:-glm-5.3-flash}"
export ALFRED_EXECUTOR_MODEL="${ALFRED_EXECUTOR_MODEL:-glm-5.3-flash}"
export ALFRED_REVIEWER_MODEL="${ALFRED_REVIEWER_MODEL:-glm-5.3-flash}"
export ALFRED_STATE_DIR="$STATE"

ALFRED() { cargo run --quiet -p alfred-cli --bin alfred -- "$@"; }

# --- 断言工具（黑盒：stdout 标记 + 落盘产物读回） ---
fail() { echo "FAIL($1): $2" >&2; exit 1; }
pass() { echo "PASS($1): $2"; }
has() { # <file> <marker> <case>
  grep -q -- "$2" "$1" || fail "$3" "缺标记 [$2]（$1）"
}
assert_order() { # <file> <case> <marker...>（依次出现且递增）
  local file="$1" case="$2"; shift 2
  local prev=0 ln m
  for m in "$@"; do
    ln=$(grep -n -m1 -- "$m" "$file" | cut -d: -f1 || true)
    [[ -n "$ln" ]] || fail "$case" "缺标记 [$m]"
    [[ "$ln" -gt "$prev" ]] || fail "$case" "标记乱序 [$m]（$ln <= ${prev}）"
    prev=$ln
  done
}
state_of() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['state_machine']['state'])" "$1/state.json"; }
assert_state() { # <run_dir> <expected> <case>
  local got; got="$(state_of "$1")"
  [[ "$got" == "$2" ]] || fail "$3" "$1 state=$got 期望 $2"
}
conv_has_owner_msg() { # <run_dir> <content>（conversation.json owner.message 轮整行）
  python3 - "$1" "$2" <<'PY'
import json, sys
log = json.load(open(sys.argv[1] + "/conversation.json"))
hit = any(t["role"] == "owner" and t["source"] == "owner.message" and t["content"] == sys.argv[2]
          for t in log["turns"])
sys.exit(0 if hit else 1)
PY
}
audit_has() { # <run_dir> <substring>
  grep -q -- "$2" "$1/audit.jsonl"
}
write_plan_fixture() { # <path> <request_id>
  cat > "$1" <<JSON
{
  "request_id": "$2",
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

echo "[chat-e2e] 离线黑盒：state=$STATE"

# ============================================================================
# Case 1：需求收集 → planner 答复分支 → Planning 停驻 → EOF 退出
# ============================================================================
CASE1="$STATE/run-chat-case1"
mkdir -p "$CASE1"
printf 'pi 需要先澄清吗\n' > "$FIX/reply1.txt"
printf '写一个 hello.txt 内容是 Hello\n按需求\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$FIX/reply1.txt" \
  ALFRED chat --run-dir "$CASE1" > "$LOG/case1.out" 2> "$LOG/case1.err"
[[ -f "$CASE1/state.json" ]] || fail case1 "run 未建立"
assert_state "$CASE1" "planning" case1
has "$LOG/case1.out" "需求已确定性转写" case1
has "$LOG/case1.out" "验收标准" case1
has "$LOG/case1.out" "\[pi\] pi 需要先澄清吗" case1
has "$LOG/case1.out" "新建 run: $CASE1" case1
C1_REQ_ID="$(python3 -c "import json;print(json.load(open('$CASE1/request.json'))['id'])")"
[[ "$C1_REQ_ID" == chat-* ]] || fail case1 "request id 非 chat-<ts>：$C1_REQ_ID"
python3 - "$CASE1" <<'PY' || fail case1 "title 应为首行原文，验收标准=需求原文"
import json, sys
r = json.load(open(sys.argv[1] + "/request.json"))
assert r["title"] == "写一个 hello.txt 内容是 Hello", r["title"]
assert r["acceptance_criteria"] == r["description"], "按需求 → 验收标准=需求原文"
PY
python3 - "$CASE1" <<'PY' || fail case1 "conversation.json 缺 request.submit 首轮"
import json, sys
log = json.load(open(sys.argv[1] + "/conversation.json"))
assert log["turns"][0]["source"] == "request.submit", log["turns"][0]
PY
pass case1 "需求收集+确定性转写+建 run+[pi] 答复 → Planning（id=${C1_REQ_ID}）"

# ============================================================================
# Case 2：断点恢复 → 建图 → [orchestrator] 流转 → 挂起拍板（含 P1 边界）→
#         Abandoned 终态 → 新需求回需求收集态（新 run 建立）
# ============================================================================
write_plan_fixture "$FIX/plan.json" "$C1_REQ_ID"
printf '直接建图\n重试\n不要重试，改成写 goodbye.txt\n别放弃，就按这个计划\n放弃吧，重试也没用\n算了\n放弃\n写 goodbye.txt 内容是 Bye\ngoodbye.txt 存在且内容为 Bye\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$FIX/plan.json" \
  ALFRED chat --run-dir "$CASE1" > "$LOG/case2.out" 2> "$LOG/case2.err"
assert_order "$LOG/case2.out" case2 \
  "恢复 run" \
  "计划审查中" \
  "已升级属主" \
  "治理挂起，等待属主拍板" \
  "run 已放弃（Abandoned）" \
  "新需求请直接说" \
  "新建 run: " \
  "规划失败已升级属主"
# 终态后新 run 的升级包（planning 来源）与拍板提示呈现
has "$LOG/case2.out" "升级来源: Some(Planning)" case2
has "$LOG/case2.out" "回复：重试 / 放弃 / 或直接说修改意见" case2
assert_state "$CASE1" "abandoned" case2
# 建图后计划摘要以 [pi] 呈现（P2：ConverseReply 语义轮单一真源）
has "$LOG/case2.out" "\[pi\] 计划（1 节点）" case2
# 拍板边界（P1）：整行精确匹配——否定/两词同现/算了 全部落 Revise 且整行作属主消息
conv_has_owner_msg "$CASE1" "不要重试，改成写 goodbye.txt" || fail case2 "'不要重试…' 应整行 Revise"
conv_has_owner_msg "$CASE1" "别放弃，就按这个计划" || fail case2 "'别放弃…' 应整行 Revise"
conv_has_owner_msg "$CASE1" "放弃吧，重试也没用" || fail case2 "两词同现应精确匹配不命中 → Revise"
conv_has_owner_msg "$CASE1" "算了" || fail case2 "'算了' 应整行 Revise"
audit_has "$CASE1" '"decision":"Retry"' || fail case2 "bare 重试 应路由 Retry"
audit_has "$CASE1" '"decision":"Abandon"' || fail case2 "bare 放弃 应路由 Abandon"
[[ "$(grep -c '"decision":"Revise"' "$CASE1/audit.jsonl")" -ge 4 ]] || fail case2 "边界句应产生 ≥4 次 Revise"
# 终态后的新需求 → 新 run（默认治理目录，不覆盖终态 run 目录）
C2_NEW_RUN="$(grep '新建 run: ' "$LOG/case2.out" | sed 's/.*新建 run: //' | tail -1)"
pass case2 "建图→流转→升级包→重试/修改/放弃拍板→终态→新需求回收集态（新 run: ${C2_NEW_RUN}）"
[[ "$C2_NEW_RUN" != "$CASE1" ]] || fail case2 "新 run 不应覆盖终态 run 目录"
assert_state "$C2_NEW_RUN" "escalated" case2
[[ -f "$C2_NEW_RUN/request.json" ]] || fail case2 "新 run 缺 request.json"
audit_has "$C2_NEW_RUN" "planning_error_escalated" || fail case2 "新 run 离线注入 id 不匹配应 planning 升级"

# ============================================================================
# Case 3：run 发现（无 --run-dir，updated_at 最新）+ 重试闭环（planning 来源
#         retry → 重规划 → 建图成功）
# ============================================================================
write_plan_fixture "$FIX/plan-run2.json" \
  "$(python3 -c "import json;print(json.load(open('$C2_NEW_RUN/request.json'))['id'])")"
printf '重试\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$FIX/plan-run2.json" \
  ALFRED chat > "$LOG/case3.out" 2> "$LOG/case3.err"
C3_RUN_ID="$(python3 -c "import json;print(json.load(open('$C2_NEW_RUN/state.json'))['run_id'])")"
has "$LOG/case3.out" "恢复 run $C3_RUN_ID" case3
has "$LOG/case3.out" "run_dir: $C2_NEW_RUN" case3
has "$LOG/case3.out" "\[pi\] 计划（1 节点）" case3
has "$LOG/case3.out" "已升级属主" case3
assert_state "$C2_NEW_RUN" "escalated" case3
audit_has "$C2_NEW_RUN" "planning_done" || fail case3 "重试后重规划应建图成功"
audit_has "$C2_NEW_RUN" '"decision":"Retry"' || fail case3 "重试轮应落 Retry 决策审计"
pass case3 "自动发现最新挂起 run + 重试 → 重规划建图 → 挂起（run=${C3_RUN_ID}）"

# ============================================================================
# Case 4：多挂起 run 并存 → 列出清单要求 --run-dir（非零退出，不猜）
# ============================================================================
CASE4="$STATE/run-chat-case4"
mkdir -p "$CASE4"
# 快速造第二个挂起 run：离线注入指向不存在的计划文件 → planning_error 升级
printf '再造一个挂起 run\n按需求\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$FIX/definitely-missing.json" \
  ALFRED chat --run-dir "$CASE4" > "$LOG/case4a.out" 2> "$LOG/case4a.err"
assert_state "$CASE4" "escalated" case4
set +e
printf '' | ALFRED chat > "$LOG/case4b.out" 2> "$LOG/case4b.err"
C4_RC=$?
set -e
[[ "$C4_RC" -ne 0 ]] || fail case4 "多挂起并存应非零退出要求 --run-dir"
has "$LOG/case4b.err" "挂起 run" case4
has "$LOG/case4b.err" "$C2_NEW_RUN" case4
has "$LOG/case4b.err" "$CASE4" case4
pass case4 "多挂起消歧（列出 2 个挂起 run 要求 --run-dir，exit=${C4_RC}）"


# ============================================================================
# Case 5：Planning 态放弃出口（P1-2：属主放弃恒可选，owner 唯一入口上必须可达）
# ============================================================================
CASE5="$STATE/run-chat-case5"
mkdir -p "$CASE5"
printf '需求收集后放弃\n按需求\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$FIX/reply1.txt" \
  ALFRED chat --run-dir "$CASE5" > "$LOG/case5a.out" 2> "$LOG/case5a.err"
assert_state "$CASE5" "planning" case5
printf '放弃\n' | \
  ALFRED chat --run-dir "$CASE5" > "$LOG/case5b.out" 2> "$LOG/case5b.err"
assert_state "$CASE5" "abandoned" case5
audit_has "$CASE5" '"decision":"Abandon"' || fail case5 "Planning 态精确放弃应路由 Abandon"
audit_has "$CASE5" '"from_state":"planning"' || fail case5 "放弃应发生在 planning 态"
pass case5 "Planning 态放弃出口（整行精确匹配 → Abandoned，owner 唯一入口可达）"

# ============================================================================
# Case 6：PlanRejected 打回（R6e 结构闸门离线确定性命中）→ 打回意见呈现 →
#         重试（P3a 伪装重规划）→ 再打回 → 放弃
# ============================================================================
CASE6="$STATE/run-chat-case6"
mkdir -p "$CASE6"
printf '写一个 hello.txt\n按需求\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_REPLY_FILE="$FIX/reply1.txt" \
  ALFRED chat --run-dir "$CASE6" > "$LOG/case6a.out" 2> "$LOG/case6a.err"
assert_state "$CASE6" "planning" case6
CASE6_REQ_ID="$(python3 -c "import json;print(json.load(open('$CASE6/request.json'))['id'])")"
write_plan_fixture "$FIX/plan-case6.json" "$CASE6_REQ_ID"
python3 - "$FIX/plan-case6.json" <<'PY'
import json, sys
p = json.load(open(sys.argv[1]))
p["nodes"][0]["sandbox"]["workspace_subdirs"] = []   # 结构闸门命中：缺产物区声明
json.dump(p, open(sys.argv[1], "w"))
PY
printf '直接建图\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$FIX/plan-case6.json" \
  ALFRED chat --run-dir "$CASE6" > "$LOG/case6b.out" 2> "$LOG/case6b.err"
assert_state "$CASE6" "plan_rejected" case6
has "$LOG/case6b.out" "计划被打回（PlanRejected）" case6
has "$LOG/case6b.out" "计划审查意见（打回）" case6
# 重试 → P3a 伪装：打回理由转写为属主口吻消息驱动重规划（同一缺声明计划再被打回）
printf '重试\n' | \
  ALFRED_OFFLINE=1 ALFRED_OFFLINE_PLAN_FILE="$FIX/plan-case6.json" \
  ALFRED chat --run-dir "$CASE6" > "$LOG/case6c.out" 2> "$LOG/case6c.err"
assert_state "$CASE6" "plan_rejected" case6
audit_has "$CASE6" '"decision":"Retry"' || fail case6 "重试应路由 Retry"
audit_has "$CASE6" '"disguised":true' || fail case6 "PlanRejected+Retry 应走伪装重规划"
python3 - "$CASE6" <<'PY' || fail case6 "伪装消息应作为净化后的属主轮落 conversation.json"
import json, sys
log = json.load(open(sys.argv[1] + "/conversation.json"))
assert any(t["role"] == "owner" and t["source"] == "owner.message" and
           not any(w in t["content"] for w in ("verdict", "打回", "reject"))
           for t in log["turns"]), "无净化后的属主伪装轮"
PY
printf '放弃\n' | \
  ALFRED chat --run-dir "$CASE6" > "$LOG/case6d.out" 2> "$LOG/case6d.err"
assert_state "$CASE6" "abandoned" case6
pass case6 "PlanRejected 打回呈现（verdict 意见）+ 伪装重试闭环 + 放弃"

# ============================================================================
# 真 LLM REPL 用例（CHAT_REAL=1 门控；照 r3 真容器模式分层）
# ============================================================================
if [[ "${CHAT_REAL:-0}" == "1" ]]; then
  echo "[chat-e2e] CHAT_REAL=1：真容器真 LLM REPL 用例"
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || fail chat-real "沙箱镜像 $IMAGE 不存在"
  if [[ -z "${ALFRED_PYTHON:-}" && -x "$REPO_ROOT/.plans/r0-lab/venv/bin/python" ]]; then
    export ALFRED_PYTHON="$REPO_ROOT/.plans/r0-lab/venv/bin/python"
  fi
  REAL="$STATE/run-chat-real"
  # 真对话（§二.8 改口语义）：需求+先答复要求 → [pi] 真答复（Planning）→ 确认建图
  # → 自主流转 → 挂起/终态。pi 若跳过答复直接建图，后续行按"终态→新需求"路由
  # （同样合法），断言与最终状态无关。
  # （子壳内 unset 离线注入 env——env 工具无法调用 shell 函数，走 ALFRED 函数本体）
  (
    unset ALFRED_OFFLINE ALFRED_OFFLINE_PLAN_FILE ALFRED_OFFLINE_REPLY_FILE
    printf '在 workspace 里写一个 hello.txt，内容必须是 Hello。先回复我你的理解，等我确认后再建图\n按需求\n确认无误，直接按验收标准建图\n' | \
      ALFRED chat --run-dir "$REAL" > "$LOG/chat-real.out" 2> "$LOG/chat-real.err"
  )
  has "$LOG/chat-real.out" "\[pi\] " chat-real
  has "$LOG/chat-real.out" "确定性转写" chat-real
  has "$LOG/chat-real.out" "会话结束" chat-real
  python3 - "$REAL" <<'PY' || fail chat-real "conversation.json 应含真实 planner ConverseReply 轮"
import json, sys
log = json.load(open(sys.argv[1] + "/conversation.json"))
assert any(t["role"] == "planner" and t["source"] == "converse.reply" and t["content"].strip()
           for t in log["turns"]), "无 planner converse.reply 轮"
PY
  [[ -n "$(ls "$REAL/llm-calls" 2>/dev/null)" ]] || fail chat-real "llm-calls/ 无真实调用证据"
  # 两轮流脚本（改口→确认建图）输入下，run 终态不得落在 escalated——escalated =
  # driver 升级（planning_error 等），历史上正是 planner outputs 跨轮残留把两轮
  # 路径钉死成 100% planning_error_escalated。断言收紧锁死改口→建图路径；若真
  # 升级必须是 execution/plan_review 来源（非 planning 侧）才放行。
  REAL_STATE="$(state_of "$REAL")"
  case "$REAL_STATE" in
    planning|plan_rejected|completed|abandoned) ;;
    escalated)
      SOURCE="$(python3 -c "import json,sys;print(json.load(open(sys.argv[1])).get('escalation_source') or 'none')" "$REAL/state.json")"
      [[ "$SOURCE" != "planning" ]] || fail chat-real "两轮输入终态 escalated 且来源 planning（改口→建图路径断裂）"
      [[ "$SOURCE" != "none" ]] || fail chat-real "两轮输入终态 escalated 且无升级来源"
      ;;
    *) fail chat-real "run 落在非法状态 $REAL_STATE" ;;
  esac
  pass chat-real "真容器真 LLM REPL 对话（state=$(state_of "$REAL")）"
else
  echo "[chat-e2e] 跳过真 LLM 用例（设 CHAT_REAL=1 开启）"
fi

echo ""
echo "chat e2e 全部通过：alfred chat owner 持续会话黑盒 PASS"
echo "  日志: $LOG"
echo "============================================="
exit 0
