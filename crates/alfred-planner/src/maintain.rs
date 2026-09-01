//! 维护者 agent（施工清单 §2.4）：更新会话文档。
//!
//! 维护者在两种时机更新会话文档：
//!   ① 一次计划审查结论落定之后（`PlanReviewed`）——写 `review_summary`；
//!   ② 属主补充新需求之后（`OwnerMessage`）——写 `key_conclusions`。
//!
//! 会话文档固定三段结构（key_file_paths / key_conclusions / review_summary），
//! 靠格式保证可查可纠。维护者产出的正确性由下游计划审查间接保证（reviewer
//! 全可见——属主补充与审查摘要都骗不过审查；判分对照基准是 OwnerRequest）。
//!
//! 隔离（§2.2/P7）：`review_summary` 必须是**中性转写**（属主口吻、无结构化
//! 否决信号），不能出现"你的计划被否决了"这类标签——由本模块的中和逻辑保证。
//!
//! 两种模式：`ALFRED_OFFLINE=1` 走确定性更新（e2e 可控）；否则走真 LLM
//! 重写（维护者也是大模型）。


use alfred_core::dagspec::DagSpec;
use alfred_core::session::SessionDoc;
use alfred_core::verdict::PlanVerdict;
use alfred_executor::config::ExecutorModel;
use anyhow::{Context, Result};
use crate::disguise::{neutralize_review_language, sanitize_review_summary};
use crate::llm::{log_llm_call, ChatMessage, LlmCallRecord};

/// 维护者 system prompt（唯一真源）：maintain 的审计消息构建与容器侧 maintain
/// 任务（R6b）共用同一份。
pub(crate) const MAINTAIN_SYSTEM_PROMPT: &str = "你是规划器的会话文档维护者。维护三字段结构：key_file_paths（计划要参考的关键文件路径）、key_conclusions（已经确立的关键结论）、review_summary（审查结论的中性摘要——用属主口吻，不得出现'审查''否决''打回'等结构化否决信号）。基于当前会话文档与新信息，输出更新后的完整三字段 JSON。";

/// 维护者触发时机（§2.4 两个时机）。
#[derive(Debug, Clone)]
pub enum MaintainTrigger {
    /// ① 一次计划审查结论落定之后。
    PlanReviewed { verdict: PlanVerdict, plan: DagSpec },
    /// ② 属主补充新需求之后。
    OwnerMessage { message: String },
}

/// 维护者选项。
#[derive(Debug, Clone)]
pub struct MaintainOptions {
    pub run_dir: std::path::PathBuf,
    pub model: ExecutorModel,
    /// R6b：planner 容器驱动选项（容器内跑 maintain；桥代发 LLM）。
    pub container: crate::container::PlannerContainerOptions,
}

impl MaintainOptions {
    pub fn new(run_dir: std::path::PathBuf, model: ExecutorModel) -> Self {
        Self {
            run_dir,
            model,
            container: crate::container::PlannerContainerOptions::default(),
        }
    }
}

/// 维护会话文档（两时机触发）。
pub fn maintain(
    opts: &MaintainOptions,
    doc: &SessionDoc,
    trigger: MaintainTrigger,
) -> Result<SessionDoc> {
    if std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1") {
        // 离线模式保留：不经容器（现状直通）。
        Ok(maintain_offline(doc, trigger))
    } else {
        // R6b：容器内 pi 读 session + trigger，产更新后会话文档 JSON（桥代发 LLM）。
        let out = crate::container::run_maintain_in_container(
            &opts.container, &opts.model, doc, &trigger,
        )?;
        let mut updated = parse_session_doc(&out.output_text)?;
        // P2 修复：真 LLM 路径对 review_summary 做禁词中和（与离线路径同构）——LLM
        // 输出不可信，任一条目含结构化否决信号 → 回退中性模板。
        sanitize_review_summary(&mut updated.review_summary);
        let messages = maintain_messages(doc, &trigger);
        log_llm_call(
            &opts.run_dir,
            &LlmCallRecord {
                ts: alfred_core::util::now_rfc3339(),
                role: "maintain".into(),
                model: opts.model.inspect_model_id(),
                offline: false,
                transport: "container_bridge".to_string(),
                messages,
                response: out.output_text.clone(),
                ok: true,
                error: None,
            },
        )?;
        Ok(updated)
    }
}

/// 离线确定性更新（不调 LLM；e2e 可控）。
fn maintain_offline(doc: &SessionDoc, trigger: MaintainTrigger) -> SessionDoc {
    let mut out = doc.clone();
    match trigger {
        MaintainTrigger::PlanReviewed { verdict, .. } => {
            if verdict.pass {
                out.review_summary
                    .push("属主：方案符合需求，按此推进。".to_string());
            } else {
                let neutral = neutralize_review_language(&verdict.reason);
                let entry = if neutral.trim().is_empty() {
                    "属主：方案跟我要的不太对，需要按需求重新规划。".to_string()
                } else {
                    format!("属主反馈：{neutral}")
                };
                out.review_summary.push(entry);
            }
        }
        MaintainTrigger::OwnerMessage { message } => {
            out.key_conclusions.push(message);
        }
    }
    out
}

/// 构建 maintain 的审计消息（system + user）：与容器侧 maintain 任务同语义
/// （当前会话文档 + 触发事件描述），供 llm-calls/ 记录断言（P9 证据）。
fn maintain_messages(doc: &SessionDoc, trigger: &MaintainTrigger) -> Vec<ChatMessage> {
    let current = serde_json::to_string_pretty(doc).unwrap_or_default();
    let trigger_desc = match trigger {
        MaintainTrigger::PlanReviewed { verdict, plan } => {
            format!(
                "一次计划审查结论落定：pass={}，理由={}；计划={}",
                verdict.pass,
                verdict.reason,
                serde_json::to_string(plan).unwrap_or_default()
            )
        }
        MaintainTrigger::OwnerMessage { message } => {
            format!("属主补充新需求：{message}")
        }
    };
    vec![
        ChatMessage::system(MAINTAIN_SYSTEM_PROMPT),
        ChatMessage::user(format!(
            "当前会话文档（JSON）：\n{current}\n\n新信息：\n{trigger_desc}\n\n\
             只输出 JSON 对象：{{\"key_file_paths\": [...], \"key_conclusions\": [...], \"review_summary\": [...]}}"
        )),
    ]
}

/// 从 LLM 输出解析 SessionDoc（容忍 markdown 围栏）。
pub fn parse_session_doc(text: &str) -> Result<SessionDoc> {
    let cleaned = strip_fences(text);
    let v: serde_json::Value = serde_json::from_str(&cleaned)
        .with_context(|| format!("session doc not JSON: {cleaned}"))?;
    serde_json::from_value(v).context("session doc schema mismatch")
}

/// 剥 markdown 代码围栏（与审查侧 `_extract_json_text` 同思路）。
fn strip_fences(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                return trimmed[start..=end].to_string();
            }
        }
    }
    trimmed.to_string()
}
