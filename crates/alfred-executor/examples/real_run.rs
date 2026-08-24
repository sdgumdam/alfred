//! S2 真跑一次 execute_in_container 的驱动：ALFRED_OFFLINE 未置 1 时
//! 走真 Docker（需 alfred-executor:latest 镜像 + LLM key + 宿主机允许
//! 容器出网）；离线置 1 时走 fixture，用于不碰 Docker 的快验。
//!
//! 用法：cargo run -p alfred-executor --example real_run -- <workspace> <run_dir>

use alfred_core::{Contract, TaskAssignment, HANDLER_RUN_INSPECT_EVAL};
use alfred_executor::{SandboxProfile, execute_in_container};
use std::path::PathBuf;

fn main() {
    let workspace = PathBuf::from(std::env::args().nth(1).expect("workspace path"));
    let run_dir = PathBuf::from(std::env::args().nth(2).expect("run dir"));
    let assignment = TaskAssignment {
        task_id: "n1".into(),
        handler: HANDLER_RUN_INSPECT_EVAL.into(),
        contract: Contract {
            prompt: "Create a file named hello.txt in the current directory with \
                     exactly the content: Hello Alfred. Then stop."
                .into(),
            acceptance_criteria: "hello.txt exists with content Hello Alfred".into(),
            reviewer_models: vec!["judge-a".into()],
        },
        params: serde_json::Map::new(),
    };
    // network=true：容器内 pi 需访问 LLM API 才能执行（profile 四维的最小放开）。
    // provider=deepseek 依赖宿主 DEEPSEEK_API_KEY 经 executor 白名单注入。
    let profile: SandboxProfile =
        serde_json::from_str(r#"{"network": true, "provider": "deepseek"}"#)
            .expect("profile literal is valid");
    match execute_in_container(&assignment, &workspace, &profile, &run_dir) {
        Ok(artifact) => {
            println!("=== ARTIFACT ===");
            println!("node_id: {}", artifact.node_id);
            println!("produced_at: {}", artifact.produced_at);
            println!("--- workspace_diff ---");
            println!("{}", artifact.workspace_diff);
        }
        Err(err) => {
            eprintln!("EXECUTE FAILED: {err}");
            std::process::exit(1);
        }
    }
}
