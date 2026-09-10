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
