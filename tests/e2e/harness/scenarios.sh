#!/usr/bin/env bash
# ============================================================================
# 等价 harness 场景库（重构方案 v2 步骤①：事件-副作用断言表 = harness 预期）。
#
# 每个场景 = 离线确定性驱动（真 alfred bin run/feed）+ 期望 audit 事件序列断言。
# 场景覆盖 HEAD 事件-副作用表的每条分支（表见 crates/alfred-cli/src/governance.rs
# 头部注释——重构步骤①盘点后固化）。
#
# 公共约定（由 equiv.sh 提供）：
#   ALFRED_BIN   — 被测二进制
#   SCEN_ROOT    — 本轮（pre 或 post）场景工作根目录
#   REPO_ROOT    — 仓库根（canonical 规范化占位用）
#   run_feed     — 封装 "$ALFRED_BIN" feed（日志落盘 + rc 检查）
#   canon        — 规范化 run 目录 → <SCEN_ROOT>/<scenario>.canonical.json
#   expect_audit — 断言 canonical dump 的 audit 事件序列 == 期望列表
#
# 场景清单（与事件-副作用表一一对应）：
#   s1_plan_review_error   Planning(Instructions) + PlanReviewing(unscored 离线回退)
#                          + Escalated + feed abandon → Abandoned
#   s2_reply_revise        Planning(Reply 停驻) + feed revise（再 Reply）+ feed abandon
#   s3_maintainer_stall    维护者停摆注入（离线维护产出注入坏 JSON → 显式报错）→
#                          planning_error_escalated → Escalated(planning) → feed retry
#                          （再失败）→ feed abandon【失败注入①：维护者失败降级等价】
#   s4_exec_hard_error     计划审查通过（mock provider 驱动宿主 pi）→ 执行硬错误
#                          （sandbox.runtime 契约拒绝，容器启动前 bail）→
#                          execution_hard_error_escalated【失败注入②：执行硬错误升级】
#   s5_exec_review_cycle   全环：planning(Instructions) → plan review pass → 容器执行
#                          （mockllm）→ exec review 离线回退升级 → feed retry 重入执行
#                          （exec-2）→ 再升级 → feed abandon【docker+mock；缺失则 SKIP】
# ============================================================================
set -euo pipefail

# ---------------------------------------------------------------------------
# 公共 fixture（场景工作根下一次性生成；request/plan 全场景共用）
# ---------------------------------------------------------------------------
write_fixtures() {
  local root="$1"
  mkdir -p "$root/fix"
  cat > "$root/fix/request.json" <<'JSON'
{
  "id": "req-equiv",
  "title": "create hello.txt",
  "description": "Create a file named hello.txt in the workspace. Its content must be exactly: Hello",
  "acceptance_criteria": "hello.txt exists in the workspace and its content is exactly 'Hello'",
  "created_at": "2026-09-07T00:00:00Z"
}
JSON
  # 忠实计划（planner 离线直通；单节点 + src 子目录 + 空参考卷）
  cat > "$root/fix/plan-faithful.json" <<'JSON'
{
  "request_id": "req-equiv",
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
  # 硬错误计划（同上但 runtime=p anticipation——executor 沙箱校验在容器启动前显式拒绝）
  cat > "$root/fix/plan-hard-error.json" <<'JSON'
{
  "request_id": "req-equiv",
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
        "runtime": "python",
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  # 维护者停摆注入：非法会话文档 JSON（run_maintain 收割解析失败 → 显式 Err）
  printf '{{{ not a session doc' > "$root/fix/maintain-bad.json"
  # 规划器答复分支注入
  printf '这是一个澄清答复：需要先确认目录结构再动手。' > "$root/fix/reply.txt"
}

# ---------------------------------------------------------------------------
# 场景 S1：Instructions → plan review 离线回退（unscored）→ Escalated → abandon
# ---------------------------------------------------------------------------
s1_plan_review_error() {
  local R="$SCEN_ROOT"; mkdir -p "$R/run"
  export ALFRED_OFFLINE=1
  export ALFRED_OFFLINE_PLAN_FILE="$R/fix/plan-faithful.json"
  unset ALFRED_OFFLINE_REPLY_FILE ALFRED_MAINTAIN_OFFLINE_FILE 2>/dev/null || true
  run_run "$R/run" "$R/fix/request.json"
  expect_audit "$R/run" s1 \
    "governance_started state_entered planning_done maintain_done state_entered plan_review_error_escalated state_entered governance_paused"
  expect_state "$R/run" "escalated" "plan_review"
  run_feed "$R/run" abandon "" 1
  expect_audit "$R/run" s1-after \
    "governance_started state_entered planning_done maintain_done state_entered plan_review_error_escalated state_entered governance_paused feed_owner_message governance_paused"
  expect_state "$R/run" "abandoned" ""
  unset ALFRED_OFFLINE ALFRED_OFFLINE_PLAN_FILE
  canon "$R/run" s1_plan_review_error
}

# ---------------------------------------------------------------------------
# 场景 S2：Reply 停驻 → feed revise（再 Reply）→ feed abandon
# ---------------------------------------------------------------------------
s2_reply_revise() {
  local R="$SCEN_ROOT"; mkdir -p "$R/run"
  export ALFRED_OFFLINE=1
  export ALFRED_OFFLINE_REPLY_FILE="$R/fix/reply.txt"
  unset ALFRED_OFFLINE_PLAN_FILE ALFRED_MAINTAIN_OFFLINE_FILE 2>/dev/null || true
  run_run "$R/run" "$R/fix/request.json"
  expect_audit "$R/run" s2 \
    "governance_started state_entered converse_reply maintain_done governance_paused"
  expect_state "$R/run" "planning" ""
  run_feed "$R/run" revise "属主补充：目录按 src/ 布局。" 1
  expect_audit "$R/run" s2-after-revise \
    "governance_started state_entered converse_reply maintain_done governance_paused feed_owner_message state_entered converse_reply maintain_done governance_paused"
  expect_state "$R/run" "planning" ""
  run_feed "$R/run" abandon "" 2
  expect_state "$R/run" "abandoned" ""
  unset ALFRED_OFFLINE ALFRED_OFFLINE_REPLY_FILE
  canon "$R/run" s2_reply_revise
}

# ---------------------------------------------------------------------------
# 场景 S3【失败注入①维护者停摆】：离线维护产出注入坏 JSON → 显式报错降级
# ---------------------------------------------------------------------------
s3_maintainer_stall() {
  local R="$SCEN_ROOT"; mkdir -p "$R/run"
  export ALFRED_OFFLINE=1
  export ALFRED_OFFLINE_PLAN_FILE="$R/fix/plan-faithful.json"
  export ALFRED_MAINTAIN_OFFLINE_FILE="$R/fix/maintain-bad.json"
  run_run "$R/run" "$R/fix/request.json"
  expect_audit "$R/run" s3 \
    "governance_started state_entered planning_done planning_error_escalated governance_paused"
  expect_state "$R/run" "escalated" "planning"
  run_feed "$R/run" retry "" 1
  expect_audit "$R/run" s3-after-retry \
    "governance_started state_entered planning_done planning_error_escalated governance_paused feed_owner_message state_entered planning_done planning_error_escalated governance_paused"
  expect_state "$R/run" "escalated" "planning"
  run_feed "$R/run" abandon "" 2
  expect_state "$R/run" "abandoned" ""
  unset ALFRED_OFFLINE ALFRED_OFFLINE_PLAN_FILE ALFRED_MAINTAIN_OFFLINE_FILE
  canon "$R/run" s3_maintainer_stall
}

# ---------------------------------------------------------------------------
# 场景 S4【失败注入②执行硬错误】：计划审查过 → sandbox 契约拒绝 → 升级
# （需要 mock provider；由 equiv.sh 预启动并导出 MOCK_PORT / FIX 模板）
# ---------------------------------------------------------------------------
s4_exec_hard_error() {
  local R="$SCEN_ROOT"; mkdir -p "$R/run"
  write_mock_config "$R/fix/config.yml"
  export ALFRED_CONFIG="$R/fix/config.yml"
  export ALFRED_REVIEWER_MODEL="mock-reviewer"
  export ALFRED_PLANNER_OFFLINE=1
  export ALFRED_OFFLINE_PLAN_FILE="$R/fix/plan-hard-error.json"
  unset ALFRED_OFFLINE 2>/dev/null || true
  run_run "$R/run" "$R/fix/request.json"
  # M3 起执行侧新增节点轨迹审计：node_started（每轮节点执行直写）。
  expect_audit "$R/run" s4 \
    "governance_started state_entered planning_done maintain_done state_entered plan_review_passed state_entered node_started execution_hard_error_escalated state_entered governance_paused"
  expect_state "$R/run" "escalated" "execution"
  unset ALFRED_CONFIG ALFRED_REVIEWER_MODEL ALFRED_PLANNER_OFFLINE ALFRED_OFFLINE_PLAN_FILE
  canon "$R/run" s4_exec_hard_error
}

# ---------------------------------------------------------------------------
# 场景 S5：全环（容器执行 mockllm）→ exec review 离线回退 → retry 重入 → abandon
# ---------------------------------------------------------------------------
s5_exec_review_cycle() {
  local R="$SCEN_ROOT"; mkdir -p "$R/run"
  write_mock_config "$R/fix/config.yml"
  export ALFRED_CONFIG="$R/fix/config.yml"
  export ALFRED_REVIEWER_MODEL="mock-reviewer"
  export ALFRED_PLANNER_OFFLINE=1
  export ALFRED_OFFLINE_PLAN_FILE="$R/fix/plan-faithful.json"
  export ALFRED_EXEC_REVIEW_OFFLINE=1
  unset ALFRED_OFFLINE 2>/dev/null || true
  run_run "$R/run" "$R/fix/request.json"
  # M3 起执行侧新增节点轨迹审计：node_started/node_completed（节点级）+
  # execution_succeeded（全图完成门，HEAD 形态不变）。
  expect_audit "$R/run" s5 \
    "governance_started state_entered planning_done maintain_done state_entered plan_review_passed state_entered node_started node_completed execution_succeeded state_entered exec_review_error_escalated state_entered governance_paused"
  expect_state "$R/run" "escalated" "execution"
  run_feed "$R/run" retry "" 1
  # retry 重入 Executing 时全图已完成 → execution_graph_rerun 清空重推
  # （HEAD 单节点"重入执行即重跑"的图级推广，exec-2 产物照旧）。
  expect_audit "$R/run" s5-after-retry \
    "governance_started state_entered planning_done maintain_done state_entered plan_review_passed state_entered node_started node_completed execution_succeeded state_entered exec_review_error_escalated state_entered governance_paused feed_owner_message state_entered execution_graph_rerun node_started node_completed execution_succeeded state_entered exec_review_error_escalated state_entered governance_paused"
  expect_state "$R/run" "escalated" "execution"
  expect_field "$R/run" "execution_count" "2"
  run_feed "$R/run" abandon "" 2
  expect_state "$R/run" "abandoned" ""
  unset ALFRED_CONFIG ALFRED_REVIEWER_MODEL ALFRED_PLANNER_OFFLINE ALFRED_OFFLINE_PLAN_FILE ALFRED_EXEC_REVIEW_OFFLINE
  canon "$R/run" s5_exec_review_cycle
}

SCENARIOS_ALL=(s1_plan_review_error s2_reply_revise s3_maintainer_stall s4_exec_hard_error s5_exec_review_cycle)
