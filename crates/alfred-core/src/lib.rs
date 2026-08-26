//! alfred-core：跨组件共享实体（唯一真源）。
//!
//! 依据：SKELETON施工清单.md §2.1/§2.3/§3.3 + 限界上下文.md §六。
//! 所有实体序列化时 `deny_unknown_fields`，杜绝字段漂移。

pub mod artifact;
pub mod assignment;
pub mod contract;
pub mod dagspec;
pub mod request;
pub mod util;
pub mod verdict;

pub use artifact::{Artifact, ChangeKind, FileChange, FileEntry};
pub use assignment::TaskAssignment;
pub use contract::{Contract, SandboxProfile, VolumeMount};
pub use dagspec::{DagSpec, PlanNode};
pub use request::OwnerRequest;
pub use verdict::{Confidence, ExecVerdict, FailureClass, PlanVerdict, VerdictGrade};
