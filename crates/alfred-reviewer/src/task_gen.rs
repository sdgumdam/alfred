//! 计划审查 Inspect Task 定义生成（Rust 生成 Python 文件）。
//!
//! 计划审查 = 独立 eval：Sample 携带 OwnerRequest + DagSpec（+ 会话文档 + 属主消息，
//! P2 修复：审查者全可见 §2.4），scorer 判忠实度产出 PlanVerdict{pass, reason}
//! （限界上下文 §6.4 + 实施计划 P3）。

use anyhow::{Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;

/// 内嵌的计划审查任务模板（见 templates/plan_review.py.tmpl）。
const PLAN_REVIEW_TEMPLATE: &str = include_str!("../templates/plan_review.py.tmpl");

/// 生成 plan_review.py 内容。
///
/// 审查者全可见（§2.4）：除 OwnerRequest + DagSpec 外，还注入会话文档 + 属主消息，
/// 供评分器对照"属主补充与审查摘要都骗不过审查"。会话文档/属主消息可为空（如
/// 独立 `alfred plan-review` 子命令无治理上下文）。
pub fn generate_plan_review_py(
    request: &OwnerRequest,
    dagspec: &DagSpec,
    session_doc: Option<&SessionDoc>,
    owner_message: Option<&str>,
) -> Result<String> {
    let mut out = PLAN_REVIEW_TEMPLATE.to_string();

    // 对象经 serde_json::to_string 成 JSON 文本，再经 json() 编码成 Python
    // 字符串字面量；模板里 json.loads 还原成 dict（值语义一致）。
    let request_json = serde_json::to_string(request).context("serialize OwnerRequest")?;
    let dagspec_json = serde_json::to_string(dagspec).context("serialize DagSpec")?;
    let session_doc_json = match session_doc {
        Some(doc) => serde_json::to_string(doc).context("serialize SessionDoc")?,
        None => "null".to_string(),
    };
    let owner_message_json = match owner_message {
        Some(m) => serde_json::to_string(m).context("serialize owner message")?,
        None => "null".to_string(),
    };

    let inject: &[(&str, String)] = &[
        ("__OWNER_REQUEST_JSON__", json(&request_json)?),
        ("__DAGSPEC_JSON__", json(&dagspec_json)?),
        ("__SESSION_DOC_JSON__", json(&session_doc_json)?),
        ("__OWNER_MESSAGE_JSON__", json(&owner_message_json)?),
        ("__RUN_ID_JSON__", json(&request.id)?),
    ];
    for (token, value) in inject {
        if !out.contains(token) {
            anyhow::bail!("plan_review template missing token {token}");
        }
        out = out.replace(token, value);
    }

    for token in [
        "__OWNER_REQUEST_JSON__",
        "__DAGSPEC_JSON__",
        "__SESSION_DOC_JSON__",
        "__OWNER_MESSAGE_JSON__",
        "__RUN_ID_JSON__",
    ] {
        if out.contains(token) {
            anyhow::bail!("plan_review token replacement incomplete: {token}");
        }
    }

    Ok(out)
}

fn json(s: &str) -> Result<String> {
    serde_json::to_string(s).context("json-encode template value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::contract::Contract;

    fn sample_request() -> OwnerRequest {
        OwnerRequest::new(
            "req-r2-plan",
            "create hello.txt",
            "create hello.txt with Hello",
            "hello.txt exists with content Hello",
        )
    }

    fn sample_dagspec() -> DagSpec {
        DagSpec::new(
            "req-r2-plan",
            vec![alfred_core::dagspec::PlanNode::new(
                "task-1",
                "write hello.txt",
                Contract {
                    prompt: "create hello.txt with Hello".into(),
                    acceptance_criteria: "hello.txt exists with Hello".into(),
                    reviewer_models: vec![],
                },
            )],
        )
    }

    #[test]
    fn generates_plan_review_py() {
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec(), None, None).unwrap();
        assert!(py.contains("OWNER_REQUEST = json.loads("));
        assert!(py.contains("plan_review_task"));
        assert!(!py.contains("__OWNER_REQUEST_JSON__"));
        assert!(!py.contains("__DAGSPEC_JSON__"));
    }

    #[test]
    fn injects_session_doc_and_owner_message() {
        // P2 修复：计划审查模板评分器输入含会话文档 + 属主消息（审查者全可见）
        let mut doc = SessionDoc::new();
        doc.key_conclusions.push("用 Rust".into());
        doc.review_summary.push("属主反馈：方案符合需求。".into());
        let py = generate_plan_review_py(
            &sample_request(),
            &sample_dagspec(),
            Some(&doc),
            Some("属主补充：验收必须严格"),
        )
        .unwrap();
        assert!(
            py.contains("SESSION_DOC = None if _SESSION_DOC_RAW"),
            "SESSION_DOC parse line missing"
        );
        assert!(
            py.contains("OWNER_MESSAGE = None if _OWNER_MESSAGE_RAW"),
            "OWNER_MESSAGE parse line missing"
        );
        assert!(py.contains("用 Rust"), "session doc key_conclusion missing");
        assert!(
            py.contains("属主反馈：方案符合需求。"),
            "review_summary missing"
        );
        assert!(py.contains("属主补充：验收必须严格"), "owner message missing");
        assert!(!py.contains("__SESSION_DOC_JSON__"));
        assert!(!py.contains("__OWNER_MESSAGE_JSON__"));
    }

    #[test]
    fn missing_context_injects_null() {
        // 独立 plan-review 子命令无治理上下文 → 会话文档/属主消息为 null
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec(), None, None).unwrap();
        assert!(py.contains("SESSION_DOC = None"), "expected None session doc");
        assert!(py.contains("OWNER_MESSAGE = None"), "expected None owner message");
    }
}
