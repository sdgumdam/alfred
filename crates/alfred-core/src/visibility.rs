//! 三容器隔离 schema：可见范围矩阵 + 工具权限矩阵（对齐方案 v2 §一，属主已批）。
//!
//! 依据：`.plans/对齐方案-三容器Agent化.md` v2 §1.1（可见范围矩阵）与 §1.2（工具
//! 权限矩阵）。三容器都是 pi agent，形态相同，隔离只靠 bind mount 可见范围；工具
//! 权限层是"给了再拦"——全工具给到、写由 AGT / 结构性拒绝。
//!
//! 可见范围矩阵（§1.1）逐行落码：
//! - **planner**：ws 全量 ro + OwnerRequest + 会话文档 + 契约（自己写的）ro；
//!   不挂 conversation.json（审查者独有）。
//! - **executor**：`workspace_subdirs` 声明子集 rw（空 = 不挂 ws，M5 已定）；
//!   只拿契约 prompt 投影，不挂全字段 / OwnerRequest / 会话文档。
//! - **reviewer**：ws 全量 ro + conversation + 契约全字段 + 审查记录，全可见。
//!
//! 工具权限矩阵（§1.2）逐行落码：
//! - planner / reviewer：全工具给全 + `DenyWrite`（写类结构性拒绝，审计 JSONL）。
//! - executor：容器默认工具面 + AGT 边界策略（默认启用，M2 已定 a）。

use serde::{Deserialize, Serialize};

/// 三角色（§1.1 矩阵行 / §1.2 矩阵行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Planner,
    Executor,
    Reviewer,
}

/// 工作区挂载面（§1.1 矩阵第 1/2 行：ws 全量 / ws 子集）。
///
/// 序列化形态：`"full_ro"` | `{"subdirs":["src","tests"]}` | `"none"`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum WorkspaceMount {
    /// ws 全量只读（planner / reviewer）。
    #[serde(rename = "full_ro")]
    FullRO,
    /// `workspace_subdirs` 声明子集（executor，rw）。
    Subdirs(Vec<String>),
    /// 不挂 ws。
    None,
}

impl WorkspaceMount {
    /// 归一化构造：**空 subdirs = 不挂 ws**（M5 已定，方案推荐 a：显式声明制，
    /// 贴合"执行者只挂契约任务描述对应的子目录"，非向后兼容的全量挂载）。
    pub fn subdirs(subdirs: Vec<String>) -> Self {
        if subdirs.is_empty() {
            WorkspaceMount::None
        } else {
            WorkspaceMount::Subdirs(subdirs)
        }
    }
}

/// 契约挂载形态（§1.1 矩阵第 7 行：契约全字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ContractVisibility {
    /// 契约全字段只读（planner 自己写的 / reviewer 全可见）。
    Full,
    /// 只挂 prompt 投影（executor：拿不到验收标准与契约全本，技术架构 T8）。
    PromptOnly,
    /// 不挂契约。
    None,
}

impl Default for ContractVisibility {
    /// `#[serde(default)]` 的缺省值：不挂契约（最保守）。
    fn default() -> Self {
        ContractVisibility::None
    }
}

/// 三角色挂载矩阵 schema（§1.1 表逐行落码）。
///
/// 构造器 `planner()` / `executor(subdirs)` / `reviewer()` 即矩阵三行；序列化带
/// `deny_unknown_fields`，字段漂移（新增未知字段）即反序列化报错。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibilitySpec {
    pub role: AgentRole,
    /// 工作区挂载（ws 全量 / ws 子集 / 不挂）。
    pub ws_mount: WorkspaceMount,
    /// OwnerRequest（request.json）是否挂载（§1.1 第 4 行）。
    #[serde(default)]
    pub request: bool,
    /// 会话文档（session.json 投影）是否挂载（§1.1 第 5 行）。
    #[serde(default)]
    pub session_doc: bool,
    /// owner↔planner 对话记录（conversation.json）是否挂载（§1.1 第 6 行）。
    /// **仅 reviewer 为 true**——planner/executor 挂载面上不存在此文件（非声明性）。
    #[serde(default)]
    pub conversation: bool,
    /// 契约挂载形态（§1.1 第 7 行）。
    #[serde(default)]
    pub contract: ContractVisibility,
    /// 只读参考卷是否允许挂载（§1.1 第 3 行"按需/档案声明"；具体卷列表由
    /// `SandboxProfile.volumes` 运行期决定）。
    #[serde(default)]
    pub references: bool,
    /// 审查记录 / verdict 是否挂载（§1.1 第 8 行）。**仅 reviewer 为 true**。
    #[serde(default)]
    pub review_records: bool,
}

impl VisibilitySpec {
    /// §1.1 planner 行：ws 全量 ro + OwnerRequest + 会话文档 + 契约（自己写的，ro）。
    /// 不挂 conversation、不挂审查记录。
    pub fn planner() -> Self {
        Self {
            role: AgentRole::Planner,
            ws_mount: WorkspaceMount::FullRO,
            request: true,
            session_doc: true,
            conversation: false,
            contract: ContractVisibility::Full,
            references: true,
            review_records: false,
        }
    }

    /// §1.1 executor 行：`workspace_subdirs` 子集 rw（空 = 不挂 ws，M5 已定）+
    /// 参考卷（档案声明）+ 契约 prompt 投影。无 OwnerRequest / 会话文档 /
    /// conversation / 审查记录。
    pub fn executor(workspace_subdirs: &[String]) -> Self {
        Self {
            role: AgentRole::Executor,
            ws_mount: WorkspaceMount::subdirs(workspace_subdirs.to_vec()),
            request: false,
            session_doc: false,
            conversation: false,
            contract: ContractVisibility::PromptOnly,
            references: true,
            review_records: false,
        }
    }

    /// §1.1 reviewer 行：ws 全量 ro + OwnerRequest + 会话文档 + conversation +
    /// 契约全字段 + 审查记录。全可见。
    pub fn reviewer() -> Self {
        Self {
            role: AgentRole::Reviewer,
            ws_mount: WorkspaceMount::FullRO,
            request: true,
            session_doc: true,
            conversation: true,
            contract: ContractVisibility::Full,
            references: true,
            review_records: true,
        }
    }
}

/// 工具面（§1.2 工具权限矩阵"工具面"列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ToolSurface {
    /// 全工具给全（读文件、bash、写类工具也出现在工具列表）——planner/reviewer。
    FullTools,
    /// pi 容器默认工具面——executor。
    Default,
}

/// 写类工具拦截策略（§1.2 "给了再拦"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum WritePolicy {
    /// 结构性拒绝写类工具（write/edit/rename/delete/mkdir/move/copy/create +
    /// bash 写重定向）——planner/reviewer；每次被拒决策落 AGT 审计 JSONL。
    DenyWrite,
    /// AGT 边界策略（`docker/agt/executor/policy.json`：workspace-write-only /
    /// no-sudo / no-host-path-touch / host-secret-read / recursive-delete）——
    /// executor 默认启用（M2 已定 a；属主拍板 AGT 默认启用）。
    AgtBoundary,
}

/// 工具权限矩阵 schema（§1.2 表逐行落码）。
///
/// 构造器 `planner()` / `executor()` / `reviewer()` 即矩阵三行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    pub role: AgentRole,
    pub tool_surface: ToolSurface,
    pub write_policy: WritePolicy,
    /// 是否启用 AGT 审计（每次被拒 / 越界 tool_call 落审计 JSONL）。
    #[serde(default = "default_true")]
    pub agt_audit: bool,
}

fn default_true() -> bool {
    true
}

impl ToolPolicy {
    /// §1.2 planner 行：全工具给全 + 结构性拒绝写。
    pub fn planner() -> Self {
        Self {
            role: AgentRole::Planner,
            tool_surface: ToolSurface::FullTools,
            write_policy: WritePolicy::DenyWrite,
            agt_audit: true,
        }
    }

    /// §1.2 executor 行：容器默认工具面 + AGT 边界策略（默认启用，M2 已定）。
    pub fn executor() -> Self {
        Self {
            role: AgentRole::Executor,
            tool_surface: ToolSurface::Default,
            write_policy: WritePolicy::AgtBoundary,
            agt_audit: true,
        }
    }

    /// §1.2 reviewer 行：全工具给全 + 结构性拒绝写。
    pub fn reviewer() -> Self {
        Self {
            role: AgentRole::Reviewer,
            tool_surface: ToolSurface::FullTools,
            write_policy: WritePolicy::DenyWrite,
            agt_audit: true,
        }
    }
}
