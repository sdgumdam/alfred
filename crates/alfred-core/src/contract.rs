//! 行为契约与沙箱档案（施工清单 §2.1/§2.3/§2.5，限界上下文 §6.3/§6.3.1）。

use serde::{Deserialize, Serialize};

/// 行为契约：给执行者的明确工作要求（限界上下文 §6.3）。
///
/// - `prompt`：给执行者（pi）的任务描述。
/// - `acceptance_criteria`：给审查者的验收标准（执行审查的判分依据）。
/// - `reviewer_models`：异构审查模型列表。**系统注入**（实施计划 E5：
///   规划器不感知审查者，add_node 后由系统从 config roles.reviewer 注入）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub prompt: String,
    pub acceptance_criteria: String,
    #[serde(default)]
    pub reviewer_models: Vec<String>,
}

/// 沙箱档案：agent 节点的执行环境声明（限界上下文 §6.3.1）。
///
/// 契约管"做什么"，档案管"在什么约束下做"。planner 声明、计划审查按
/// 最小权限审、编排器起容器时照档案执行。**工作区本身由执行驱动固定
/// 挂载，不在此列**（施工清单 §2.5）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    /// 工作区之外的挂载卷（一律只读，用于参考材料；不 cp 进工作区）。
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// 语言运行时（如 "rust" / "python"）。缺省 = 容器默认镜像。
    #[serde(default)]
    pub runtime: Option<String>,
    /// 需要安装的依赖包。
    #[serde(default)]
    pub packages: Vec<String>,
    /// 是否允许联网。**默认拒绝**（施工清单 §2.5 / P2）。
    #[serde(default)]
    pub network: bool,
    /// 契约声明的工作区子目录（相对持久 ws 的**相对路径**；R6a/M5 显式声明制）。
    /// executor 只挂这些子目录（空 = 不挂 ws），且必须相对、不含 `..`——
    /// 绝对/越界路径在 compose 生成时被拒绝（防静默换基 rw 挂载）。
    #[serde(default)]
    pub workspace_subdirs: Vec<String>,
}

impl Default for SandboxProfile {
    fn default() -> Self {
        Self {
            volumes: Vec::new(),
            runtime: None,
            packages: Vec::new(),
            network: false,
            workspace_subdirs: Vec::new(),
        }
    }
}

/// 挂载卷（限界上下文 §6.3.1）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VolumeMount {
    /// 宿主机路径（planner 申请的范围，计划审查按最小权限审）。
    pub host_path: String,
    /// 容器内挂载路径（建议独立路径如 /references，不与工作区混）。
    pub container_path: String,
}
