//! alfred-core：跨组件共享实体（唯一真源）。
//!
//! 依据：SKELETON施工清单.md §2.1/§2.3/§3.3 + 限界上下文.md §六。
//! 所有实体序列化时 `deny_unknown_fields`，杜绝字段漂移。

pub mod artifact;
pub mod assignment;
pub mod builder;
pub mod contract;
pub mod conversation;
pub mod dagspec;
pub mod governance;
pub mod request;
pub mod session;
pub mod session_index;
pub mod util;
pub mod verdict;
pub mod visibility;

pub use artifact::{Artifact, ChangeKind, FileChange, FileEntry};
pub use assignment::TaskAssignment;
pub use builder::{BuildInstruction, GraphBuilder};
pub use contract::{Contract, SandboxProfile, VolumeMount};
pub use conversation::{
    append_to_disk, load_conversation, save_conversation, ConversationLog, ConversationRole,
    ConversationSource, ConversationTurn, CONVERSATION_FILE,
};
pub use dagspec::{DagSpec, Edge, PlanNode};
pub use governance::{
    route, GovernanceAblation, GovernanceEvent, GovernanceOptions, GovernanceRun,
    GovernanceState, GovernanceStateMachine, OwnerDecision, RoutingDecision, TransitionError,
};
pub use request::OwnerRequest;
pub use session::SessionDoc;
pub use verdict::{Confidence, ExecVerdict, FailureClass, PlanVerdict, VerdictGrade};
pub use visibility::{
    AgentRole, ContractVisibility, ToolPolicy, ToolSurface, VisibilitySpec, WorkspaceMount,
    WritePolicy,
};
