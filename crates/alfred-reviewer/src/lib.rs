//! alfred-reviewer：审查者（R2 实现 + R6c 容器化）。
//!
//! 职责（施工清单 §3.1 / 实施计划 P3，三容器 Inspect 统一管）：
//! 1. 计划审查/执行审查都走 reviewer 容器（ws 全量 ro + 对话记录 + 契约全字段，
//!    AGT 拦写层），经 Inspect 容器管理接口（DockerSandboxEnvironment +
//!    sandbox_agent_bridge + exec_remote）起容器/驱动 pi——不再有 `inspect eval`
//!    评测路径（内嵌 scorer / 独立计划审查 eval 均已移除）。
//! 2. verdict.rs 承载 verdict.json 输出契约（宿主读容器产出做 Pydantic 等价校验）。
//! 3. exec_review.rs 为执行审查独立容器驱动；plan_review.rs 为计划审查容器驱动。

pub mod container;
pub mod exec_review;
pub mod plan_review;
pub mod task_gen;
pub mod verdict;

pub use container::{
    run_exec_review_in_container, run_plan_review_in_container, ReviewerContainerOptions,
};
pub use exec_review::{default_exec_review_dir, execute_exec_review, ExecReviewOptions, ExecReviewOutcome};
pub use plan_review::{execute_plan_review, PlanReviewOptions, PlanReviewOutcome};
pub use verdict::{
    parse_exec_verdict_json, parse_plan_verdict_json, ExecVerdictDoc, PlanVerdictDoc,
};
