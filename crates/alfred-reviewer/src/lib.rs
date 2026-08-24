#[allow(unused_imports)]
use alfred_core::{
    chat_completion, offline_mode, strip_code_fence, Artifact, Confidence, Contract, DagSpec,
    ExecVerdict, LlmRole, Message, OwnerRequest, PlanVerdict, SessionDoc, OFFLINE_ENV,
};
use serde::Serialize;
use std::fmt;
use std::path::Path;

// 模式开关：ALFRED_OFFLINE=1 → 返回确定性 stub（供无 LLM 环境的单测/e2e）；
// 默认走真 LLM 审查。开关语义与 alfred-core 单一真源。

/// 审查失败的唯一错误类型：LLM 调用失败，或模型输出不符合裁决 schema。
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewError {
    // chat_completion 失败（配置缺失 / HTTP / API 错误），原文透传。
    Llm(String),
    // 模型输出不是合法的裁决 JSON（语法错误或 schema 校验失败）。
    Parse(String),
}

impl fmt::Display for ReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(err) => write!(f, "reviewer llm call failed: {err}"),
            Self::Parse(err) => write!(f, "reviewer output is not valid verdict json: {err}"),
        }
    }
}

impl std::error::Error for ReviewError {}

// 计划审查 prompt：审查者全可见（OwnerRequest / DagSpec / SessionDoc / 对话记录），
// 只回答"计划是否忠实于属主需求"，输出严格 JSON。
const PLAN_REVIEW_SYSTEM_PROMPT: &str = "\
你是 alfred 治理环的计划审查 agent。你的唯一职责：判断待审计划（DagSpec）是否忠实于属主需求（OwnerRequest）。
你拥有全量上下文：属主需求、待审计划、会话文档（SessionDoc）、对话记录。
审查要点：
- 计划的节点与边覆盖属主需求的全部验收标准（acceptance_criteria），不缺项、不跑题；
- 每个节点的契约（prompt / acceptance_criteria）可执行、可验收，且与属主需求一致；
- 计划没有引入与需求无关的额外工作。
你只输出这一个 JSON 对象：
{\"pass\": true|false, \"reason\": \"...\"}
- pass：true 表示计划忠实于属主需求，可进入执行；false 表示必须返修。
- reason：一句话说明通过或拒绝的依据；拒绝时指出具体缺漏或偏差。
除该 JSON 对象外不输出任何内容。";

// 执行审查 prompt：以契约的 acceptance_criteria 为唯一验收标准核验产物，
// 输出严格 JSON，value/failure_class 取值集合锁死。
const EXEC_REVIEW_SYSTEM_PROMPT: &str = "\
你是 alfred 治理环的执行审查 agent。你的唯一职责：判断节点产物（Artifact）是否兑现了节点契约（Contract）。
以契约的 acceptance_criteria 为唯一验收标准，对照产物逐项核验：不凭印象放行，也不苛求契约之外的内容。
你只输出这一个 JSON 对象：
{\"value\": \"C\"|\"I\"|\"P\", \"failure_class\": null|\"mechanical\"|\"contract_ambiguity\"|\"fidelity_dispute\"|\"contract_fault\"|\"disagreement\", \"confidence\": \"high\"|\"medium\"|\"low\", \"evidence\": [\"...\"], \"explanation\": \"...\"}
- value：C = 产物正确兑现契约；I = 产物不完整；P = 产物有错。
- failure_class：value 为 C 时必须为 null；否则必填，取值为
  mechanical（机械性失败：文件缺失、命令未执行等）、
  contract_ambiguity（契约本身有歧义，多种解读都合理）、
  fidelity_dispute（产物与契约的忠实度存在争议）、
  contract_fault（契约本身有错，无法照它验收）、
  disagreement（审查者内部无法达成一致）。
- confidence：你对本次裁决的置信度，high / medium / low。
- evidence：支撑裁决的证据条目（文件路径、命令输出摘要等）；无证据留空数组。
- explanation：一段话解释裁决依据。
除该 JSON 对象外不输出任何内容。";

/// 计划审查：判断 DagSpec 是否忠实于 OwnerRequest。
/// 审查者全可见——属主需求、计划、会话文档、对话记录全部进 prompt。
/// ALFRED_OFFLINE=1 时返回确定性 stub，不碰 LLM。
pub fn review_plan(
    request: &OwnerRequest,
    dag_spec: &DagSpec,
    session_doc: &SessionDoc,
    conversation_log: &str,
    run_dir: &Path,
) -> Result<PlanVerdict, ReviewError> {
    if offline_mode() {
        return Ok(review_plan_stub(()));
    }
    let messages = vec![
        Message::system(PLAN_REVIEW_SYSTEM_PROMPT),
        Message::user(format!(
            "属主需求（OwnerRequest）：\n{}\n\n待审计划（DagSpec）：\n{}\n\n会话文档（SessionDoc）：\n{}\n\n对话记录：\n{conversation_log}",
            to_json(request),
            to_json(dag_spec),
            to_json(session_doc),
        )),
    ];
    let raw = chat_completion(LlmRole::Reviewer, &messages, run_dir)
        .map_err(|err| ReviewError::Llm(err.to_string()))?;
    parse_plan_verdict(&raw)
}

/// 执行审查：判断 Artifact 是否兑现 Contract。
/// ALFRED_OFFLINE=1 时返回确定性 stub，不碰 LLM。
pub fn review_execution(
    contract: &Contract,
    artifact: &Artifact,
    run_dir: &Path,
) -> Result<ExecVerdict, ReviewError> {
    if offline_mode() {
        return Ok(review_execution_stub(()));
    }
    let messages = vec![
        Message::system(EXEC_REVIEW_SYSTEM_PROMPT),
        Message::user(format!(
            "节点契约（Contract）：\n{}\n\n节点产物（Artifact）：\n{}",
            to_json(contract),
            to_json(artifact),
        )),
    ];
    let raw = chat_completion(LlmRole::Reviewer, &messages, run_dir)
        .map_err(|err| ReviewError::Llm(err.to_string()))?;
    parse_exec_verdict(&raw)
}

// 离线 stub：ALFRED_OFFLINE=1 时两个审查入口的确定性返回值，单测/e2e 靠它跑通治理环。
pub fn review_plan_stub(_verdict_input: ()) -> PlanVerdict {
    PlanVerdict { pass: true, reason: "offline stub".into() }
}

pub fn review_execution_stub(_verdict_input: ()) -> ExecVerdict {
    ExecVerdict::correct(Confidence::High, vec![], "offline stub".into())
}

fn parse_plan_verdict(raw: &str) -> Result<PlanVerdict, ReviewError> {
    serde_json::from_str::<PlanVerdict>(strip_code_fence(raw))
        .map_err(|err| ReviewError::Parse(format!("{err}; raw output: {}", raw.trim())))
}

fn parse_exec_verdict(raw: &str) -> Result<ExecVerdict, ReviewError> {
    serde_json::from_str::<ExecVerdict>(strip_code_fence(raw))
        .map_err(|err| ReviewError::Parse(format!("{err}; raw output: {}", raw.trim())))
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("core entity serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::{Contract, EdgeSpec, FailureClass, NodeSpec, NodeType, VerdictValue};
    use std::collections::BTreeMap;

    // 离线用例只写同一个值，并发幂等无竞争（与 alfred-planner 测试同一约定）。
    fn force_offline_mode() {
        std::env::set_var(OFFLINE_ENV, "1");
    }

    fn owner_request() -> OwnerRequest {
        OwnerRequest {
            request_id: "req-1".into(),
            requirement: "greet the world".into(),
            acceptance_criteria: "greeting exists".into(),
            dag_spec: None,
        }
    }

    fn single_step_spec() -> DagSpec {
        DagSpec {
            name: "greeting".into(),
            version: 1,
            entrypoint: "step-a".into(),
            nodes: vec![NodeSpec {
                node_id: "step-a".into(),
                node_type: NodeType::Step,
                contract: contract(),
                params: serde_json::Map::new(),
                input_schema: BTreeMap::new(),
                routes: None,
            }],
            edges: Vec::<EdgeSpec>::new(),
        }
    }

    fn session_doc() -> SessionDoc {
        SessionDoc {
            key_file_paths: vec!["src/main.rs".into()],
            key_conclusions: vec!["single step suffices".into()],
            review_summary: String::new(),
        }
    }

    fn contract() -> Contract {
        Contract {
            prompt: "write greeting".into(),
            acceptance_criteria: "greeting is written".into(),
            reviewer_models: vec!["model-a".into()],
        }
    }

    fn artifact() -> Artifact {
        Artifact {
            node_id: "step-a".into(),
            workspace_diff: "diff --git a/greeting.txt b/greeting.txt".into(),
            produced_at: "2026-08-24T00:00:00Z".into(),
        }
    }

    // ---- 合法 JSON 解析 ----

    #[test]
    fn parses_plan_verdict_from_strict_json() {
        let verdict =
            parse_plan_verdict(r#"{"pass": true, "reason": "dag covers the requirement"}"#)
                .expect("valid plan verdict json parses");
        assert_eq!(verdict, PlanVerdict { pass: true, reason: "dag covers the requirement".into() });
    }

    #[test]
    fn parses_plan_verdict_wrapped_in_code_fence() {
        let raw = "```json\n{\"pass\": false, \"reason\": \"missing acceptance coverage\"}\n```";
        let verdict = parse_plan_verdict(raw).expect("fenced json still parses");
        assert_eq!(verdict.pass, false);
        assert_eq!(verdict.reason, "missing acceptance coverage");
    }

    #[test]
    fn parses_exec_verdict_correct_from_strict_json() {
        let raw = r#"{"value": "C", "failure_class": null, "confidence": "high", "evidence": ["greeting.txt exists"], "explanation": "contract fulfilled"}"#;
        let verdict = parse_exec_verdict(raw).expect("valid exec verdict json parses");
        assert_eq!(
            verdict,
            ExecVerdict::correct(
                Confidence::High,
                vec!["greeting.txt exists".into()],
                "contract fulfilled".into(),
            )
        );
    }

    #[test]
    fn parses_all_three_verdict_values() {
        for (raw_value, expected) in
            [("C", VerdictValue::C), ("I", VerdictValue::I), ("P", VerdictValue::P)]
        {
            let raw = format!(
                r#"{{"value": "{raw_value}", "failure_class": null, "confidence": "low", "evidence": [], "explanation": "x"}}"#
            );
            let verdict = parse_exec_verdict(&raw).expect("value variant parses");
            assert_eq!(verdict.value, expected);
        }
    }

    #[test]
    fn parses_all_five_failure_classes() {
        for (raw_class, expected) in [
            ("mechanical", FailureClass::Mechanical),
            ("contract_ambiguity", FailureClass::ContractAmbiguity),
            ("fidelity_dispute", FailureClass::FidelityDispute),
            ("contract_fault", FailureClass::ContractFault),
            ("disagreement", FailureClass::Disagreement),
        ] {
            let raw = format!(
                r#"{{"value": "I", "failure_class": "{raw_class}", "confidence": "medium", "evidence": [], "explanation": "x"}}"#
            );
            let verdict = parse_exec_verdict(&raw).expect("failure class variant parses");
            assert_eq!(verdict.failure_class, Some(expected));
        }
    }

    // ---- 非法 JSON → ReviewError::Parse ----

    #[test]
    fn invalid_plan_json_maps_to_parse_error() {
        let err = parse_plan_verdict("the plan looks fine to me")
            .expect_err("non-json output is a parse error");
        assert!(matches!(err, ReviewError::Parse(_)), "expected Parse, got {err:?}");
    }

    #[test]
    fn schema_violating_plan_json_maps_to_parse_error() {
        let err = parse_plan_verdict(r#"{"pass": "yes", "reason": "ok"}"#)
            .expect_err("wrong field type violates schema");
        assert!(matches!(err, ReviewError::Parse(_)), "expected Parse, got {err:?}");
    }

    #[test]
    fn invalid_exec_json_maps_to_parse_error() {
        let err = parse_exec_verdict(r#"{"value": "Q", "confidence": "high"}"#)
            .expect_err("unknown verdict value violates schema");
        assert!(matches!(err, ReviewError::Parse(_)), "expected Parse, got {err:?}");
    }

    // ---- 离线模式：返回 stub，不碰 LLM ----

    #[test]
    fn offline_review_plan_returns_stub_without_llm() {
        force_offline_mode();
        let run_dir = tempfile::tempdir().expect("temp dir");
        let verdict = review_plan(
            &owner_request(),
            &single_step_spec(),
            &session_doc(),
            "owner: please greet the world",
            run_dir.path(),
        )
        .expect("offline review succeeds");
        assert_eq!(verdict, PlanVerdict { pass: true, reason: "offline stub".into() });
        assert!(
            !run_dir.path().join("llm-calls").exists(),
            "offline mode must not touch the llm (no call records on disk)"
        );
    }

    #[test]
    fn offline_review_execution_returns_stub_without_llm() {
        force_offline_mode();
        let run_dir = tempfile::tempdir().expect("temp dir");
        let verdict = review_execution(&contract(), &artifact(), run_dir.path())
            .expect("offline review succeeds");
        assert_eq!(
            verdict,
            ExecVerdict::correct(Confidence::High, vec![], "offline stub".into())
        );
        assert!(
            !run_dir.path().join("llm-calls").exists(),
            "offline mode must not touch the llm (no call records on disk)"
        );
    }
}
