//! alfred-core 黑盒测试：实体 schema 与不变量（公共 API）。

use alfred_core::{
    Artifact, ChangeKind, Confidence, Contract, ExecVerdict, FailureClass, FileChange,
    FileEntry, OwnerRequest, SandboxProfile, TaskAssignment, VerdictGrade,
};

#[test]
fn volume_mount_accepts_readonly_bool_alias() {
    // 原始用例实测形态（pi 按教学输出把 mode 写成 readonly: true）——必须解析
    // 归一成 mode="ro"，而不是 planning_error_escalated。
    let v: alfred_core::VolumeMount = serde_json::from_str(
        r#"{"host_path":"/tmp/docs","container_path":"/references","readonly":true}"#,
    )
    .expect("readonly: true 别名必须解析");
    assert_eq!(v.mode, "ro");
    assert_eq!(v.host_path, "/tmp/docs");
    assert_eq!(v.container_path, "/references");

    // readonly: false 照实落 "rw"（语义不静默改写——只读闸门由执行侧显式拒绝）。
    let rw: alfred_core::VolumeMount = serde_json::from_str(
        r#"{"host_path":"/tmp/docs","container_path":"/references","readonly":false}"#,
    )
    .expect("readonly: false 别名必须解析");
    assert_eq!(rw.mode, "rw");
}

#[test]
fn volume_mount_accepts_readonly_string_aliases() {
    for (alias, want) in [
        ("ro", "ro"),
        ("readonly", "ro"),
        ("read-only", "ro"),
        ("read_only", "ro"),
        ("rw", "rw"),
        ("readwrite", "rw"),
        ("read-write", "rw"),
        ("read_write", "rw"),
    ] {
        let json = format!(
            r#"{{"host_path":"/tmp/docs","container_path":"/references","readonly":"{alias}"}}"#
        );
        let v: alfred_core::VolumeMount =
            serde_json::from_str(&json).unwrap_or_else(|e| panic!("readonly=\"{alias}\" 应解析: {e}"));
        assert_eq!(v.mode, want, "readonly=\"{alias}\" → mode={want}");
    }
    // read_only（蛇形拼写）同面。
    let snake: alfred_core::VolumeMount = serde_json::from_str(
        r#"{"host_path":"/tmp/docs","container_path":"/references","read_only":true}"#,
    )
    .expect("read_only: true 别名必须解析");
    assert_eq!(snake.mode, "ro");
}

#[test]
fn volume_mount_canonical_mode_wins_and_defaults_ro() {
    // 规范 mode 字段在场即权威（原样透传）。
    let v: alfred_core::VolumeMount = serde_json::from_str(
        r#"{"host_path":"/tmp/docs","container_path":"/references","mode":"rw"}"#,
    )
    .expect("canonical mode 解析");
    assert_eq!(v.mode, "rw");
    // 无任何 mode/别名字段 → 缺省 ro（legacy JSON 兼容不变）。
    let legacy: alfred_core::VolumeMount =
        serde_json::from_str(r#"{"host_path":"/tmp/x","container_path":"/references"}"#)
            .expect("legacy JSON 解析");
    assert_eq!(legacy.mode, "ro");
    // 非法 mode 值 → 报错（不静默归一）。
    let bad = serde_json::from_str::<alfred_core::VolumeMount>(
        r#"{"host_path":"/tmp/docs","container_path":"/references","readonly":"bogus"}"#,
    );
    assert!(bad.is_err(), "非法 mode 值必须报错");
}

#[test]
fn volume_mount_still_denies_unknown_fields() {
    // 容错只认 readonly/read_only 两个别名；其余未知字段仍拒（字段漂移防线不变）。
    let bad = serde_json::from_str::<alfred_core::VolumeMount>(
        r#"{"host_path":"/tmp/docs","container_path":"/references","mount_options":"ro"}"#,
    );
    assert!(bad.is_err(), "未知字段必须仍被拒绝");
}

#[test]
fn volume_mount_serializes_canonical_form() {
    let v = alfred_core::VolumeMount {
        host_path: "/tmp/docs".into(),
        container_path: "/references".into(),
        mode: "ro".into(),
    };
    let json = serde_json::to_string(&v).unwrap();
    // 序列化仍是规范三字段（round-trip 稳定；别名只在入方向）。
    assert!(json.contains(r#""mode":"ro""#), "{json}");
    assert!(!json.contains("readonly"), "{json}");
    let back: alfred_core::VolumeMount = serde_json::from_str(&json).unwrap();
    assert_eq!(v, back);
}

fn sample_request() -> OwnerRequest {
    OwnerRequest::new(
        "req-1",
        "create hello.txt",
        "Create a file named hello.txt with content Hello",
        "hello.txt exists and its content is exactly 'Hello'",
    )
}

#[test]
fn owner_request_round_trip_denies_unknown_fields() {
    let req = sample_request();
    let json = serde_json::to_string(&req).unwrap();
    let back: OwnerRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(req, back);

    // deny_unknown_fields：未知字段必须报错
    let bad = r#"{"id":"x","title":"t","description":"d","acceptance_criteria":"a","created_at":"2026-01-01T00:00:00Z","extra":1}"#;
    assert!(serde_json::from_str::<OwnerRequest>(bad).is_err());
}

#[test]
fn task_assignment_round_trip() {
    let contract = Contract {
        prompt: "do it".into(),
        acceptance_criteria: "done".into(),
        reviewer_models: vec!["glm-5.2".into()],
    };
    let ta = TaskAssignment::new("task-1", contract);
    let json = serde_json::to_string(&ta).unwrap();
    let back: TaskAssignment = serde_json::from_str(&json).unwrap();
    assert_eq!(ta, back);
    assert_eq!(back.handler, "run_inspect_eval");
    assert_eq!(back.sandbox, SandboxProfile::default());
    assert!(!back.sandbox.network, "联网默认拒绝 (P2)");
}

#[test]
fn sandbox_profile_defaults_network_off() {
    let p = SandboxProfile::default();
    assert!(!p.network);
    assert!(p.volumes.is_empty());
    assert!(p.packages.is_empty());
    assert_eq!(p.runtime, None);
    assert!(
        p.workspace_subdirs.is_empty(),
        "workspace_subdirs 空 = 不挂 ws (M5)"
    );
}

#[test]
fn sandbox_profile_workspace_subdirs_round_trip() {
    // R6a：workspace_subdirs 缺省为空（向后兼容旧 DagSpec），有值可序列化回环
    let json = r#"{"volumes":[],"runtime":null,"packages":[],"network":false}"#;
    let p: SandboxProfile = serde_json::from_str(json).unwrap();
    assert!(p.workspace_subdirs.is_empty());

    let p2 = SandboxProfile {
        workspace_subdirs: vec!["src".into(), "tests".into()],
        ..SandboxProfile::default()
    };
    let back: SandboxProfile = serde_json::from_str(&serde_json::to_string(&p2).unwrap()).unwrap();
    assert_eq!(back, p2);
    assert_eq!(back.workspace_subdirs, vec!["src", "tests"]);
}

#[test]
fn dagspec_with_workspace_subdirs_parses() {
    // R6a：DagSpec 的节点 sandbox 带 workspace_subdirs 可解析（限界上下文 §6.3.1 钉死）
    let json = r#"{
        "request_id": "req-1",
        "nodes": [{
            "id": "task-1",
            "summary": "create hello.txt",
            "contract": { "prompt": "p", "acceptance_criteria": "a" },
            "sandbox": {
                "volumes": [],
                "runtime": null,
                "packages": [],
                "network": false,
                "workspace_subdirs": ["src", "tests"]
            }
        }]
    }"#;
    let dag: alfred_core::DagSpec = serde_json::from_str(json).unwrap();
    assert_eq!(dag.nodes[0].sandbox.workspace_subdirs, vec!["src", "tests"]);
    // 序列化回环（deny_unknown_fields 下结构仍稳定）
    let back: alfred_core::DagSpec =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(back, dag);
}

#[test]
fn verdict_invariant_c_requires_no_failure_class() {
    // C 带 failure_class → 拒绝
    assert!(ExecVerdict::new(
        VerdictGrade::C,
        Some(FailureClass::Mechanical),
        Confidence::High,
        vec![],
        "x"
    )
    .is_err());

    // I 缺 failure_class → 拒绝
    assert!(ExecVerdict::new(
        VerdictGrade::I,
        None,
        Confidence::High,
        vec![],
        "x"
    )
    .is_err());

    // 合法组合
    let ok = ExecVerdict::new(
        VerdictGrade::C,
        None,
        Confidence::High,
        vec!["evidence".into()],
        "pass",
    )
    .unwrap();
    assert_eq!(ok.value, VerdictGrade::C);
    assert_eq!(ok.failure_class, None);

    let fail = ExecVerdict::new(
        VerdictGrade::I,
        Some(FailureClass::Mechanical),
        Confidence::Medium,
        vec![],
        "env broke",
    )
    .unwrap();
    assert_eq!(fail.failure_class, Some(FailureClass::Mechanical));

    // 序列化枚举命名（R2 scorer 产出 JSON 的对照基准）
    let json = serde_json::to_string(&fail).unwrap();
    assert!(json.contains("\"value\":\"I\""));
    assert!(json.contains("\"failure_class\":\"mechanical\""));
    assert!(json.contains("\"confidence\":\"medium\""));
}

#[test]
fn artifact_round_trip() {
    let art = Artifact {
        task_id: "task-1".into(),
        changes: vec![FileChange {
            path: "hello.txt".into(),
            kind: ChangeKind::Created,
            before: None,
            after: Some(FileEntry {
                path: "hello.txt".into(),
                size: 5,
                sha256: "abc".into(),
            }),
        }],
        files: vec![FileEntry {
            path: "hello.txt".into(),
            size: 5,
            sha256: "abc".into(),
        }],
    };
    let json = serde_json::to_string(&art).unwrap();
    let back: Artifact = serde_json::from_str(&json).unwrap();
    assert_eq!(art, back);
}

#[test]
fn plan_node_time_limit_secs_optional_round_trip() {
    // A：time_limit_secs 可选声明——有值用值、无值 None 兼容旧契约、None 不落序列化。
    let with = r#"{
        "id": "task-1",
        "summary": "s",
        "contract": { "prompt": "p", "acceptance_criteria": "a" },
        "time_limit_secs": 1800
    }"#;
    let node: alfred_core::PlanNode = serde_json::from_str(with).unwrap();
    assert_eq!(node.time_limit_secs, Some(1800));

    let without = r#"{
        "id": "task-1",
        "summary": "s",
        "contract": { "prompt": "p", "acceptance_criteria": "a" }
    }"#;
    let node: alfred_core::PlanNode = serde_json::from_str(without).unwrap();
    assert_eq!(node.time_limit_secs, None);
    // None 不落序列化（旧 dagspec.json 断言面不变）。
    let text = serde_json::to_string(&node).unwrap();
    assert!(!text.contains("time_limit_secs"));
    let back: alfred_core::PlanNode = serde_json::from_str(&text).unwrap();
    assert_eq!(back, node);
}

#[test]
fn builder_add_node_time_limit_secs_threads_to_plan_node() {
    // A：建图指令 add_node 可选 time_limit_secs → PlanNode 透传（instructions.json
    // 的 dagspec 构造通道）。
    use alfred_core::{BuildInstruction, GraphBuilder};

    let insts = r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"task-1","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"},
         "sandbox":{"workspace_subdirs":["src"]},"time_limit_secs":1800},
        {"op":"commit"}
    ]"#;
    let insts: Vec<BuildInstruction> = serde_json::from_str(insts).unwrap();
    let mut builder = GraphBuilder::new();
    for inst in insts {
        builder.apply(inst).unwrap();
    }
    let dag = builder.build().unwrap();
    assert_eq!(dag.nodes[0].time_limit_secs, Some(1800));

    // 未声明 → None（兼容）。
    let insts = r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"task-1","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"},
         "sandbox":{"workspace_subdirs":["src"]}},
        {"op":"commit"}
    ]"#;
    let insts: Vec<BuildInstruction> = serde_json::from_str(insts).unwrap();
    let mut builder = GraphBuilder::new();
    for inst in insts {
        builder.apply(inst).unwrap();
    }
    let dag = builder.build().unwrap();
    assert_eq!(dag.nodes[0].time_limit_secs, None);
}

#[test]
fn plan_node_resolved_time_limit_prefers_declaration() {
    // A 透传真源：契约声明优先，未声明回退治理缺省（有值用值/无值 fallback 600）。
    let contract = Contract {
        prompt: "p".into(),
        acceptance_criteria: "a".into(),
        reviewer_models: vec![],
    };
    let declared = alfred_core::PlanNode::new("task-1", "s", contract.clone());
    let mut declared = declared;
    declared.time_limit_secs = Some(1800);
    assert_eq!(declared.resolved_time_limit_secs(600), 1800);

    let undeclared = alfred_core::PlanNode::new("task-1", "s", contract);
    assert_eq!(undeclared.resolved_time_limit_secs(600), 600);
}

// ---------- M1 多节点：DagSpec edges + 拓扑排序（数据层） ----------

fn dag_node(id: &str) -> alfred_core::PlanNode {
    alfred_core::PlanNode::new(
        id,
        "s",
        Contract {
            prompt: "p".into(),
            acceptance_criteria: "a".into(),
            reviewer_models: vec![],
        },
    )
}

fn dag_edge(from: &str, to: &str) -> alfred_core::Edge {
    alfred_core::Edge {
        from: from.into(),
        to: to.into(),
    }
}

#[test]
fn dagspec_edges_round_trip_and_omitted_when_empty() {
    // M1：edges 有值往返序列化；空 vec 不落字段（与 time_limit_secs 的
    // None 不落同一范式——旧契约断言面逐字节不变）。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a"), dag_node("b")],
        edges: vec![dag_edge("a", "b")],
    };
    let text = serde_json::to_string(&dag).unwrap();
    assert!(
        text.contains(r#""edges":[{"from":"a","to":"b"}]"#),
        "{text}"
    );
    let back: alfred_core::DagSpec = serde_json::from_str(&text).unwrap();
    assert_eq!(back, dag);

    // 空 edges（DagSpec::new 通道）：序列化不落 edges 字段。
    let empty = alfred_core::DagSpec::new("req-1", vec![dag_node("a")]);
    let text = serde_json::to_string(&empty).unwrap();
    assert!(!text.contains("edges"), "{text}");
    let back: alfred_core::DagSpec = serde_json::from_str(&text).unwrap();
    assert_eq!(back, empty);
    assert!(back.edges.is_empty());
}

#[test]
fn dagspec_old_json_without_edges_still_parses() {
    // 旧契约兼容（红线）：M1 之前的 dagspec.json（无 edges 字段）原样可读，
    // edges 反序列化为空；再序列化也不落 edges（读旧写旧逐字节兼容）。
    let json = r#"{
        "request_id": "req-1",
        "nodes": [{
            "id": "task-1",
            "summary": "s",
            "contract": {"prompt": "p", "acceptance_criteria": "a", "reviewer_models": []},
            "sandbox": {"volumes": [], "runtime": null, "packages": [], "network": false}
        }]
    }"#;
    let dag: alfred_core::DagSpec = serde_json::from_str(json).unwrap();
    assert_eq!(dag.nodes.len(), 1);
    assert!(dag.edges.is_empty());
    let text = serde_json::to_string(&dag).unwrap();
    assert!(!text.contains("edges"), "{text}");
}

#[test]
fn dagspec_topological_order_diamond() {
    // 菱形依赖 a→b、a→c、b→d、c→d：a 最先、d 最后；b/c 同时就绪取声明序
    // 最前（稳定拓扑序）→ [a, b, c, d]。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a"), dag_node("b"), dag_node("c"), dag_node("d")],
        edges: vec![
            dag_edge("a", "b"),
            dag_edge("a", "c"),
            dag_edge("b", "d"),
            dag_edge("c", "d"),
        ],
    };
    assert_eq!(dag.topological_order().unwrap(), vec!["a", "b", "c", "d"]);
}

#[test]
fn dagspec_topological_order_isolated_node_stays_in_order() {
    // 孤立节点（无入边无出边）合法，照常出现在序里。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a"), dag_node("b"), dag_node("island")],
        edges: vec![dag_edge("a", "b")],
    };
    assert_eq!(dag.topological_order().unwrap(), vec!["a", "b", "island"]);
}

#[test]
fn dagspec_topological_order_rejects_cycle_with_path() {
    // 环拒绝：a→b→c→a；Err 显式报出环路径（前驱回溯提取、反转成边方向）。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a"), dag_node("b"), dag_node("c")],
        edges: vec![dag_edge("a", "b"), dag_edge("b", "c"), dag_edge("c", "a")],
    };
    let err = dag.topological_order().unwrap_err();
    assert!(err.contains("cycle"), "err = {err}");
    assert!(err.contains("b -> c -> a -> b"), "err = {err}");

    // 自环也是环：a→a。
    let self_loop = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a")],
        edges: vec![dag_edge("a", "a")],
    };
    let err = self_loop.topological_order().unwrap_err();
    assert!(err.contains("cycle"), "err = {err}");
    assert!(err.contains("a -> a"), "err = {err}");
}

#[test]
fn dagspec_topological_order_rejects_dangling_and_duplicate_edges() {
    // 悬空边：to 端引用不存在节点。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a")],
        edges: vec![dag_edge("a", "ghost")],
    };
    let err = dag.topological_order().unwrap_err();
    assert!(err.contains("unknown node 'ghost'"), "err = {err}");

    // 悬空边：from 端引用不存在节点。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a")],
        edges: vec![dag_edge("ghost", "a")],
    };
    let err = dag.topological_order().unwrap_err();
    assert!(err.contains("unknown node 'ghost'"), "err = {err}");

    // 重复边：同一 (from, to) 两次。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("a"), dag_node("b")],
        edges: vec![dag_edge("a", "b"), dag_edge("a", "b")],
    };
    let err = dag.topological_order().unwrap_err();
    assert!(err.contains("duplicate edge 'a -> b'"), "err = {err}");
}

#[test]
fn builder_add_edge_rejects_unknown_duplicate_and_cyclic() {
    // M1：add_edge 指令校验——两端节点必须已存在、不许重复、不得成环
    //（错误指到具体指令）。
    use alfred_core::{BuildInstruction, GraphBuilder};

    fn apply_all(insts: &str) -> Result<(), String> {
        let insts: Vec<BuildInstruction> = serde_json::from_str(insts).unwrap();
        let mut builder = GraphBuilder::new();
        for inst in insts {
            builder.apply(inst)?;
        }
        Ok(())
    }

    // from 端节点不存在。
    let err = apply_all(
        r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"a","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_edge","from":"ghost","to":"a"}
    ]"#,
    )
    .unwrap_err();
    assert!(err.contains("unknown node 'ghost'"), "err = {err}");

    // to 端节点不存在。
    let err = apply_all(
        r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"a","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_edge","from":"a","to":"ghost"}
    ]"#,
    )
    .unwrap_err();
    assert!(err.contains("unknown node 'ghost'"), "err = {err}");

    // 重复边：a→b 两次。
    let err = apply_all(
        r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"a","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_node","id":"b","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_edge","from":"a","to":"b"},
        {"op":"add_edge","from":"a","to":"b"}
    ]"#,
    )
    .unwrap_err();
    assert!(err.contains("duplicate edge 'a -> b'"), "err = {err}");

    // 成环：a→b 再 b→a。
    let err = apply_all(
        r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"a","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_node","id":"b","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_edge","from":"a","to":"b"},
        {"op":"add_edge","from":"b","to":"a"}
    ]"#,
    )
    .unwrap_err();
    assert!(err.contains("would create a cycle"), "err = {err}");
}

#[test]
fn builder_build_carries_edges_and_orders_nodes() {
    // M1：build() 把边原样落进 DagSpec.edges，节点按依赖拓扑序重排
    //（finalize 校验点 = DagSpec::topological_order 单一真源）。
    use alfred_core::{BuildInstruction, GraphBuilder};

    let insts = r#"[
        {"op":"begin","request_id":"req-1"},
        {"op":"add_node","id":"d","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_node","id":"c","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_node","id":"b","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_node","id":"a","summary":"s",
         "contract":{"prompt":"p","acceptance_criteria":"a"}},
        {"op":"add_edge","from":"a","to":"b"},
        {"op":"add_edge","from":"a","to":"c"},
        {"op":"add_edge","from":"b","to":"d"},
        {"op":"add_edge","from":"c","to":"d"},
        {"op":"commit"}
    ]"#;
    let insts: Vec<BuildInstruction> = serde_json::from_str(insts).unwrap();
    let mut builder = GraphBuilder::new();
    for inst in insts {
        builder.apply(inst).unwrap();
    }
    let dag = builder.build().unwrap();
    // 节点重排成依赖序：a 最先、d 最后；b/c 同级取声明序（c 声明在 b 前）。
    let ids: Vec<&str> = dag.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "c", "b", "d"]);
    // 边原样携带（落盘契约）。
    assert_eq!(
        dag.edges,
        vec![
            dag_edge("a", "b"),
            dag_edge("a", "c"),
            dag_edge("b", "d"),
            dag_edge("c", "d"),
        ]
    );
    // 序列化往返。
    let back: alfred_core::DagSpec =
        serde_json::from_str(&serde_json::to_string(&dag).unwrap()).unwrap();
    assert_eq!(back, dag);
}

// ---------- M3 多节点：执行推进 + 治理环节点状态（数据层/状态机契约） ----------

#[test]
fn dagspec_topological_order_rejects_duplicate_node_ids() {
    // M3：同 id 节点多于一个——id 键控的执行推进/完成记账（next_pending_node /
    // GovernanceRun.completed_nodes）无法区分，结构坏图显式拒绝（builder
    // add_node 已拒，此处兜底离线注入路径）。
    let dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("dup"), dag_node("dup")],
        edges: vec![],
    };
    let err = dag.topological_order().unwrap_err();
    assert!(err.contains("duplicate node id 'dup'"), "err = {err}");
}

#[test]
fn dagspec_next_pending_node_selects_first_uncompleted_in_dependency_order() {
    // M3：依赖序首个未完成节点——b 声明在前但依赖 a → 先取 a；completed
    // 推进后取 b；全图完成 → None（全图完成门）。环图随 M1 校验显式 Err。
    let mut dag = alfred_core::DagSpec {
        request_id: "req-1".into(),
        nodes: vec![dag_node("b"), dag_node("a")],
        edges: vec![dag_edge("a", "b")],
    };
    assert_eq!(dag.next_pending_node(&[]).unwrap().unwrap().id, "a");
    assert_eq!(
        dag.next_pending_node(&["a".to_string()])
            .unwrap()
            .unwrap()
            .id,
        "b"
    );
    assert!(dag
        .next_pending_node(&["a".to_string(), "b".to_string()])
        .unwrap()
        .is_none());

    // 环图：next_pending_node 直接透传 M1 topological_order 的 Err。
    dag.edges = vec![dag_edge("a", "b"), dag_edge("b", "a")];
    assert!(dag.next_pending_node(&[]).unwrap_err().contains("cycle"));
}

#[test]
fn governance_executing_node_completed_self_loop() {
    // M3：节点完成但全图未竟 → Executing 自环（ExecutionFailedRetry 同款
    // 范式）；其他状态收到该事件 = 非法转移显式报错。
    use alfred_core::{GovernanceEvent, GovernanceState};

    let mut run = sample_governance_run();
    run.apply(GovernanceEvent::PlanProduced).unwrap();
    run.apply(GovernanceEvent::PlanReviewPassed).unwrap();
    assert_eq!(run.state(), GovernanceState::Executing);
    run.apply(GovernanceEvent::ExecutionNodeCompleted).unwrap();
    assert_eq!(run.state(), GovernanceState::Executing);
    run.apply(GovernanceEvent::ExecutionNodeCompleted).unwrap();
    assert_eq!(run.state(), GovernanceState::Executing);
    // 全图完成 → ExecutionSucceeded → ExecReviewing（C 转移语义 = 全图完成）。
    run.apply(GovernanceEvent::ExecutionSucceeded).unwrap();
    assert_eq!(run.state(), GovernanceState::ExecReviewing);

    // 非法：PlanReviewing 态收到节点完成事件。
    let mut run = sample_governance_run();
    run.apply(GovernanceEvent::PlanProduced).unwrap();
    assert!(run.apply(GovernanceEvent::ExecutionNodeCompleted).is_err());
}

#[test]
fn governance_plan_produced_clears_completed_nodes() {
    // M3：completed_nodes 生命周期 = 当前计划执行周期——PlanProduced 清零
    // （replan 复用节点 id 时不误标已完成）。
    use alfred_core::{GovernanceEvent, GovernanceState};
    let mut run = sample_governance_run();
    run.apply(GovernanceEvent::PlanProduced).unwrap();
    run.apply(GovernanceEvent::PlanReviewPassed).unwrap();
    run.completed_nodes = vec!["a".into(), "b".into()];
    // 重规划（合法路径）：执行升级 → Escalated → OwnerRevise → Planning →
    // PlanProduced。
    run.apply(GovernanceEvent::ExecutionFailedEscalate).unwrap();
    assert_eq!(run.state(), GovernanceState::Escalated);
    run.apply(GovernanceEvent::OwnerRevise).unwrap();
    run.apply(GovernanceEvent::PlanProduced).unwrap();
    assert!(run.completed_nodes.is_empty());

    // 自环推进不清完成集（断点恢复真源）。
    let mut run = sample_governance_run();
    run.apply(GovernanceEvent::PlanProduced).unwrap();
    run.apply(GovernanceEvent::PlanReviewPassed).unwrap();
    run.completed_nodes = vec!["a".into()];
    run.apply(GovernanceEvent::ExecutionNodeCompleted).unwrap();
    assert_eq!(run.completed_nodes, vec!["a".to_string()]);
}

#[test]
fn governance_run_completed_nodes_state_json_compat() {
    // M3：completed_nodes 落 state.json（断点恢复真源）；旧 state.json（M3
    // 之前无该字段）原样可读（serde default 空 vec）；空 vec 序列化不落字段
    // （与 edges/time_limit_secs 同范式——旧断言面逐字节不变）。
    let mut run = sample_governance_run();
    run.completed_nodes = vec!["a".into(), "b".into()];
    let text = serde_json::to_string(&run).unwrap();
    assert!(text.contains(r#""completed_nodes":["a","b"]"#), "{text}");
    let back: alfred_core::GovernanceRun = serde_json::from_str(&text).unwrap();
    assert_eq!(back.completed_nodes, vec!["a".to_string(), "b".to_string()]);

    // 空 vec：不落字段。
    run.completed_nodes.clear();
    let text = serde_json::to_string(&run).unwrap();
    assert!(!text.contains("completed_nodes"), "{text}");

    // 旧 state.json（无 completed_nodes 字段）→ 空 vec。
    let mut v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&back).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("completed_nodes");
    let legacy: alfred_core::GovernanceRun = serde_json::from_value(v).unwrap();
    assert!(legacy.completed_nodes.is_empty());
}

/// 治理环 run 实体测试夹具（状态机从 Planning 起步）。
fn sample_governance_run() -> alfred_core::GovernanceRun {
    alfred_core::GovernanceRun::new("run-m3", sample_request(), Default::default())
}
