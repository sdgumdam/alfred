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

use crate::disguise::neutralize_review_language;
use crate::llm::{log_llm_call, ChatMessage, LlmCallRecord, LlmClient};

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
}

/// 维护会话文档（两时机触发）。
pub fn maintain(
    opts: &MaintainOptions,
    doc: &SessionDoc,
    trigger: MaintainTrigger,
) -> Result<SessionDoc> {
    if std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1") {
        Ok(maintain_offline(doc, trigger))
    } else {
        maintain_llm(opts, doc, trigger)
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

/// 真 LLM 重写会话文档（维护者也是大模型，§2.4）。
fn maintain_llm(
    opts: &MaintainOptions,
    doc: &SessionDoc,
    trigger: MaintainTrigger,
) -> Result<SessionDoc> {
    let client = LlmClient::new(opts.model.clone());
    let current = serde_json::to_string_pretty(doc).context("serialize session doc")?;
    let trigger_desc = match &trigger {
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
    let messages = vec![
        ChatMessage::system(
            "你是规划器的会话文档维护者。维护三字段结构：key_file_paths（计划要参考的\
             关键文件路径）、key_conclusions（已经确立的关键结论）、review_summary（审查\
             结论的中性摘要——用属主口吻，不得出现'审查''否决''打回'等结构化否决信号）。\
             基于当前会话文档与新信息，输出更新后的完整三字段 JSON。",
        ),
        ChatMessage::user(format!(
            "当前会话文档（JSON）：\n{current}\n\n新信息：\n{trigger_desc}\n\n\
             只输出 JSON 对象：{{\"key_file_paths\": [...], \"key_conclusions\": [...], \"review_summary\": [...]}}"
        )),
    ];
    let response = client.chat_with_max_tokens(&messages, 4096)?;
    let updated = parse_session_doc(&response)?;
    log_llm_call(
        &opts.run_dir,
        &LlmCallRecord {
            ts: alfred_core::util::now_rfc3339(),
            role: "maintain".into(),
            model: client.model.inspect_model_id(),
            offline: false,
            messages,
            response: response.clone(),
            ok: true,
            error: None,
        },
    )?;
    Ok(updated)
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

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::contract::Contract;
    use alfred_core::util::now_rfc3339;

    fn doc_with() -> SessionDoc {
        let mut d = SessionDoc::new();
        d.key_file_paths.push("src/main.rs".into());
        d
    }

    fn plan() -> DagSpec {
        DagSpec::new(
            "req-1",
            vec![alfred_core::dagspec::PlanNode::new(
                "task-1",
                "create hello.txt",
                Contract {
                    prompt: "create hello.txt".into(),
                    acceptance_criteria: "hello.txt exists".into(),
                    reviewer_models: vec![],
                },
            )],
        )
    }

    #[test]
    fn offline_plan_reviewed_fail_writes_neutral_summary() {
        let doc = doc_with();
        let v = PlanVerdict::new(false, "The plan was rejected by the reviewer: it fails to create hello.txt.");
        let out = maintain_offline(&doc, MaintainTrigger::PlanReviewed { verdict: v, plan: plan() });
        assert_eq!(out.key_file_paths, vec!["src/main.rs"]);
        assert_eq!(out.review_summary.len(), 1);
        let entry = &out.review_summary[0];
        assert!(entry.contains("hello.txt"), "keeps feedback: {entry}");
        assert!(
            crate::disguise::contains_forbidden_signal(entry).is_none(),
            "leaked structured signal: {entry}"
        );
    }

    #[test]
    fn offline_plan_reviewed_pass_writes_positive_summary() {
        let doc = SessionDoc::new();
        let v = PlanVerdict::new(true, "plan covers everything");
        let out = maintain_offline(&doc, MaintainTrigger::PlanReviewed { verdict: v, plan: plan() });
        assert!(out.review_summary[0].contains("符合需求"));
    }

    #[test]
    fn offline_owner_message_appends_key_conclusion() {
        let doc = doc_with();
        let out = maintain_offline(
            &doc,
            MaintainTrigger::OwnerMessage {
                message: "技术选型用 Rust".into(),
            },
        );
        assert_eq!(out.key_conclusions, vec!["技术选型用 Rust"]);
        // 原文档不被就地修改（不可变语义）
        assert!(doc.key_conclusions.is_empty());
    }

    #[test]
    fn parse_session_doc_strips_fences() {
        let text = "```json\n{\"key_file_paths\": [\"a.rs\"], \"key_conclusions\": [], \"review_summary\": []}\n```";
        let doc = parse_session_doc(text).unwrap();
        assert_eq!(doc.key_file_paths, vec!["a.rs"]);
    }

    #[test]
    fn session_doc_serializes_deny_unknown() {
        let doc = SessionDoc::new();
        let json = serde_json::to_string(&doc).unwrap();
        let back: SessionDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
        // deny_unknown_fields
        let bad = r#"{"key_file_paths":[],"key_conclusions":[],"review_summary":[],"extra":1}"#;
        assert!(serde_json::from_str::<SessionDoc>(bad).is_err());
    }

    #[test]
    fn llm_call_record_has_timestamp() {
        let rec = LlmCallRecord {
            ts: now_rfc3339(),
            role: "maintain".into(),
            model: "m".into(),
            offline: false,
            messages: vec![],
            response: "{}".into(),
            ok: true,
            error: None,
        };
        assert!(rec.ts.contains('T'));
    }
}
