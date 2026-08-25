//! alfred-core 黑盒测试：实体 schema 与不变量（公共 API）。

use alfred_core::{
    Artifact, ChangeKind, Confidence, Contract, ExecVerdict, FailureClass, FileChange,
    FileEntry, OwnerRequest, SandboxProfile, TaskAssignment, VerdictGrade,
};

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
