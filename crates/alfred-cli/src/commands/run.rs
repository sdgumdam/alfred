//! `alfred run` 子命令（R1 雏形）。
//!
//! 读取 OwnerRequest JSON → 构造单节点 TaskAssignment → alfred-executor
//! 驱动真容器 pi → 采集产物 → 打印摘要。
//!
//! R1 中本命令是规划器的占位替身：真实规划器（R2）会从 OwnerRequest 拆出
//! 多节点 DagSpec；本命令直接把 request.description 当契约 prompt。

use std::path::PathBuf;

use anyhow::{Context, Result};
use alfred_core::assignment::TaskAssignment;
use alfred_core::contract::Contract;
use alfred_core::request::OwnerRequest;
use alfred_executor::config::load_executor_model;
use alfred_executor::run::{default_run_dir, execute_run, RunOptions};
use clap::Args;

#[derive(Args, Debug)]
pub struct RunArgs {
    /// OwnerRequest JSON 文件路径。
    #[arg(long)]
    pub request: PathBuf,

    /// 运行目录（缺省自动创建于 $ALFRED_STATE_DIR 或 ~/.local/state/alfred/runs）。
    #[arg(long)]
    pub run_dir: Option<PathBuf>,

    /// 单样本时间上限（秒）。
    #[arg(long, default_value_t = 600)]
    pub time_limit: u32,

    /// 沙箱镜像。
    #[arg(long, default_value = "alfred-executor:latest")]
    pub image: String,

    /// 关闭 `inspect ctl` 观测轮询。
    #[arg(long)]
    pub no_ctl: bool,
}

pub fn run(args: RunArgs) -> Result<()> {
    let text = std::fs::read_to_string(&args.request)
        .with_context(|| format!("read request {}", args.request.display()))?;
    let request: OwnerRequest = serde_json::from_str(&text)
        .with_context(|| format!("parse OwnerRequest {}", args.request.display()))?;

    // R1 替身规划：request → 单节点契约
    let contract = Contract {
        prompt: request.description.clone(),
        acceptance_criteria: request.acceptance_criteria.clone(),
        reviewer_models: vec![], // E5：R2 起由系统从 config roles.reviewer 注入
    };
    let assignment = TaskAssignment::new(format!("task-{}", request.id), contract);

    let model = load_executor_model()?;
    let run_dir = args.run_dir.clone().unwrap_or_else(default_run_dir);

    let opts = RunOptions {
        run_dir: run_dir.clone(),
        image: args.image.clone(),
        assignment,
        time_limit_secs: args.time_limit,
        ctl_enabled: !args.no_ctl,
        ..RunOptions::default()
    };

    println!("alfred run: task via inspect sandbox -> pi (executor model: {})", model.inspect_model_id());
    println!("  run_dir : {}", run_dir.display());
    println!("  request : {}", args.request.display());
    println!("  prompt  : {}", truncate(&request.description, 120));

    let outcome = execute_run(&opts, &model, &request)?;

    println!();
    println!("eval status      : {}", outcome.eval_status);
    println!("eval location    : {}", outcome.eval_location.as_deref().unwrap_or("(none)"));
    match &outcome.artifact {
        Some(art) => {
            println!("artifact changes : {} ({} files)", art.changes.len(), art.files.len());
            for c in &art.changes {
                println!("  - [{}] {}", change_kind_label(c.kind), c.path);
            }
        }
        None => println!("artifact         : (none)"),
    }
    println!("state.json       : {}/state.json", run_dir.display());
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn change_kind_label(kind: alfred_core::artifact::ChangeKind) -> &'static str {
    use alfred_core::artifact::ChangeKind::*;
    match kind {
        Created => "created",
        Modified => "modified",
        Deleted => "deleted",
    }
}
