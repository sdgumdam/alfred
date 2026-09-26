//! alfred-executor 黑盒测试：G2 放大值透传——driver.py 渲染取参单一真源。
//!
//! [`TaskGenParams::time_limit_secs`]（上游 = governance 侧
//! `node.resolved_time_limit_secs`，timed_out 机械重跑的放大值写回
//! run.dagspec 后经 execute_run 传入）必须原样渲染进 driver.py 的
//! `TIME_LIMIT_SECS`——重跑渲染不得回落治理缺省 600。
//!
//! 治理侧写回链（放大 → effects.dagspec → commit_intent → run.dagspec →
//! 渲染输入）由 alfred-cli governance 单测锁定；真跑渲染断言
//! （exec-N/driver.py 渲染放大后的 TIME_LIMIT）由 e2e r3 case2 覆盖。

use alfred_executor::task_gen::{generate_task_py, TaskGenParams};

fn params(time_limit_secs: u32) -> TaskGenParams {
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
        agt_ext: String::new(),
        agt_policy_path: String::new(),
        agt_audit_path: String::new(),
        ref_volume_dirs: vec![],
        run_id: "run-1".into(),
        settle_grace_seconds: 20.0,
        time_limit_secs,
        done_marker: "/tmp/driver.done.json".into(),
        task_name: "alfred-executor".into(),
        sessions_dir_host: "/tmp/run-1/sessions".into(),
        evidence_binding: String::new(),
        sandbox_metadata: std::collections::BTreeMap::new(),
    }
}

#[test]
fn time_limit_secs_renders_verbatim_into_driver() {
    // 放大值域（600 缺省 → ×2 放大 1200 → cap 3600；e2e r3 用 1→2→4 小值域）
    // 逐值渲染，断言 driver.py 的自限时（anyio.fail_after 的时基）与传入参数
    // 一致——参数与渲染之间无第二真源。
    for tl in [1u32, 4, 600, 1200, 3600] {
        let py = generate_task_py(&params(tl)).expect("generate driver.py");
        assert!(
            py.contains(&format!("TIME_LIMIT_SECS = float({tl})")),
            "time_limit_secs={tl} 必须原样渲染（不得回落缺省 600）"
        );
    }
}

#[test]
fn sessions_dir_host_renders_verbatim_into_driver() {
    // 原生 session 宿主保留目录必须原样渲染（driver 据此把 pi RPC 返回的
    // 容器内 sessionFile 精确映射到宿主保留路径；参数与渲染之间无第二真源）。
    let py = generate_task_py(&params(600)).expect("generate driver.py");
    assert!(
        py.contains("SESSIONS_DIR_HOST = \"/tmp/run-1/sessions\""),
        "sessions_dir_host 必须原样渲染进 driver.py"
    );
    assert!(
        py.contains("CONTAINER_SESSIONS_DIR = \"/tmp/.alfred-sessions\""),
        "容器内挂载点常量必须与 compose 挂载一致"
    );
}

#[test]
fn context_window_renders_verbatim_into_driver() {
    // 容量透传回归锁（2026-09-26 用户指令：输入输出用模型声明最大值）：
    // 声明的 contextWindow 原样渲染 CONTEXT_WINDOW（None 原样渲染 None）
    // ——参数与渲染之间无第二真源。
    let mut p = params(600);
    p.context_window = Some(1048576);
    let py = generate_task_py(&p).expect("generate driver.py");
    assert!(
        py.contains("CONTEXT_WINDOW = 1048576"),
        "declared context_window must render verbatim: {}",
        &py[..py.len().min(4000)]
    );
    let py = generate_task_py(&params(600)).expect("generate driver.py");
    assert!(
        py.contains("CONTEXT_WINDOW = None"),
        "undeclared context_window must render None verbatim"
    );
}
