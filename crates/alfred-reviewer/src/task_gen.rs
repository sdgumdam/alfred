//! 计划审查 Inspect Task 定义生成（Rust 生成 Python 文件）。
//!
//! 计划审查 = 独立 eval：Sample 携带 OwnerRequest + DagSpec，scorer 判
//! 忠实度产出 PlanVerdict{pass, reason}（限界上下文 §6.4 + 实施计划 P3）。

use anyhow::{Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;

/// 内嵌的计划审查任务模板（见 templates/plan_review.py.tmpl）。
const PLAN_REVIEW_TEMPLATE: &str = include_str!("../templates/plan_review.py.tmpl");

/// 生成 plan_review.py 内容。
pub fn generate_plan_review_py(request: &OwnerRequest, dagspec: &DagSpec) -> Result<String> {
    let mut out = PLAN_REVIEW_TEMPLATE.to_string();

    // 对象经 serde_json::to_string 成 JSON 文本，再经 json() 编码成 Python
    // 字符串字面量；模板里 json.loads 还原成 dict（值语义一致）。
    let request_json = serde_json::to_string(request).context("serialize OwnerRequest")?;
    let dagspec_json = serde_json::to_string(dagspec).context("serialize DagSpec")?;

    let inject: &[(&str, String)] = &[
        ("__OWNER_REQUEST_JSON__", json(&request_json)?),
        ("__DAGSPEC_JSON__", json(&dagspec_json)?),
        ("__RUN_ID_JSON__", json(&request.id)?),
    ];
    for (token, value) in inject {
        if !out.contains(token) {
            anyhow::bail!("plan_review template missing token {token}");
        }
        out = out.replace(token, value);
    }

    for token in ["__OWNER_REQUEST_JSON__", "__DAGSPEC_JSON__", "__RUN_ID_JSON__"] {
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
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec()).unwrap();
        assert!(py.contains("OWNER_REQUEST = json.loads("));
        assert!(py.contains("plan_review_task"));
        assert!(!py.contains("__OWNER_REQUEST_JSON__"));
        assert!(!py.contains("__DAGSPEC_JSON__"));
    }
}
