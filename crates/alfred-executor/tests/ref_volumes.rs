//! alfred-executor 集成测试：executor ref_volumes 宿主材料进路（9/3 方案②，
//! 原始用例欠账——契约要求读宿主设计文档时 volumes=[] 导致执行者全 ws 无材料）。
//!
//! 可观测行为（黑盒，经公共 API）：
//!   1. `validate_executor_sandbox` 经 `execute_run` 入口语义——这里直接对
//!      `compose_gen::validate_ref_volume`（放行条件单一真源）断言：
//!      mode 非 ro 拒 / host 相对拒 / host 不存在拒 / container 非法拒 /
//!      保留挂载点冲突拒；合法 ro 卷放行。
//!   2. `generate_executor_compose`：合法参考卷 → `host:container:ro` 挂载行；
//!      非法卷 → Err（compose 生成不静默跳过）。
//!
//! 注意：`validate_executor_sandbox` 是 run.rs 私有函数（经 execute_run 全链
//! 生效，黑盒验收在 tests/e2e/ref_volumes.sh）；单测层钉死放行条件真源本身。

use std::fs;
use std::path::{Path, PathBuf};

use alfred_core::contract::VolumeMount;
use alfred_executor::compose_gen::{generate_executor_compose, validate_ref_volume, ExecutorMounts};

/// compose 生成的 canonicalize 校验要求目录在 HOME 下（E3：colima 只共享 ~），
/// 测试目录不能放 /tmp（agt_wiring.rs 同范式）。
fn temp_dir_under_home(label: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(format!(
        ".alfred-ref-volumes-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 最小工作区（含契约声明的 subdir，非空挂载保证）+ 独立参考材料目录。
fn make_fixture(label: &str) -> (PathBuf, PathBuf) {
    let base = temp_dir_under_home(label);
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("src")).unwrap();
    let refs = base.join("refdocs");
    fs::create_dir_all(&refs).unwrap();
    fs::write(refs.join("design.md"), "# 治理架构\n核心组件……").unwrap();
    (ws, refs)
}

fn vol(host: &Path, container: &str, mode: &str) -> VolumeMount {
    VolumeMount {
        host_path: host.display().to_string(),
        container_path: container.to_string(),
        mode: mode.to_string(),
    }
}


// ---------------------------------------------------------------------------
// 放行条件真源：validate_ref_volume
// ---------------------------------------------------------------------------

#[test]
fn valid_ro_ref_volume_passes() {
    let (_base, refs) = make_fixture("valid");
    let v = vol(&refs, "/references", "ro");
    validate_ref_volume(&v).expect("合法 ro 参考卷必须放行");
    let _ = fs::remove_dir_all(_base);
}

#[test]
fn mode_defaults_to_ro_for_legacy_json() {
    // 旧契约 JSON（无 mode 字段）必须兼容解析且缺省 ro（向后兼容，不破既有 run）。
    let v: VolumeMount =
        serde_json::from_str(r#"{"host_path":"/tmp/x","container_path":"/references"}"#)
            .expect("legacy volume json parses");
    assert_eq!(v.mode, "ro", "缺省 mode 必须是 ro");
}

#[test]
fn non_ro_mode_rejected() {
    let (_base, refs) = make_fixture("rw-mode");
    for mode in ["rw", "RO", "rwo", ""] {
        let err = validate_ref_volume(&vol(&refs, "/references", mode))
            .expect_err("非 ro mode 必须拒绝");
        assert!(
            err.to_string().contains("ro"),
            "拒绝信息应说明只读约束: {err}"
        );
    }
    let _ = fs::remove_dir_all(_base);
}

#[test]
fn relative_or_missing_host_rejected() {
    let (base, refs) = make_fixture("host");
    // 相对路径（E1：docker 静默变 named volume）
    let err = validate_ref_volume(&vol(Path::new("refdocs"), "/references", "ro"))
        .expect_err("相对 host_path 必须拒绝");
    assert!(err.to_string().contains("absolute"), "{err}");
    // 宿主材料不存在（计划缺陷：申请的参考材料没生效）
    let missing = base.join("no-such-dir");
    let err = validate_ref_volume(&vol(&missing, "/references", "ro"))
        .expect_err("host 不存在必须拒绝");
    assert!(err.to_string().contains("does not exist"), "{err}");
    // 文件路径也是合法宿主材料（单文件参考）——存在即可放行
    fs::write(base.join("single.md"), "x").unwrap();
    validate_ref_volume(&vol(&base.join("single.md"), "/references", "ro"))
        .expect("存在的单文件参考卷放行");
    let _ = refs; // refs 生命周期标记
    let _ = fs::remove_dir_all(base);
}

#[test]
fn illegal_container_paths_rejected() {
    let (_base, refs) = make_fixture("container");
    for bad in ["", "references", "/references/../etc", "./x"] {
        let err = validate_ref_volume(&vol(&refs, bad, "ro"))
            .expect_err("非法 container_path 必须拒绝");
        assert!(!err.to_string().is_empty(), "拒绝信息非空: {bad}");
    }
    let _ = fs::remove_dir_all(_base);
}

#[test]
fn reserved_mount_points_rejected() {
    let (_base, refs) = make_fixture("reserved");
    // 工作区 rw 面 / AGT 策略面不可被参考卷遮蔽
    for bad in ["/workspace", "/workspace/src", "/tmp/.agt", "/tmp/.agt/audit"] {
        let err = validate_ref_volume(&vol(&refs, bad, "ro"))
            .expect_err("保留挂载点必须拒绝");
        assert!(
            err.to_string().contains("reserved"),
            "拒绝信息应点名保留挂载点: {err}"
        );
    }
    // 保留路径前缀相似但不冲突的放行（如 /workspace2）
    validate_ref_volume(&vol(&refs, "/workspace2", "ro"))
        .expect("非保留前缀放行");
    let _ = fs::remove_dir_all(_base);
}

// ---------------------------------------------------------------------------
// compose 生成：参考卷 ro 挂载行动态追加
// ---------------------------------------------------------------------------

#[test]
fn compose_appends_ref_volume_ro_line() {
    let (ws, refs) = make_fixture("compose-ro");
    let mounts = ExecutorMounts {
        workspace_subdirs: vec!["src".into()],
        ref_volumes: vec![vol(&refs, "/references", "ro")],
        agt_dir: None,
        agt_audit_dir: None,
    };
    let compose = generate_executor_compose(&ws, "alfred-executor:latest", &mounts)
        .expect("generate compose with ref volume");

    let refs_abs = fs::canonicalize(&refs).unwrap();
    let expected = format!("{}:/references:ro", refs_abs.display());
    assert!(
        compose.contains(&expected),
        "compose 必须含参考卷 ro 挂载行 `{expected}`：{compose}"
    );
    // 工作区挂载不受影响
    assert!(compose.contains(":/workspace:rw"), "{compose}");

    let _ = fs::remove_dir_all(ws.parent().unwrap());
}

#[test]
fn compose_rejects_illegal_ref_volume_not_silent() {
    let (ws, refs) = make_fixture("compose-bad");
    // mode 非 ro → compose 生成失败（不静默跳过挂载）
    let mounts = ExecutorMounts {
        workspace_subdirs: vec!["src".into()],
        ref_volumes: vec![vol(&refs, "/references", "rw")],
        agt_dir: None,
        agt_audit_dir: None,
    };
    assert!(
        generate_executor_compose(&ws, "alfred-executor:latest", &mounts).is_err(),
        "非法参考卷必须让 compose 生成失败（不静默跳过）"
    );
    // host 不存在 → 同样失败
    let mounts = ExecutorMounts {
        workspace_subdirs: vec!["src".into()],
        ref_volumes: vec![vol(&ws.join("missing-refs"), "/references", "ro")],
        agt_dir: None,
        agt_audit_dir: None,
    };
    assert!(
        generate_executor_compose(&ws, "alfred-executor:latest", &mounts).is_err(),
        "host 不存在的参考卷必须让 compose 生成失败"
    );
    let _ = fs::remove_dir_all(ws.parent().unwrap());
}

#[test]
fn compose_without_ref_volumes_unchanged() {
    let (ws, _refs) = make_fixture("compose-none");
    let mounts = ExecutorMounts {
        workspace_subdirs: vec!["src".into()],
        ref_volumes: vec![],
        agt_dir: None,
        agt_audit_dir: None,
    };
    let compose = generate_executor_compose(&ws, "alfred-executor:latest", &mounts)
        .expect("generate compose without ref volumes");
    assert!(
        !compose.contains(":ro"),
        "无参考卷时 compose 不得出现 ro 行（AGT 也未接）：{compose}"
    );
    let _ = fs::remove_dir_all(ws.parent().unwrap());
}

#[test]
fn driver_py_injects_ref_volume_env() {
    // driver.py 注入 AGT_REF_VOLUMES（容器内 pi env → AGT 扩展豁免面数据源）。
    use alfred_executor::task_gen::{generate_task_py, TaskGenParams};
    let py = generate_task_py(&TaskGenParams {
        compose_file: "/tmp/executor.compose.yaml".into(),
        contract_prompt: "read /references and write summary".into(),
        workspace_subdirs: vec!["src".into()],
        port: 13100,
        pi_model: "inspect-bridge/inspect".into(),
        bridge_model: "inspect/mockllm/model".into(),
        max_tokens: 8192,
        workspace_dir: "/workspace".into(),
        sandbox_user: "root".into(),
        agt_ext: "/tmp/.agt/agt-policy.ts".into(),
        agt_policy_path: "/tmp/.agt/policy.json".into(),
        agt_audit_path: "/tmp/.agt/audit/audit.jsonl".into(),
        ref_volume_dirs: vec!["/references".into()],
        run_id: "run-1".into(),
        settle_grace_seconds: 20.0,
        time_limit_secs: 600,
        done_marker: "/tmp/driver.done.json".into(),
        task_name: "alfred-executor".into(),
    })
    .expect("generate driver.py with ref volumes");

    assert!(
        py.contains("REF_VOLUMES = \"/references\""),
        "driver.py 必须注入 REF_VOLUMES 配置：{}",
        &py[..py.len().min(4000)]
    );
    assert!(
        py.contains("env[\"AGT_REF_VOLUMES\"] = REF_VOLUMES"),
        "driver.py 必须把 REF_VOLUMES 注入 pi env AGT_REF_VOLUMES"
    );
}
