#!/usr/bin/env bash
# ============================================================================
# alfred 全链 e2e 汇总入口（R5 收尾，S3）
#
# 依次运行：
#   r1（执行侧）→ r2（审查侧四用例）→ r3（治理环闭环四用例）→
#   r4（决策面板 RPC 两用例）→ escape（越界写边界）→ agt（AGT 策略原型确定性）
# 全部 PASS 才算过；任一失败打印汇总报告并以非零退出（全绿才算过）。
#
# 两种模式（各脚本头部亦有说明）：
#
#   1) 真容器真 LLM（需 docker 沙箱镜像 alfred-executor:latest + 
#      ~/.config/alfred/config.yml 模型凭据；容器 network none + 只挂 workspace）：
#        - r2 case1/1b: 执行审查（reviewer 容器判 C / 离线回退升级挂起）
#        - r3 case1  : 正路径全环（真规划 → 计划审查 → 真容器执行 → 验收 C → Completed）
#        - r2 case1/1b: 执行审查（scorer 判 C / P 部分兑现）
#        - r3 case1  : 正路径全环（真规划 → 计划审查 → 真容器执行 → 验收 C → Completed）
#        - escape    : 纯容器边界两向验证（无 LLM）
#
#   2) 离线注入（ALFRED_OFFLINE=1 + ALFRED_OFFLINE_PLAN_FILE 确定性直通，
#      绕过 planner LLM；计划/执行审查仍走真 LLM）：
#        - r2 case2/3 : 计划审查（注定不忠实打回 / 解析失败 unscored）
#        - r3 case2/3/4: 机械升级闭环 / 打回伪装闭环 / 多轮会话文档
#        - r4 case1/2 : 决策面板（escalated→abandon / plan_rejected→retry→重规划→执行审查离线回退→Escalated）
#
#   3) 确定性（无 LLM、无容器）：
#        - agt : AGT 策略求值原型（node 直测 policy 语义；agt/demo.sh 实机
#                容器拦截演示是 LLM 依赖的可选演示，不在此链内）
#
# 每步日志落 tests/e2e/.runs/skeleton-<ts>/<step>.log。
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

if ! command -v node >/dev/null 2>&1; then
  echo "ERROR: node not found (agt 确定性测试需要)" >&2
  exit 1
fi

TS="$(date +%Y%m%d-%H%M%S)"
LOG_DIR="${SKELETON_LOG_DIR:-$REPO_ROOT/tests/e2e/.runs/skeleton-$TS}"
mkdir -p "$LOG_DIR"

STEPS=(r1 r2 r3 r4 escape agt)
declare -a FAILED=()
declare -a TIMES=()

for step in "${STEPS[@]}"; do
  start="$(date +%s)"
  log="$LOG_DIR/$step.log"
  echo "=== [$step] ==="
  if [[ "$step" == "agt" ]]; then
    if node tests/e2e/agt/agt-policy.test.mjs >"$log" 2>&1; then
      rc=0
    else
      rc=$?
    fi
  else
    if bash "tests/e2e/$step.sh" >"$log" 2>&1; then
      rc=0
    else
      rc=$?
    fi
  fi
  elapsed="$(( $(date +%s) - start ))"
  TIMES+=("$step ${elapsed}s")
  if [[ $rc -eq 0 ]]; then
    echo "  PASS: $step (${elapsed}s)"
  else
    echo "  FAIL: $step (${elapsed}s) — 日志: $log"
    FAILED+=("$step")
  fi
done

echo ""
echo "============================================="
echo "skeleton.sh 全链汇总"
for t in "${TIMES[@]}"; do echo "  $t"; done
if [[ ${#FAILED[@]} -eq 0 ]]; then
  echo "  结果: ALL GREEN（r1-r4 + escape + agt 全链通过）"
  echo "  日志 : $LOG_DIR"
  echo "============================================="
  exit 0
fi
echo "  结果: FAILED — ${FAILED[*]}"
echo "  日志 : ${LOG_DIR}（逐个看 <step>.log 尾部）"
echo "============================================="
exit 1
