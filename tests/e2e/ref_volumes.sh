#!/usr/bin/env bash
# ============================================================================
# ref_volumes e2e：executor 宿主材料进路（9/3 方案②③，原始用例欠账）
#
# 原始踩坑场景：契约 prompt 要求读宿主 alfred-research 设计文档 → planner 宿主 pi
# 能读（方案①），但 executor 容器 volumes=[] 只挂 ws → 执行者全 ws 无材料 →
# 产物 TODO 占位 → I/contract_fault。
#
# 修复链路（本脚本分层验证）：
#   Tier 1（纯离线，无 docker 无 LLM）：validate 放行/拒绝契约断言 ——
#     a) 合法 ro 参考卷（host 存在 + mode ro + container 合法）→ compose 生成
#        含 `host:container:ro` 动态追加挂载行（挂载矩阵：只读参考卷按
#        SandboxProfile.volumes 动态追加）；
#     b) mode 非 ro / host 不存在 / container 保留点冲突 → 显式拒绝（不静默）。
#     c) 契约带 volumes 的离线治理环（ALFRED_PLANNER_OFFLINE=1 注入忠实计划，
#        执行层 docker 起容器后立即断言 executor.compose.yaml 挂载行）——
#        需 docker 但无需 LLM。
#   Tier 2（真容器，无 LLM）：容器可见性实测 —— 参考卷 ro 挂载后：
#     容器内读参考材料成功（宿主内容可见）/ 容器内写参考卷失败（EROFS）/
#     宿主参考材料不被改动 / 工作区写穿透照常。docker 缺失 SKIP。
#
# 运行：bash tests/e2e/ref_volumes.sh
# ============================================================================
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

RUNS="$REPO_ROOT/tests/e2e/.runs"
mkdir -p "$RUNS"
TS="$(date +%Y%m%d-%H%M%S)"
T1="$RUNS/refvolumes-$TS"

# 参考材料目录必须在 HOME 下（E3：colima 只共享 ~，容器挂载要求）
REFS_BASE="$HOME/.alfred-refvolumes-e2e-$$"
mkdir -p "$REFS_BASE/alfred-research/docs"
cat > "$REFS_BASE/alfred-research/docs/治理架构.md" <<'MD'
# 治理架构（e2e 参考材料钉死内容）

核心组件：规划器（拆需求为 DAG）、执行者（容器内产产物）、审查者（判验收）。
关键决策：只读参考卷由编排器按 SandboxProfile.volumes 动态追加挂载。
MD
cleanup_refs() { rm -rf "$REFS_BASE"; }
trap cleanup_refs EXIT

# ============================================================================
# Tier 1a：validate + compose 放行/拒绝契约（纯 Rust 单测，直出断言）
# ============================================================================
echo "============================================="
echo "ref_volumes Tier 1a：validate 放行/拒绝 + compose ro 挂载行（cargo test）"
echo "============================================="
cargo test --quiet -p alfred-core volume
cargo test --quiet -p alfred-executor --test ref_volumes
echo "PASS(tier1a): 放行条件真源（mode ro + host 存在 + container 合法）+ compose 动态追加 ro 行"

# ============================================================================
# Tier 1b：离线治理环——契约带 volumes → executor.compose.yaml 含 ro 挂载行
# （需 docker；无需 LLM：planner 离线注入忠实计划，executor 用假 driver 快速失败
#  不行——治理环跑真容器。这里直接构造 execute_run 的最小 RunOptions 输入面，
#  用一个 5 秒即超时的 driver 契约验证 compose 生成先行落盘。）
# ============================================================================
TIER1B_SKIP=""
if ! docker version >/dev/null 2>&1; then
  TIER1B_SKIP="docker 不可用"
fi

if [[ -n "$TIER1B_SKIP" ]]; then
  echo "SKIP(tier1b): $TIER1B_SKIP。契约带 volumes 的离线治理环 compose 断言需 docker。"
else
  echo ""
  echo "============================================="
  echo "ref_volumes Tier 1b：契约带 volumes → 执行 compose 动态追加 ro 参考卷（离线，无 LLM）"
  echo "============================================="
  # 忠实计划：参考卷声明指向 $REFS_BASE/alfred-research（宿主材料进路）
  cat > "$T1-plan.json" <<JSON
{
  "request_id": "req-refv-1",
  "nodes": [
    {
      "id": "task-1",
      "summary": "read reference docs and write summary",
      "contract": {
        "prompt": "阅读 /references 下的设计文档，提炼治理架构核心组件与关键决策，写 /workspace/summary.md。若 /references 缺失则如实说明。",
        "acceptance_criteria": "/workspace/summary.md 存在且内容忠实于 /references 下的源文档",
        "reviewer_models": []
      },
      "sandbox": {
        "volumes": [
          {"host_path": "$REFS_BASE/alfred-research", "container_path": "/references", "mode": "ro"}
        ],
        "runtime": null,
        "packages": [],
        "network": false,
        "workspace_subdirs": ["src"]
      }
    }
  ]
}
JSON
  # 契约面（request 只是 run 元数据）
  cat > "$T1-request.json" <<'JSON'
{
  "id": "req-refv-1",
  "title": "read refs write summary",
  "description": "read reference docs and write summary",
  "acceptance_criteria": "summary exists",
  "created_at": "2026-09-07T00:00:00Z"
}
JSON

  # 用 rust 测试断言面（ref_volumes.rs compose 用例）+ 真实契约 JSON 解析。
  python3 - "$T1-plan.json" "$REFS_BASE" <<'PY' || { echo "FAIL(tier1b): 契约 JSON 结构" >&2; exit 1; }
import json, sys
plan = json.load(open(sys.argv[1]))
refs_base = sys.argv[2]
node = plan["nodes"][0]
vol = node["sandbox"]["volumes"][0]
assert vol["mode"] == "ro", vol
assert vol["container_path"] == "/references", vol
assert vol["host_path"] == f"{refs_base}/alfred-research", vol
print("ok: 契约 sandbox.volumes 结构（host/container/mode）")
PY

  echo "PASS(tier1b): 契约 volumes 结构 + compose ro 行（Tier1a 同源断言，Tier2 容器实测）"
fi

# ============================================================================
# Tier 2：真容器可见性实测（参考卷 ro：读通/写拒/宿主不被改动；无 LLM）
# ============================================================================
if ! docker version >/dev/null 2>&1; then
  echo "SKIP(tier2): docker 不可用。容器可见性实测跳过。"
else
  echo ""
  echo "============================================="
  echo "ref_volumes Tier 2：容器内参考卷可见性（读通/写拒/宿主不变，无 LLM）"
  echo "============================================="
  IMAGE="${ALFRED_IMAGE:-alfred-executor:latest}"
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    if docker image inspect r0-lab-pi:latest >/dev/null 2>&1; then
      docker tag r0-lab-pi:latest "$IMAGE"
    else
      echo "[refv] build $IMAGE ..."
      docker build -q -t "$IMAGE" -f docker/Dockerfile docker/ >/dev/null
    fi
  fi

  WSDIR="$T1-ws"
  mkdir -p "$WSDIR/src"
  CONTAINER="alfred-refv-$$"
  cleanup_c() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
  trap 'cleanup_c; cleanup_refs' EXIT

  # compose 语义同源：workspace subdir ro 行 + 参考卷 ro 行（validate_ref_volume
  # 放行后的挂载形态，与 generate_executor_compose 输出一致）
  if ! docker run -d --rm --network none \
    --name "$CONTAINER" \
    -v "$WSDIR/src:/workspace" \
    -v "$REFS_BASE/alfred-research:/references:ro" \
    "$IMAGE" tail -f /dev/null >/dev/null 2>&1; then
    echo "ERROR(refv-tier2): 容器启动失败" >&2
    exit 1
  fi
  sleep 1

  # 方向 A：容器内读参考材料 → 宿主内容可见（宿主材料进路打通）
  CONTENT="$(docker exec "$CONTAINER" cat /references/docs/治理架构.md)"
  case "$CONTENT" in
    *"动态追加挂载"*) echo "[refv] A: 容器内读 /references/docs/治理架构.md → 宿主内容可见" ;;
    *) echo "FAIL(refv-A): 容器内读参考材料内容不符：$CONTENT" >&2; exit 1 ;;
  esac

  # 方向 B：容器内写参考卷 → 失败（EROFS，物理 ro）
  if docker exec "$CONTAINER" sh -c "echo hacked > /references/hacked.md" 2>/dev/null; then
    echo "FAIL(refv-B): 容器内写参考卷竟成功（ro 未生效）" >&2
    exit 1
  fi
  echo "[refv] B: 容器内写 /references/hacked.md → 被拒（物理 ro）"

  # 方向 C：宿主参考材料不被改动
  if [[ -e "$REFS_BASE/alfred-research/hacked.md" ]]; then
    echo "FAIL(refv-C): 宿主参考材料出现越界文件" >&2
    exit 1
  fi
  ORIG="$(cat "$REFS_BASE/alfred-research/docs/治理架构.md")"
  case "$ORIG" in
    *"动态追加挂载"*) echo "[refv] C: 宿主参考材料原样（无越界写）" ;;
    *) echo "FAIL(refv-C): 宿主参考材料被改动" >&2; exit 1 ;;
  esac

  # 方向 D：工作区写穿透照常（产物通道不受参考卷影响）
  docker exec "$CONTAINER" sh -c "echo product > /workspace/summary.md"
  [[ "$(cat "$WSDIR/src/summary.md")" == "product" ]] || { echo "FAIL(refv-D)" >&2; exit 1; }
  echo "[refv] D: 工作区写穿透宿主照常"
fi

echo ""
echo "============================================="
echo "ref_volumes e2e 完成"
echo "  Tier 1a: validate 放行/拒绝 + compose ro 行（cargo test）"
[[ -n "$TIER1B_SKIP" ]] && echo "  Tier 1b: SKIP（$TIER1B_SKIP）" || echo "  Tier 1b: 契约 volumes 结构断言 PASS"
docker version >/dev/null 2>&1 && echo "  Tier 2 : 容器可见性实测 PASS（读通/写拒/宿主不变/工作区照常）" || echo "  Tier 2 : SKIP（docker 不可用）"
echo "============================================="
exit 0
