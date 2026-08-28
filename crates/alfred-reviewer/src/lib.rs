//! alfred-reviewer：审查者（R2 实现 + R6c 容器化）。
//!
//! 职责（施工清单 §3.1 / 实施计划 P3）：
//! 1. 执行审查 scorer 内嵌 executor 的 task 模板（投影物理隔离：executor 只拿
//!    contract.prompt，scorer 只拿 contract.acceptance_criteria）——物理位置在
//!    alfred-executor/templates/pi_task.py.tmpl。
//! 2. 计划审查独立 eval：Sample = OwnerRequest + DagSpec，scorer 判忠实度 →
//!    PlanVerdict{pass, reason}。
//! 3. Rust 驱动复用 alfred-executor::driver（spawn / poll / archive）。
//!
//! R6c：新增 reviewer 容器驱动（container.rs）——计划审查/执行审查都走独立
//!   容器 Agent（ws 全量 ro + 对话记录 + 契约全字段，AGT 拦写层）。

pub mod container;
pub mod plan_review;
pub mod task_gen;

pub use container::{
    run_exec_review_in_container, run_plan_review_in_container, ReviewerContainerOptions,
};
pub use plan_review::{execute_plan_review, extract_plan_verdict, PlanReviewOptions, PlanReviewOutcome};
pub use task_gen::generate_plan_review_py;
