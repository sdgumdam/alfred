//! alfred-planner：规划器（R3 治理环闭环）。
//!
//! 职责（施工清单 §2.1/§2.4/§2.5）：
//! 1. `converse`：对话 agent——会话文档 + 属主消息 → 建图指令序列 → DagSpec
//!    （宿主 Rust 直调 LLM，每次调用落盘 llm-calls/；`ALFRED_OFFLINE=1` 离线
//!    确定性直通）。
//! 2. `maintain`：维护者 agent——两个触发时机（① 计划审查结论落定；② 属主
//!    补充新需求）更新会话文档三段。
//! 3. `disguise`：计划打回的伪装转写（P7）——属主口吻消息 + 禁词检查。
//!
//! 隔离（§2.2/§2.4）：规划器不感知审查者/执行者；converse 只吃会话文档 +
//! 属主消息；reviewer_models 由编排器从 config 系统注入（E5），规划器不填。

pub mod converse;
pub mod disguise;
pub mod llm;
pub mod maintain;
pub mod container;
pub mod task_gen;

pub use converse::{build_messages, converse, instructions_to_dagspec, ConverseOptions, ConverseOutcome};
pub use disguise::{contains_forbidden_signal, disguise_rejection, neutralize_review_language, FORBIDDEN_SIGNALS};
pub use llm::{log_llm_call, ChatMessage, LlmCallRecord};
pub use maintain::{maintain, MaintainOptions, MaintainTrigger};

use alfred_core::request::OwnerRequest;

/// 把 OwnerRequest 转成属主本轮消息（初始规划时 converse 的 owner_message）。
pub fn format_request_message(request: &OwnerRequest) -> String {
    format!(
        "需求：{}\n\n{}，\n\n验收标准：{}",
        request.title, request.description, request.acceptance_criteria
    )
}
