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
