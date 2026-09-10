//! alfred-reviewer 黑盒测试：执行审查输入投影（M4 多节点全节点契约）。
//!
//! 断言面 = [`exec_review_inputs`] 落盘前的输入集（审查输入文件的可观测契约，
//! e2e multinode.sh 对磁盘产物断言同一形状）：
//! - 单节点：request.json + contract.json（首节点契约）+ sandbox.json——既有
//!   形态不变（旧断言面零回归）；
//! - 多节点：request.json + dagspec.json（全节点契约拼接：每节点 prompt/验收
//!   标准 + sandbox.workspace_subdirs 产物归属 + edges 依赖序）——**不含**
//!   首节点契约投影（M4 修的漏审形态：按首节点契约判全 ws）；
//! - 空 dagspec 显式 Err。

use alfred_core::{Contract, DagSpec, Edge, OwnerRequest, PlanNode, SandboxProfile};
use alfred_reviewer::host::exec_review_inputs;

fn fixture_request() -> OwnerRequest {
    OwnerRequest::new(
        "req-m4",
        "notes then report",
        "First create notes.md as bullets. Then, based on notes.md, create report.md as full sentences.",
        "notes.md exists with bullet items; report.md exists and covers the same points as full sentences",
    )
}

fn fixture_contract(prompt: &str, acceptance: &str) -> Contract {
    Contract {
        prompt: prompt.to_string(),
        acceptance_criteria: acceptance.to_string(),
        reviewer_models: Vec::new(),
    }
}

fn fixture_subdirs(subdirs: &[&str]) -> SandboxProfile {
    SandboxProfile {
        workspace_subdirs: subdirs.iter().map(|s| s.to_string()).collect(),
        ..SandboxProfile::default()
    }
}

/// 输入集 → (文件名, 内容) map（断言用；重名即投影 bug，先炸）。
fn inputs_map(request: &OwnerRequest, dagspec: &DagSpec) -> std::collections::HashMap<String, String> {
    let inputs = exec_review_inputs(request, dagspec).expect("exec_review_inputs");
    let mut map = std::collections::HashMap::new();
    for (name, content) in inputs {
        if map.insert(name.clone(), content).is_some() {
            panic!("duplicate input file: {name}");
        }
    }
    map
}

#[test]
fn single_node_inputs_keep_legacy_contract_shape() {
    // 单节点（现状形态）：request.json + contract.json（首节点契约全字段）+
    // sandbox.json（挂载语义）；无 dagspec.json——旧断言面零回归。
    let request = fixture_request();
    let node = PlanNode::new(
        "task-1",
        "write notes.md",
        fixture_contract(
            "Create notes.md in the workspace summarizing the key points as bullet items.",
            "notes.md exists and contains bullet items",
        ),
    );
    let dagspec = DagSpec::new("req-m4", vec![node]);

    let map = inputs_map(&request, &dagspec);
    assert_eq!(map.len(), 3, "单节点输入集 = request + contract + sandbox");
    assert!(map.contains_key("request.json"));

    let contract: Contract = serde_json::from_str(&map["contract.json"]).expect("contract.json 合法");
    assert_eq!(contract.prompt, "Create notes.md in the workspace summarizing the key points as bullet items.");
    assert_eq!(contract.acceptance_criteria, "notes.md exists and contains bullet items");

    let sandbox: serde_json::Value = serde_json::from_str(&map["sandbox.json"]).expect("sandbox.json 合法");
    assert_eq!(sandbox["workspace_subdirs"], serde_json::json!([]));

    assert!(!map.contains_key("dagspec.json"), "单节点不落 dagspec.json");
}

#[test]
fn single_node_inputs_carry_workspace_subdirs() {
    // 单节点带 workspace_subdirs：sandbox.json 照实投影（挂载语义输入不丢）。
    let request = fixture_request();
    let mut node = PlanNode::new(
        "task-1",
        "write hello.txt",
        fixture_contract("Create hello.txt.", "hello.txt exists with content Hello"),
    );
    node.sandbox = fixture_subdirs(&["src"]);
    let dagspec = DagSpec::new("req-m4", vec![node]);

    let map = inputs_map(&request, &dagspec);
    let sandbox: serde_json::Value = serde_json::from_str(&map["sandbox.json"]).unwrap();
    assert_eq!(sandbox["workspace_subdirs"], serde_json::json!(["src"]));
}

#[test]
fn multi_node_inputs_concatenate_all_node_contracts() {
    // M4 核心：多节点审查输入 = 全节点契约拼接（dagspec.json）——两节点的
    // prompt/验收标准 + 各自产物归属（sandbox.workspace_subdirs）+ edges
    // 依赖序全部进输入；reviewer 一次看全图。
    let request = fixture_request();
    let notes = PlanNode::new(
        "task-1",
        "write notes.md bullet points",
        fixture_contract(
            "Create notes.md in the workspace summarizing the key points as bullet items.",
            "notes.md exists and contains bullet items",
        ),
    );
    let mut report = PlanNode::new(
        "task-2",
        "write report.md from notes.md",
        fixture_contract(
            "Based on the notes.md produced by the prerequisite task, create report.md in the workspace covering the same points as full sentences.",
            "report.md exists and covers the same points as full sentences",
        ),
    );
    report.sandbox = fixture_subdirs(&["src"]);
    let mut dagspec = DagSpec::new("req-m4", vec![notes, report]);
    dagspec.edges = vec![Edge {
        from: "task-1".to_string(),
        to: "task-2".to_string(),
    }];

    let map = inputs_map(&request, &dagspec);
    assert_eq!(map.len(), 2, "多节点输入集 = request + dagspec");
    assert!(map.contains_key("request.json"));

    let dag: DagSpec = serde_json::from_str(&map["dagspec.json"]).expect("dagspec.json 合法");
    assert_eq!(dag.request_id, "req-m4");
    let ids: Vec<&str> = dag.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["task-1", "task-2"]);
    // 两节点契约都进输入（M4：不再只按首节点契约审全 ws）。
    assert_eq!(
        dag.nodes[0].contract.prompt,
        "Create notes.md in the workspace summarizing the key points as bullet items."
    );
    assert_eq!(dag.nodes[0].contract.acceptance_criteria, "notes.md exists and contains bullet items");
    assert_eq!(
        dag.nodes[1].contract.prompt,
        "Based on the notes.md produced by the prerequisite task, create report.md in the workspace covering the same points as full sentences."
    );
    assert_eq!(
        dag.nodes[1].contract.acceptance_criteria,
        "report.md exists and covers the same points as full sentences"
    );
    // 产物归属随节点进输入（该节点 workspace_subdirs）。
    assert_eq!(dag.nodes[1].sandbox.workspace_subdirs, vec!["src".to_string()]);
    // 依赖序随图进输入（下游契约引用上游产物的核验依据）。
    assert_eq!(
        dag.edges,
        vec![Edge {
            from: "task-1".to_string(),
            to: "task-2".to_string()
        }]
    );

    // 漏审形态必须消失：多节点输入不得再含首节点契约投影 / 单节点挂载语义。
    assert!(
        !map.contains_key("contract.json"),
        "多节点不得落首节点契约投影 contract.json（M4 漏审形态）"
    );
    assert!(
        !map.contains_key("sandbox.json"),
        "多节点不得落单节点挂载语义 sandbox.json（产物归属已随 dagspec 节点进输入）"
    );
}

#[test]
fn multi_node_without_edges_still_uses_dagspec_input() {
    // 多节点判定口径 = nodes.len() > 1（孤立节点图同样全节点进输入，不依赖
    // edges 是否声明）。
    let request = fixture_request();
    let a = PlanNode::new("task-1", "a", fixture_contract("Do A.", "A exists"));
    let b = PlanNode::new("task-2", "b", fixture_contract("Do B.", "B exists"));
    let dagspec = DagSpec::new("req-m4", vec![a, b]);

    let map = inputs_map(&request, &dagspec);
    assert!(map.contains_key("dagspec.json"));
    assert!(!map.contains_key("contract.json"));
    let dag: DagSpec = serde_json::from_str(&map["dagspec.json"]).unwrap();
    assert_eq!(dag.nodes.len(), 2);
    assert!(dag.edges.is_empty());
}

#[test]
fn empty_dagspec_is_rejected() {
    // 空 dagspec 显式 Err（治理环同款文案；审查输入无契约可写）。
    let request = fixture_request();
    let dagspec = DagSpec::new("req-m4", Vec::new());
    let err = exec_review_inputs(&request, &dagspec).expect_err("空 dagspec 应显式拒绝");
    assert!(
        err.to_string().contains("dagspec has no nodes"),
        "错误文案应点名空图: {err:#}"
    );
}
