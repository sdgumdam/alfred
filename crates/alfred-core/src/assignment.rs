//! 任务分派（限界上下文 §6.7 TaskAssignment）。

use serde::{Deserialize, Serialize};

use crate::contract::{Contract, SandboxProfile};

/// 编排器 → 执行驱动的任务分派。
///
/// 限界上下文 §6.7 的 `params` 为 `Map<String, Any>`（Orch8 视角）；
/// 骨架按"对齐表未覆盖决策记录到交付文档"的纪律，收敛为具名字段
/// `contract` + `sandbox`，避免匿名 map 跨层传递（代码质量红线 4）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskAssignment {
    /// 任务唯一 id。
    pub task_id: String,
    /// 执行处理器名（R1 固定 "run_inspect_eval"）。
    pub handler: String,
    /// 行为契约。
    pub contract: Contract,
    /// 沙箱档案（R1 恒为默认：联网拒绝、无额外挂卷）。
    #[serde(default)]
    pub sandbox: SandboxProfile,
}

impl TaskAssignment {
    pub fn new(task_id: impl Into<String>, contract: Contract) -> Self {
        Self {
            task_id: task_id.into(),
            handler: "run_inspect_eval".to_string(),
            contract,
            sandbox: SandboxProfile::default(),
        }
    }
}
