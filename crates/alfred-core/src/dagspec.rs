//! 计划（DagSpec）：规划器产物，计划审查输入（限界上下文 §6.4 DagSpec）。
//!
//! R2 实现审查侧：计划审查 eval 用 `OwnerRequest + DagSpec` 判忠实度，
//! 产出 PlanVerdict。DagSpec 是规划器（R2 后续填充）的产物形态，先作为
//! 跨组件共享实体落在 alfred-core——planner / reviewer / cli 都要引用，
//! 单一真源（代码质量红线 1）。

use serde::{Deserialize, Serialize};

use crate::contract::{Contract, SandboxProfile};

/// 计划节点：一个任务节点 = 契约 + 一句话摘要。
///
/// `contract` 与 TaskAssignment.contract 同构；执行侧复用同一实体。
/// R3 增补 `sandbox`（§2.5：每个要跑 agent 的节点在图里除了契约之外，
/// 还要附带一份沙箱档案——契约管"做什么"，档案管"在什么约束下做"）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlanNode {
    /// 节点 id（对应 TaskAssignment.task_id）。
    pub id: String,
    /// 节点摘要（该节点做什么，供计划审查快速对齐）。
    pub summary: String,
    /// 行为契约。
    pub contract: Contract,
    /// 沙箱档案（§2.5；缺省 = 联网拒绝、无额外挂卷——execute_run 只支持默认档案）。
    #[serde(default)]
    pub sandbox: SandboxProfile,
}

impl PlanNode {
    pub fn new(id: impl Into<String>, summary: impl Into<String>, contract: Contract) -> Self {
        Self {
            id: id.into(),
            summary: summary.into(),
            contract,
            sandbox: SandboxProfile::default(),
        }
    }
}

/// 计划：属主需求 → 任务节点的 DAG 拆解。
///
/// R2 只承载审查输入；拓扑边在 R3 规划器填充。节点按依赖序排列，
/// 审查判"拆解是否忠实于 OwnerRequest"。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DagSpec {
    /// 被拆解的需求 id（对应 OwnerRequest.id）。
    pub request_id: String,
    /// 节点列表（R2 为有序列表）。
    pub nodes: Vec<PlanNode>,
}

impl DagSpec {
    pub fn new(request_id: impl Into<String>, nodes: Vec<PlanNode>) -> Self {
        Self {
            request_id: request_id.into(),
            nodes,
        }
    }
}
