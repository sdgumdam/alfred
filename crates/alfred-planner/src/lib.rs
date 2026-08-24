use alfred_core::{
    chat_completion, offline_mode, strip_code_fence, CommitError, DagSpec, EdgeSpec,
    GraphBuilder, LlmRole, Message, NodeSpec, OwnerRequest, PlanVerdict, RoutesSpec, SessionDoc,
    ValidationError,
};
use serde_json::Value;
use std::fmt;
use std::path::{Path, PathBuf};

pub const STRUCTURED_INTENT_DRAFT_PREFIX: &str = "draft-";

// 模式开关：ALFRED_OFFLINE=1 → 确定性直通（供无 LLM 环境的单测/e2e）；
// 默认走真 LLM 双 agent。开关语义与 alfred-core 单一真源。
// ALFRED_RUN_DIR 指定 llm-calls 落盘目录，缺省取当前目录。
const RUN_DIR_ENV: &str = "ALFRED_RUN_DIR";

#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    UnsupportedInput,
    Build(CommitError),
    // 对话 agent 选择答复而非产出计划增量（需求尚未收敛成计划）。
    OwnerReply(String),
    // 真 LLM 调用或输出解析失败（配置缺失 / 网络 / 违反输出契约）。
    Llm(String),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // 离线直通模式下 plan 只接受携带结构化计划意图的 OwnerRequest。
            Self::UnsupportedInput => write!(
                f,
                "OwnerRequest carries no dag_spec; offline passthrough (ALFRED_OFFLINE=1) requires the structured intent"
            ),
            Self::Build(err) => write!(f, "{err}"),
            Self::OwnerReply(text) => {
                write!(f, "planner replied instead of producing a plan increment: {text}")
            }
            Self::Llm(err) => write!(f, "llm planner call failed: {err}"),
        }
    }
}

impl std::error::Error for PlanError {}

pub fn plan_owner_request(request: &OwnerRequest) -> Result<DagSpec, PlanError> {
    if offline_mode() {
        return plan_structured_intent(request);
    }
    plan_via_converse(request)
}

// 离线测试模式：结构化意图原样过 builder+校验的确定性直通。
fn plan_structured_intent(request: &OwnerRequest) -> Result<DagSpec, PlanError> {
    let intended_spec = request.dag_spec.as_ref().ok_or(PlanError::UnsupportedInput)?;
    assemble_through_builder(intended_spec).map_err(PlanError::Build)
}

// 真 LLM 路径：空会话文档 + 本轮 OwnerRequest 交给对话 agent，
// 计划增量仍过 builder+校验兜底，答复文本说明需求未收敛。
fn plan_via_converse(request: &OwnerRequest) -> Result<DagSpec, PlanError> {
    let run_dir = std::env::var_os(RUN_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let message = serde_json::to_string(request).expect("OwnerRequest serialization cannot fail");
    match converse(&SessionDoc::default(), &message, &run_dir)? {
        PlannerOutput::PlanIncrement(spec) => {
            assemble_through_builder(&spec).map_err(PlanError::Build)
        }
        PlannerOutput::Reply(text) => Err(PlanError::OwnerReply(text)),
    }
}

// 对话 agent 的输出：计划增量（交给 builder+校验落地）或直接答复属主的文本。
#[derive(Debug, Clone, PartialEq)]
pub enum PlannerOutput {
    PlanIncrement(DagSpec),
    Reply(String),
}

// 维护 agent 的触发事件：计划审查落定，或属主补充了需求。
#[derive(Debug, Clone, PartialEq)]
pub enum MaintainTrigger {
    PlanReviewed(PlanVerdict),
    OwnerMessage(String),
}

const CONVERSE_SYSTEM_PROMPT: &str = r#"你是 alfred 计划器的对话 agent。输入是三段式会话文档（key_file_paths 关键文件路径 / key_conclusions 关键结论 / review_summary 审查结论摘要）与属主本轮消息。
你只输出一个 JSON 对象，二选一：
1. {"instructions": [...]} —— 需求已足够明确，输出建图指令序列。每条指令是一个 JSON 对象，带 op 字段标识操作类型。支持的 op：

- {"op": "begin", "name": "...", "version": 1} — 开始建图。name 是图名称，version 是版本号。必须是指令序列的第一条。
- {"op": "add_node", "node_id": "...", "node_type": "step", "contract": {"prompt": "...", "acceptance_criteria": "...", "reviewer_models": ["model-a"]}} — 添加节点。node_type 可选值：start / end / step / router / batch。contract 是节点契约。
- {"op": "add_edge", "id": "...", "from": "...", "to": "..."} — 添加有向边。from 和 to 是已添加的 node_id。
- {"op": "set_routes", "node_id": "...", "routes": {"choice1": "node1", "choice2": "node2"}} — 为 router 节点设置路由表。key 是路由选项名，value 是目标 node_id。只能对 router 类型节点使用。
- {"op": "commit", "entrypoint": "..."} — 提交建图，产出最终 DagSpec。entrypoint 是入口节点的 node_id。必须是指令序列的最后一条。

示例：
{"instructions": [
  {"op": "begin", "name": "greeting", "version": 1},
  {"op": "add_node", "node_id": "step-a", "node_type": "step", "contract": {"prompt": "write greeting", "acceptance_criteria": "greeting is written", "reviewer_models": ["model-a"]}},
  {"op": "commit", "entrypoint": "step-a"}
]}

2. {"reply": "..."} — 需要向属主追问澄清（需求不明确、缺少关键信息、存在歧义），或本轮无需改计划。

除该 JSON 对象外不输出任何内容。"#;

const MAINTAIN_SYSTEM_PROMPT: &str = "\
你是 alfred 计划器的维护 agent。根据当前会话文档与触发事件（计划审查落定 / 属主补充需求），\
重写会话文档。会话文档格式锁死为三段，你只输出这一个 JSON 对象：
{\"key_file_paths\": [\"...\"], \"key_conclusions\": [\"...\"], \"review_summary\": \"...\"}
- key_file_paths：与需求/计划相关的关键文件路径。
- key_conclusions：对话沉淀下来的关键结论。
- review_summary：计划审查结论摘要；无审查结论时保留原摘要或留空字符串。
除该 JSON 对象外不输出任何内容。";

// 对话 agent：会话文档 + 属主本轮消息 → 计划增量或答复文本。
pub fn converse(
    session_doc: &SessionDoc,
    owner_message: &str,
    run_dir: &Path,
) -> Result<PlannerOutput, PlanError> {
    let messages = vec![
        Message::system(CONVERSE_SYSTEM_PROMPT),
        Message::user(format!(
            "会话文档：\n{}\n\n属主本轮消息：\n{owner_message}",
            serde_json::to_string_pretty(session_doc).expect("SessionDoc serialization cannot fail"),
        )),
    ];
    let raw = chat_completion(LlmRole::Planner, &messages, run_dir)
        .map_err(|err| PlanError::Llm(err.to_string()))?;
    parse_converse_output(&raw)
}

// 维护 agent：计划审查落定后 / 属主补充需求后重写会话文档。
pub fn maintain(
    session_doc: &SessionDoc,
    trigger: &MaintainTrigger,
    run_dir: &Path,
) -> Result<SessionDoc, PlanError> {
    let trigger_json = match trigger {
        MaintainTrigger::PlanReviewed(verdict) => serde_json::json!({ "plan_reviewed": verdict }),
        MaintainTrigger::OwnerMessage(text) => serde_json::json!({ "owner_message": text }),
    };
    let messages = vec![
        Message::system(MAINTAIN_SYSTEM_PROMPT),
        Message::user(format!(
            "当前会话文档：\n{}\n\n触发事件：\n{}",
            serde_json::to_string_pretty(session_doc).expect("SessionDoc serialization cannot fail"),
            serde_json::to_string_pretty(&trigger_json).expect("trigger serialization cannot fail"),
        )),
    ];
    let raw = chat_completion(LlmRole::Planner, &messages, run_dir)
        .map_err(|err| PlanError::Llm(err.to_string()))?;
    serde_json::from_str::<SessionDoc>(strip_code_fence(&raw))
        .map_err(|err| PlanError::Llm(format!("maintain output violates SessionDoc schema: {err}")))
}

fn parse_converse_output(raw: &str) -> Result<PlannerOutput, PlanError> {
    let Ok(value) = serde_json::from_str::<Value>(strip_code_fence(raw)) else {
        // 模型没输出 JSON：把原文当答复文本，对话 agent 永不为难调用方。
        return Ok(PlannerOutput::Reply(raw.trim().to_owned()));
    };
    // reply 分支：模型选择追问而非产出计划。
    if let Some(text) = value.get("reply").and_then(Value::as_str) {
        return Ok(PlannerOutput::Reply(text.to_owned()));
    }
    // instructions 分支：模型输出建图指令序列。
    if let Some(instructions) = value.get("instructions") {
        return execute_builder_instructions(instructions, Path::new("."))
            .map(PlannerOutput::PlanIncrement);
    }
    // 两个分支都没命中：降级为答复文本。
    Ok(PlannerOutput::Reply(raw.trim().to_owned()))
}

/// 逐条执行建图指令序列，驱动 GraphBuilder 产出 DagSpec。
///
/// 每条指令是一个 JSON 对象，带 op 字段标识操作类型：
/// begin / add_node / add_edge / set_routes / commit。
/// 任何一条指令执行失败，错误信息包含指令序号（从 1 开始）、
/// op 名称和底层错误描述。
pub fn execute_builder_instructions(
    instructions: &Value,
    run_dir: &Path,
) -> Result<DagSpec, PlanError> {
    let _ = run_dir; // 预留：未来可用于指令审计落盘。
    let list = instructions.as_array().ok_or_else(|| {
        PlanError::Llm("builder instructions: expected a JSON array".into())
    })?;
    if list.is_empty() {
        return Err(PlanError::Llm(
            "builder instructions: empty instruction sequence".into(),
        ));
    }

    let draft_id = format!("{STRUCTURED_INTENT_DRAFT_PREFIX}converse");
    let mut builder = GraphBuilder::new();
    let mut began = false;
    let mut committed = false;
    let mut entrypoint = String::new();

    for (index, instruction) in list.iter().enumerate() {
        let step = index + 1; // 人类可读的指令序号，从 1 开始。
        let op = instruction
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PlanError::Llm(format!(
                    "instruction #{step}: missing op field"
                ))
            })?;
        match op {
            "begin" => {
                let name = instruction
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlanError::Llm(format!(
                            "instruction #{step} begin: missing name field"
                        ))
                    })?;
                // version 字段当前仅作校验，GraphBuilder 内部固定 version=1。
                let _version = instruction.get("version").and_then(Value::as_u64);
                builder.begin(&draft_id, name).map_err(|err| {
                    PlanError::Llm(format!(
                        "instruction #{step} begin: {err}"
                    ))
                })?;
                began = true;
            }
            "add_node" => {
                if !began {
                    return Err(PlanError::Llm(format!(
                        "instruction #{step} add_node: begin must be the first instruction"
                    )));
                }
                let node_id = instruction
                    .get("node_id")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                // 剥掉 op 字段再反序列化为 NodeSpec。
                let mut node_value = instruction.clone();
                node_value.as_object_mut().expect("instruction is object").remove("op");
                let node: NodeSpec =
                    serde_json::from_value(node_value).map_err(|err| {
                        PlanError::Llm(format!(
                            "instruction #{step} add_node({node_id}): invalid node schema: {err}"
                        ))
                    })?;
                builder.add_node(&draft_id, node).map_err(|err| {
                    PlanError::Build(CommitError::Builder(err))
                })?;
            }
            "add_edge" => {
                if !began {
                    return Err(PlanError::Llm(format!(
                        "instruction #{step} add_edge: begin must be the first instruction"
                    )));
                }
                let edge_id = instruction
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                // 剥掉 op 字段再反序列化为 EdgeSpec。
                let mut edge_value = instruction.clone();
                edge_value.as_object_mut().expect("instruction is object").remove("op");
                let edge: EdgeSpec =
                    serde_json::from_value(edge_value).map_err(|err| {
                        PlanError::Llm(format!(
                            "instruction #{step} add_edge({edge_id}): invalid edge schema: {err}"
                        ))
                    })?;
                builder.add_edge(&draft_id, edge).map_err(|err| {
                    PlanError::Build(CommitError::Builder(err))
                })?;
            }
            "set_routes" => {
                if !began {
                    return Err(PlanError::Llm(format!(
                        "instruction #{step} set_routes: begin must be the first instruction"
                    )));
                }
                let node_id = instruction
                    .get("node_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlanError::Llm(format!(
                            "instruction #{step} set_routes: missing node_id field"
                        ))
                    })?;
                // 指令格式是扁平 map {"choice1": "node1"}，需要包成 RoutesSpec { choices: {...} }。
                let flat_routes = instruction
                    .get("routes")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        PlanError::Llm(format!(
                            "instruction #{step} set_routes({node_id}): routes must be a JSON object"
                        ))
                    })?;
                let choices: std::collections::BTreeMap<String, String> = flat_routes
                    .iter()
                    .map(|(k, v)| {
                        let target = v.as_str().ok_or_else(|| {
                            PlanError::Llm(format!(
                                "instruction #{step} set_routes({node_id}): route target for '{k}' must be a string"
                            ))
                        })?;
                        Ok((k.clone(), target.to_owned()))
                    })
                    .collect::<Result<_, PlanError>>()?;
                let routes = RoutesSpec { choices };
                builder.set_routes(&draft_id, node_id, routes).map_err(|err| {
                    PlanError::Build(CommitError::Builder(err))
                })?;
            }
            "commit" => {
                if !began {
                    return Err(PlanError::Llm(format!(
                        "instruction #{step} commit: begin must be the first instruction"
                    )));
                }
                entrypoint = instruction
                    .get("entrypoint")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlanError::Llm(format!(
                            "instruction #{step} commit: missing entrypoint field"
                        ))
                    })?
                    .to_owned();
                committed = true;
            }
            _ => {
                return Err(PlanError::Llm(format!(
                    "instruction #{step}: unknown op '{op}'"
                )));
            }
        }
    }

    if !began {
        return Err(PlanError::Llm(
            "builder instructions: missing begin instruction".into(),
        ));
    }
    if !committed {
        return Err(PlanError::Llm(
            "builder instructions: missing commit instruction".into(),
        ));
    }

    builder.commit(&draft_id, &entrypoint).map_err(PlanError::Build)
}

fn assemble_through_builder(intended_spec: &DagSpec) -> Result<DagSpec, CommitError> {
    let draft_id = format!("{STRUCTURED_INTENT_DRAFT_PREFIX}{}", intended_spec.name);
    let mut builder = GraphBuilder::new();
    if builder.begin(draft_id.clone(), intended_spec.name.clone()).is_err() {
        unreachable!("a fresh builder never has a draft with this id");
    }
    for node in &intended_spec.nodes {
        builder.add_node(&draft_id, node.clone()).map_err(CommitError::Builder)?;
    }
    for edge in &intended_spec.edges {
        builder.add_edge(&draft_id, edge.clone()).map_err(CommitError::Builder)?;
    }
    for node in &intended_spec.nodes {
        if let Some(routes) = &node.routes {
            builder
                .set_routes(&draft_id, &node.node_id, routes.clone())
                .map_err(CommitError::Builder)?;
        }
    }
    builder.commit(&draft_id, &intended_spec.entrypoint)
}

// 所有拒绝路径（schema 反序列化失败 / 无计划意图 / 结构校验失败）收敛成
// 同一份机器可读拒绝报告的唯一定义点，CLI 原样透传。
pub fn rejection_report(request_path: &Path, err: &PlanError) -> Value {
    let mut report = serde_json::Map::new();
    report.insert("error".into(), "owner_request_rejected".into());
    report.insert("source_file".into(), request_path.display().to_string().into());
    match err {
        PlanError::UnsupportedInput | PlanError::OwnerReply(_) | PlanError::Llm(_) => {
            report.insert("reason".into(), err.to_string().into());
        }
        PlanError::Build(CommitError::Builder(builder_err)) => {
            report.insert("reason".into(), builder_err.to_string().into());
        }
        PlanError::Build(CommitError::Validation(ValidationError { issues, raw_json })) => {
            let issue_reports = issues
                .iter()
                .map(|issue| serde_json::to_value(issue.report()).expect("issue serialization cannot fail"))
                .collect::<Vec<_>>();
            report.insert("issues".into(), Value::Array(issue_reports));
            if let Some(raw) = raw_json {
                report.insert("raw_json".into(), raw.clone());
            }
        }
    }
    Value::Object(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::{Contract, EdgeSpec, NodeSpec, NodeType, OFFLINE_ENV};
    use std::collections::BTreeMap;

    // 全部用例钉在离线直通模式（并发测试写同一个值，幂等无竞争）。
    fn force_offline_mode() {
        std::env::set_var(OFFLINE_ENV, "1");
    }

    fn owner_request_with(spec: DagSpec) -> OwnerRequest {
        OwnerRequest {
            request_id: "req-1".into(),
            requirement: "greet the world".into(),
            acceptance_criteria: "greeting exists".into(),
            dag_spec: Some(spec),
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
                contract: Contract {
                    prompt: "write greeting".into(),
                    acceptance_criteria: "greeting is written".into(),
                    reviewer_models: vec!["model-a".into()],
                },
                params: serde_json::Map::new(),
                input_schema: BTreeMap::new(),
                routes: None,
            }],
            edges: vec![],
        }
    }

    #[test]
    fn structured_intent_request_plans_through_builder() {
        force_offline_mode();
        let request = owner_request_with(single_step_spec());

        let planned = plan_owner_request(&request).expect("valid intent plans through builder");

        assert_eq!(planned, single_step_spec());
    }

    #[test]
    fn request_without_intent_is_refused() {
        force_offline_mode();
        let mut request = owner_request_with(single_step_spec());
        request.dag_spec = None;

        let err = plan_owner_request(&request).expect_err("no intent, no plan");
        assert_eq!(err, PlanError::UnsupportedInput);
    }

    #[test]
    fn invalid_intent_surfaces_validation_issues() {
        force_offline_mode();
        let mut spec = single_step_spec();
        spec.nodes[0].contract.reviewer_models.clear();
        let request = owner_request_with(spec);

        let err = plan_owner_request(&request).expect_err("incomplete contract rejected");
        let report = rejection_report(Path::new("request.json"), &err);
        let rendered = report.to_string();
        assert!(rendered.contains("CONTRACT_INCOMPLETE"));
        assert!(rendered.contains("step-a"));
        assert!(rendered.contains("contract.reviewer_models"));
    }

    #[test]
    fn disconnected_graph_is_rejected_until_repaired() {
        force_offline_mode();
        let request_json = r#"{
            "request_id": "req-2",
            "requirement": "two steps",
            "acceptance_criteria": "both steps done",
            "dag_spec": {
                "name": "two-steps",
                "version": 1,
                "entrypoint": "step-a",
                "nodes": [
                    {"node_id": "step-a", "node_type": "step", "contract": {"prompt": "a", "acceptance_criteria": "a done", "reviewer_models": ["m"]}},
                    {"node_id": "step-b", "node_type": "step", "contract": {"prompt": "b", "acceptance_criteria": "b done", "reviewer_models": ["m"]}}
                ],
                "edges": []
            }
        }"#;
        let request: OwnerRequest = serde_json::from_str(request_json).expect("fixture parses");
        let err = plan_owner_request(&request).expect_err("disconnected second node rejected");
        assert!(err.to_string().contains("UNREACHABLE_NODE"));

        let mut repaired: OwnerRequest = serde_json::from_str(request_json).expect("fixture parses");
        let spec = repaired.dag_spec.as_mut().expect("intent present");
        spec.edges.push(EdgeSpec { id: "e1".into(), from: "step-a".into(), to: "step-b".into() });
        assert!(plan_owner_request(&repaired).is_ok());
    }

    // --- execute_builder_instructions 单测 ---

    /// 合法指令序列产出合法 DagSpec。
    #[test]
    fn valid_instructions_produce_dagspec() {
        let instructions = serde_json::json!([
            {"op": "begin", "name": "greeting", "version": 1},
            {"op": "add_node", "node_id": "step-a", "node_type": "step", "contract": {"prompt": "write greeting", "acceptance_criteria": "greeting is written", "reviewer_models": ["model-a"]}},
            {"op": "commit", "entrypoint": "step-a"}
        ]);

        let spec = execute_builder_instructions(&instructions, Path::new("."))
            .expect("valid instructions should produce DagSpec");

        assert_eq!(spec.name, "greeting");
        assert_eq!(spec.entrypoint, "step-a");
        assert_eq!(spec.nodes.len(), 1);
        assert_eq!(spec.nodes[0].node_id, "step-a");
    }

    /// 多节点 + 边 + 路由的完整指令序列。
    #[test]
    fn multi_node_instructions_with_routes() {
        let instructions = serde_json::json!([
            {"op": "begin", "name": "router-flow", "version": 1},
            {"op": "add_node", "node_id": "step-a", "node_type": "step", "contract": {"prompt": "do a", "acceptance_criteria": "a done", "reviewer_models": ["m"]}},
            {"op": "add_node", "node_id": "router-b", "node_type": "router", "contract": {"prompt": "route", "acceptance_criteria": "routed", "reviewer_models": ["m"]}},
            {"op": "add_node", "node_id": "step-c", "node_type": "step", "contract": {"prompt": "do c", "acceptance_criteria": "c done", "reviewer_models": ["m"]}},
            {"op": "add_edge", "id": "e1", "from": "step-a", "to": "router-b"},
            {"op": "add_edge", "id": "e2", "from": "router-b", "to": "step-c"},
            {"op": "set_routes", "node_id": "router-b", "routes": {"go-c": "step-c"}},
            {"op": "commit", "entrypoint": "step-a"}
        ]);

        let spec = execute_builder_instructions(&instructions, Path::new("."))
            .expect("multi-node instructions should produce DagSpec");

        assert_eq!(spec.nodes.len(), 3);
        assert_eq!(spec.edges.len(), 2);
        let router = spec.nodes.iter().find(|n| n.node_id == "router-b").expect("router exists");
        assert!(router.routes.is_some());
    }

    /// 非法指令序列报错：报出第几条什么错（missing op field）。
    #[test]
    fn invalid_instruction_missing_op_reports_step() {
        let instructions = serde_json::json!([
            {"op": "begin", "name": "test", "version": 1},
            {"name": "no-op-field"},
        ]);

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("missing op should fail");

        let msg = err.to_string();
        assert!(msg.contains("instruction #2"), "should mention step 2, got: {msg}");
        assert!(msg.contains("missing op"), "should mention missing op, got: {msg}");
    }

    /// 未知 op 报错：报出第几条什么错。
    #[test]
    fn invalid_instruction_unknown_op_reports_step_and_op() {
        let instructions = serde_json::json!([
            {"op": "begin", "name": "test", "version": 1},
            {"op": "explode", "detail": "boom"},
        ]);

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("unknown op should fail");

        let msg = err.to_string();
        assert!(msg.contains("instruction #2"), "should mention step 2, got: {msg}");
        assert!(msg.contains("explode"), "should mention the op name, got: {msg}");
    }

    /// add_node 在 begin 之前执行报错。
    #[test]
    fn add_node_before_begin_fails() {
        let instructions = serde_json::json!([
            {"op": "add_node", "node_id": "step-a", "node_type": "step", "contract": {"prompt": "a", "acceptance_criteria": "a done", "reviewer_models": ["m"]}},
        ]);

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("add_node before begin should fail");

        let msg = err.to_string();
        assert!(msg.contains("instruction #1"), "should mention step 1, got: {msg}");
        assert!(msg.contains("begin must be the first"), "should mention begin constraint, got: {msg}");
    }

    /// reply 指令返回 OwnerReply。
    #[test]
    fn reply_returns_owner_reply() {
        let raw = r#"{"reply": "需要你提供更多细节"}"#;

        let output = parse_converse_output(raw).expect("reply should parse");

        assert_eq!(output, PlannerOutput::Reply("需要你提供更多细节".into()));
    }

    /// 空指令序列报错。
    #[test]
    fn empty_instructions_fails() {
        let instructions = serde_json::json!([]);

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("empty instructions should fail");

        assert!(err.to_string().contains("empty instruction sequence"));
    }

    /// 非数组指令序列报错。
    #[test]
    fn non_array_instructions_fails() {
        let instructions = serde_json::json!({"not": "an array"});

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("non-array instructions should fail");

        assert!(err.to_string().contains("expected a JSON array"));
    }

    /// begin 缺失报错。
    #[test]
    fn missing_begin_fails() {
        let instructions = serde_json::json!([
            {"op": "commit", "entrypoint": "step-a"}
        ]);

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("missing begin should fail");

        assert!(err.to_string().contains("begin must be the first instruction"));
    }

    /// commit 缺失报错。
    #[test]
    fn missing_commit_fails() {
        let instructions = serde_json::json!([
            {"op": "begin", "name": "test", "version": 1},
            {"op": "add_node", "node_id": "step-a", "node_type": "step", "contract": {"prompt": "a", "acceptance_criteria": "a done", "reviewer_models": ["m"]}},
        ]);

        let err = execute_builder_instructions(&instructions, Path::new("."))
            .expect_err("missing commit should fail");

        let msg = err.to_string();
        assert!(msg.contains("missing commit instruction"), "got: {msg}");
    }

    /// parse_converse_output 正确处理 instructions 字段。
    #[test]
    fn parse_converse_output_with_instructions() {
        let raw = r#"{"instructions": [
            {"op": "begin", "name": "test", "version": 1},
            {"op": "add_node", "node_id": "s1", "node_type": "step", "contract": {"prompt": "a", "acceptance_criteria": "b", "reviewer_models": ["m"]}},
            {"op": "commit", "entrypoint": "s1"}
        ]}"#;

        let output = parse_converse_output(raw).expect("instructions should parse");
        match output {
            PlannerOutput::PlanIncrement(spec) => {
                assert_eq!(spec.name, "test");
                assert_eq!(spec.entrypoint, "s1");
            }
            PlannerOutput::Reply(text) => panic!("expected PlanIncrement, got Reply: {text}"),
        }
    }

    /// parse_converse_output 正确处理 reply 字段。
    #[test]
    fn parse_converse_output_with_reply() {
        let raw = r#"{"reply": "请补充更多信息"}"#;

        let output = parse_converse_output(raw).expect("reply should parse");

        assert_eq!(output, PlannerOutput::Reply("请补充更多信息".into()));
    }

    /// plan_via_converse 的离线直通路径不受指令模式影响。
    #[test]
    fn offline_passthrough_not_affected_by_instruction_mode() {
        force_offline_mode();
        let request = owner_request_with(single_step_spec());
        let planned = plan_owner_request(&request).expect("offline passthrough still works");
        assert_eq!(planned, single_step_spec());
    }
}
