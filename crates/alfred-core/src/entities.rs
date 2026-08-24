use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const END_NODE: &str = "__end__";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerRequest {
    pub request_id: String,
    pub requirement: String,
    pub acceptance_criteria: String,
    // S0 取舍：owner request 可携带结构化计划意图，离线测试模式
    // （ALFRED_OFFLINE=1）的 plan 路径用它走 builder+校验；真 LLM 路径忽略它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dag_spec: Option<DagSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagSpec {
    pub name: String,
    pub version: u32,
    pub entrypoint: String,
    pub nodes: Vec<NodeSpec>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub node_id: String,
    pub node_type: NodeType,
    // 取舍：契约是强类型必填字段而非藏在 params:Any 里——
    // 缺字段在反序列化即硬失败，不落运行时探测。
    pub contract: Contract,
    #[serde(default)]
    pub params: Map<String, Value>,
    #[serde(default)]
    pub input_schema: BTreeMap<String, InputValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<RoutesSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Start,
    End,
    Step,
    Router,
    Batch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub prompt: String,
    pub acceptance_criteria: String,
    pub reviewer_models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSpec {
    pub id: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutesSpec {
    pub choices: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeFieldRef {
    pub node: String,
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputValue {
    Value(Value),
    Ref(NodeFieldRef),
    Refs(BTreeMap<String, InputValue>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAssignment {
    pub task_id: String,
    /// handler 标识执行-审查-裁决三段流水线；取值见 `HANDLER_RUN_INSPECT_EVAL` 常量。
    /// S1 阶段 executor 消费 contract.prompt，handler 字段 reserved——保留给
    /// 后续 Inspect AI 集成按 handler 值分派不同执行策略。
    pub handler: String,
    pub contract: Contract,
    #[serde(default)]
    pub params: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResult {
    pub status: TaskStatus,
    #[serde(default)]
    pub output: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VerdictValue {
    C,
    I,
    P,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Mechanical,
    ContractAmbiguity,
    FidelityDispute,
    ContractFault,
    Disagreement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

// PlanVerdict / ExecVerdict 的唯一真源在 verdict.rs（治理环裁决实体），
// 此处只保留通用实体，避免两处真源漂移。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub node_id: String,
    pub workspace_diff: String,
    pub produced_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_request_roundtrips_through_json() {
        let dag = DagSpec {
            name: "roundtrip".into(),
            version: 3,
            entrypoint: "n1".into(),
            nodes: vec![NodeSpec {
                node_id: "n1".into(),
                node_type: NodeType::Step,
                contract: Contract {
                    prompt: "p".into(),
                    acceptance_criteria: "a".into(),
                    reviewer_models: vec!["m1".into(), "m2".into()],
                },
                params: Map::new(),
                input_schema: BTreeMap::from([(
                    "source".into(),
                    InputValue::Ref(NodeFieldRef { node: "n0".into(), field: "out".into() }),
                )]),
                routes: None,
            }],
            edges: vec![EdgeSpec { id: "e1".into(), from: "n1".into(), to: END_NODE.into() }],
        };
        let request = OwnerRequest {
            request_id: "req-x".into(),
            requirement: "build thing".into(),
            acceptance_criteria: "thing built".into(),
            dag_spec: Some(dag),
        };

        let json = serde_json::to_string(&request).expect("serializes");
        let restored: OwnerRequest = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(restored, request);
    }

    #[test]
    fn verdict_enums_serialize_with_documented_literals() {
        assert_eq!(serde_json::to_string(&VerdictValue::C).expect("c"), "\"C\"");
        assert_eq!(serde_json::to_string(&VerdictValue::I).expect("i"), "\"I\"");
        assert_eq!(serde_json::to_string(&VerdictValue::P).expect("p"), "\"P\"");
        assert_eq!(
            serde_json::to_string(&FailureClass::ContractAmbiguity).expect("enum"),
            "\"contract_ambiguity\""
        );
        assert_eq!(serde_json::to_string(&Confidence::High).expect("enum"), "\"high\"");
    }

    #[test]
    fn unknown_json_fields_are_rejected_at_deserialize_time() {
        let json = r#"{"request_id":"r","requirement":"x","acceptance_criteria":"a","mystery":"field"}"#;
        let err = serde_json::from_str::<OwnerRequest>(json).expect_err("strict schema rejects unknown field");
        assert!(err.to_string().contains("mystery"));
    }

    #[test]
    fn missing_required_field_is_rejected_at_deserialize_time() {
        let json = r#"{"requirement":"x","acceptance_criteria":"a"}"#;
        let err = serde_json::from_str::<OwnerRequest>(json).expect_err("missing request_id rejected");
        assert!(err.to_string().contains("request_id"));
    }
}

