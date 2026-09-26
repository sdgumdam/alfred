//! alfred-executor 黑盒测试：executor run 路径 AGT 接线（属主钉死项：权限控制
//! 不让写文件——工具给到，越界写由工具级策略在 tool_call 处拦截；属主拍板
//! **默认启用**，`ALFRED_AGT_DISABLE=1` opt-out）。
//!
//! 可观测行为（run 路径三处接线，照 planner/reviewer 范式）：
//!   1. `prepare_agt_work`（alfred_executor::agt）：Builtin 落内嵌默认资产（
//!      `docker/agt/`）到 `<work>/agt/` + 建 `audit/` 子目录（rw 挂载源）；
//!      Dir 拷显式目录（`ALFRED_AGT_DIR` 覆盖面）；Off → 不挂。
//!   2. `generate_executor_compose`：AGT 目录 Some → `/tmp/.agt:ro` 策略卷 +
//!      `/tmp/.agt/audit:rw` 审计卷（R6a 拆分挂载：可写审计不可改策略）；
//!      None → 无 AGT 卷行。
//!   3. `generate_task_py`：AGT 容器内路径注入 driver.py（模板内 env + `-e`
//!      加载逻辑由 `if AGT_EXT:` 守卫）；空 = 不加载；无残留 token。
//!
//! 容器内真拦截（pi tool_call 被拒 + 审计 JSONL 落宿主）见
//! tests/e2e/agt/exec-demo.sh（真容器 + 真 LLM 黑盒实机演示，不进 cargo test）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alfred_executor::agt::{
    assets, executor_agt_source, prepare_agt_work, resolve_agt_source, AgtSource,
};
use alfred_executor::compose_gen::{generate_executor_compose, ExecutorMounts};
use alfred_executor::task_gen::{generate_task_py, TaskGenParams};

/// 内置默认资产真源文件（拷贝保真断言的对照面）。
fn agt_asset(policy_rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/agt").join(policy_rel)
}

/// 显式目录模式（`ALFRED_AGT_DIR` 覆盖面）的源目录：仓库资产拷贝合成
/// （源目录契约 = agt-policy.ts + policy.json 同目录）。
fn stage_explicit_dir(label: &str) -> PathBuf {
    let dir = temp_dir_under_home(label);
    fs::copy(agt_asset("agt-policy.ts"), dir.join("agt-policy.ts")).unwrap();
    fs::copy(agt_asset("executor/policy.json"), dir.join("policy.json")).unwrap();
    dir
}

/// compose 生成的 canonicalize 校验要求目录在 HOME 下（E3：colima 只共享 ~），
/// 测试目录不能放 /tmp。
fn temp_dir_under_home(label: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(format!(
        ".alfred-agt-wiring-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 准备一份最小工作区（含契约声明的 subdir，非空挂载保证）。
fn make_workspace(label: &str) -> PathBuf {
    let ws = temp_dir_under_home(label).join("ws");
    fs::create_dir_all(ws.join("src")).unwrap();
    ws
}

#[test]
fn prepare_agt_work_builtin_stages_embedded_assets() {
    let work = temp_dir_under_home("prepare");
    let staged = prepare_agt_work(&work, &AgtSource::Builtin, assets::EXECUTOR_POLICY)
        .expect("prepare agt work");
    let staged = staged.expect("Builtin → staged dir");

    assert_eq!(staged, work.join("agt"));
    let copied_ext = fs::read(staged.join("agt-policy.ts")).expect("copied extension");
    let copied_policy = fs::read(staged.join("policy.json")).expect("copied policy");
    assert_eq!(
        copied_ext,
        fs::read(agt_asset("agt-policy.ts")).expect("repo extension asset"),
        "extension 落盘保真（内嵌 == 仓库资产）"
    );
    assert_eq!(
        copied_policy,
        fs::read(agt_asset("executor/policy.json")).expect("repo policy asset"),
        "policy 落盘保真（内嵌 == 仓库资产）"
    );
    assert!(staged.join("audit").is_dir(), "审计子目录必须存在（rw 挂载源）");

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn prepare_agt_work_off_is_noop() {
    let work = temp_dir_under_home("prepare-off");
    let staged = prepare_agt_work(&work, &AgtSource::Off, assets::EXECUTOR_POLICY)
        .expect("Off → Ok(None)");
    assert!(staged.is_none(), "Off → 不挂");
    assert!(!work.join("agt").exists(), "Off 不得创建 agt 目录");

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn compose_mounts_agt_policy_ro_and_audit_rw() {
    let ws = make_workspace("compose");
    let work = ws.parent().unwrap().to_path_buf();
    let src = stage_explicit_dir("compose-dir");
    let staged = prepare_agt_work(&work, &AgtSource::Dir(src), assets::EXECUTOR_POLICY)
        .expect("prepare agt work")
        .expect("staged dir");

    let mounts = ExecutorMounts {
        workspace_subdirs: vec!["src".into()],
        ref_volumes: vec![],
        agt_dir: Some(staged.clone()),
        agt_audit_dir: Some(staged.join("audit")),
        sessions_dir: None,
    };
    let compose = generate_executor_compose(&ws, "alfred-executor:latest", &mounts)
        .expect("generate compose with AGT");

    assert!(
        compose.contains(":/tmp/.agt:ro"),
        "策略目录必须 ro 挂载（agent 不可改策略）：{compose}"
    );
    assert!(
        compose.contains(":/tmp/.agt/audit:rw"),
        "审计子目录必须 rw 挂载（审计 JSONL 落宿主）：{compose}"
    );

    let _ = fs::remove_dir_all(&work);
}

#[test]
fn compose_without_agt_has_no_agt_mounts() {
    let ws = make_workspace("compose-none");
    let mounts = ExecutorMounts {
        workspace_subdirs: vec!["src".into()],
        ref_volumes: vec![],
        agt_dir: None,
        agt_audit_dir: None,
        sessions_dir: None,
    };
    let compose = generate_executor_compose(&ws, "alfred-executor:latest", &mounts)
        .expect("generate compose without AGT");

    assert!(
        !compose.contains("/tmp/.agt"),
        "AGT 未接入时 compose 不得出现 AGT 卷行：{compose}"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// TaskGenParams 最小构造（AGT 三字段按用例填）。
fn params(agt: (&str, &str, &str)) -> TaskGenParams {
    TaskGenParams {
        compose_file: "/tmp/executor.compose.yaml".into(),
        contract_prompt: "do the thing".into(),
        workspace_subdirs: vec!["src".into()],
        port: 13100,
        pi_model: "inspect-bridge/inspect".into(),
        bridge_model: "inspect/mockllm/model".into(),
        max_tokens: 8192,
        context_window: None,
        workspace_dir: "/workspace".into(),
        sandbox_user: "root".into(),
        agt_ext: agt.0.into(),
        agt_policy_path: agt.1.into(),
        agt_audit_path: agt.2.into(),
        ref_volume_dirs: vec![],
        run_id: "run-1".into(),
        settle_grace_seconds: 20.0,
        time_limit_secs: 600,
        done_marker: "/tmp/driver.done.json".into(),
        task_name: "alfred-executor".into(),
        sessions_dir_host: "/tmp/run-1/sessions".into(),
        evidence_binding: String::new(),
        sandbox_metadata: std::collections::BTreeMap::new(),
    }
}

#[test]
fn task_gen_injects_agt_paths_and_loads_extension() {
    let py = generate_task_py(&params((
        "/tmp/.agt/agt-policy.ts",
        "/tmp/.agt/policy.json",
        "/tmp/.agt/audit/audit.jsonl",
    )))
    .expect("generate driver.py with AGT");

    assert!(
        py.contains(r#"AGT_EXT = "/tmp/.agt/agt-policy.ts""#),
        "AGT 扩展路径必须注入 driver.py"
    );
    assert!(
        py.contains(r#"AGT_POLICY_PATH = "/tmp/.agt/policy.json""#),
        "AGT 策略路径必须注入 driver.py"
    );
    assert!(
        py.contains(r#"AGT_AUDIT_PATH = "/tmp/.agt/audit/audit.jsonl""#),
        "AGT 审计路径必须注入 driver.py"
    );
    // 模板加载逻辑：env 注入 + pi -e 扩展（照 planner/reviewer 范式）。
    assert!(py.contains("if AGT_EXT:"), "扩展加载由 if AGT_EXT: 守卫");
    assert!(py.contains(r#"cmd += ["-e", AGT_EXT]"#), "pi 必须经 -e 加载扩展");
    assert!(py.contains(r#"env["AGT_POLICY_PATH"] = AGT_POLICY_PATH"#));
    assert!(!py.contains("__AGT_"), "不得残留 AGT token");
}

#[test]
fn task_gen_empty_agt_disables_extension() {
    let py = generate_task_py(&params(("", "", ""))).expect("generate driver.py without AGT");

    assert!(py.contains(r#"AGT_EXT = """#), "空串 = 不加载扩展");
    assert!(py.contains(r#"AGT_POLICY_PATH = """#));
    assert!(py.contains(r#"AGT_AUDIT_PATH = """#));
    assert!(!py.contains("__AGT_"), "不得残留 AGT token");
}

// ---- env 全局状态测试（串行化防并行竞态，同 python_binary.rs 范式）----

static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvGuard { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn resolve_agt_source_reads_alfred_agt_dir_env_and_disable_wins() {
    let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = EnvGuard::set("ALFRED_AGT_DIR", "/tmp/some-agt-dir");
    assert_eq!(
        resolve_agt_source(),
        AgtSource::Dir(PathBuf::from("/tmp/some-agt-dir")),
        "ALFRED_AGT_DIR 设置时必须解析出显式目录（覆盖面沿用）"
    );
    // opt-out 优先级最高：DISABLE=1 压过显式目录。
    let _d = EnvGuard::set("ALFRED_AGT_DISABLE", "1");
    assert_eq!(
        resolve_agt_source(),
        AgtSource::Off,
        "ALFRED_AGT_DISABLE=1 → Off（压过 ALFRED_AGT_DIR）"
    );
}

#[test]
fn resolve_agt_source_unset_defaults_builtin() {
    let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("ALFRED_AGT_DIR");
    std::env::remove_var("ALFRED_AGT_DISABLE");
    assert_eq!(
        resolve_agt_source(),
        AgtSource::Builtin,
        "未设任何 AGT env → 内置默认策略（属主拍板：默认启用）"
    );
    // 空串等价未设（沿用原语义）。
    let _g = EnvGuard::set("ALFRED_AGT_DIR", "");
    assert_eq!(resolve_agt_source(), AgtSource::Builtin, "空串 ALFRED_AGT_DIR → 内置默认");
}

#[test]
fn executor_agt_source_explicit_dir_scopes_to_executor_only() {
    let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("ALFRED_AGT_DIR");
    std::env::remove_var("ALFRED_AGT_DISABLE");
    // 显式目录（--executor-agt-dir）→ executor 持 Dir；同一进程 env 下
    // planner/reviewer 的共享解析不受影响（Builtin）——A1/A2 消融下
    // reviewer 守卫（requires built-in AGT layer）因此保持成立。
    assert_eq!(
        executor_agt_source(Some(Path::new("/tmp/some-agt-dir"))).unwrap(),
        AgtSource::Dir(PathBuf::from("/tmp/some-agt-dir"))
    );
    assert_eq!(resolve_agt_source(), AgtSource::Builtin);
}

#[test]
fn executor_agt_source_none_falls_back_to_shared_env_semantics() {
    let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _g = EnvGuard::set("ALFRED_AGT_DIR", "/tmp/some-agt-dir");
    assert_eq!(
        executor_agt_source(None).unwrap(),
        AgtSource::Dir(PathBuf::from("/tmp/some-agt-dir")),
        "无旗标 = 共享 env 解析（无旗标旧调用行为不变）"
    );
}

#[test]
fn executor_agt_source_conflicts_with_global_disable_fail_closed() {
    let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _d = EnvGuard::set("ALFRED_AGT_DISABLE", "1");
    // 全局关闭优先：显式目录不得绕过（显式拒绝，非静默 Off/Dir）。
    assert!(executor_agt_source(Some(Path::new("/tmp/some-agt-dir"))).is_err());
}
