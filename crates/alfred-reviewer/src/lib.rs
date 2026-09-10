//! alfred-reviewer：审查者（R2 实现 + 宿主 pi 化）。
//!
//! 职责（架构演进：planner/reviewer 去容器化，属主拍板 2026-09-03）：
//! 1. 计划审查/执行审查都由**宿主 pi agent** 驱动（`host.rs`：spawn `pi -p
//!    --no-session -nc`，cwd=项目根，reviewer 全可见——ws 产物/对话记录/verdict
//!    历史经绝对路径自由读，AGT 只拦写），不再有容器路径（inspect 容器驱动已删）。
//! 2. verdict.rs 承载 verdict.json 输出契约（宿主读产出做 serde 等价校验）。
//! 3. exec_review.rs 为执行审查驱动；plan_review.rs 为计划审查驱动。

pub mod exec_review;
pub mod host;
pub mod plan_review;
pub mod verdict;

pub use exec_review::{default_exec_review_dir, execute_exec_review, ExecReviewOptions, ExecReviewOutcome};
pub use host::{
    exec_review_inputs, run_exec_review_on_host, run_plan_review_on_host,
    ReviewerHostOptions, PLAN_REVIEW_SYSTEM_PROMPT, EXEC_REVIEW_SYSTEM_PROMPT,
};
pub use plan_review::{execute_plan_review, PlanReviewOptions, PlanReviewOutcome};
pub use verdict::{
    parse_exec_verdict_json, parse_plan_verdict_json, ExecVerdictDoc, PlanVerdictDoc,
};
