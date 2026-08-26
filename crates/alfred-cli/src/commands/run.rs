//! `alfred run` 子命令（R3 治理环完整化）。
//!
//! request → converse 规划 → 计划审查 → 通过 → 执行 → 执行审查 → 分级路由
//! （mechanical 重跑预算 N=2 / 耗尽升级）→ 挂起态。state.json 存状态机，
//! `alfred decide` 可从挂起态续跑。

use std::path::PathBuf;

use anyhow::{Context, Result};
use alfred_core::governance::{GovernanceOptions, GovernanceRun};
use alfred_core::request::OwnerRequest;
use alfred_executor::config::{load_executor_model, load_planner_model, load_reviewer_model};
use clap::Args;

use super::governance::{
    audit, default_governance_dir, persist_governance_run, run_governance_loop, state_label,
    GovernanceContext,
};

#[derive(Args, Debug)]
pub struct RunArgs {
    /// OwnerRequest JSON 文件路径。
    #[arg(long)]
    pub request: PathBuf,

    /// 运行目录（缺省自动创建于 $ALFRED_STATE_DIR 或 ~/.local/state/alfred/runs）。
    #[arg(long)]
    pub run_dir: Option<PathBuf>,

    /// 执行 eval 单样本时间上限（秒）。
    #[arg(long, default_value_t = 600)]
    pub time_limit: u32,

    /// 计划审查 eval 单样本时间上限（秒）。
    #[arg(long, default_value_t = 300)]
    pub review_time_limit: u32,

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

    let planner = load_planner_model()?;
    let executor = load_executor_model()?;
    let reviewer = load_reviewer_model()?;
    if reviewer.provider == executor.provider {
        eprintln!(
            "[alfred] warn: reviewer 与 executor 同 provider '{}'——异构审查降级（共用同一模型通道）",
            reviewer.provider
        );
    }

    let run_dir = args.run_dir.clone().unwrap_or_else(default_governance_dir);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(&request).context("serialize OwnerRequest")?,
    )
    .context("write request.json")?;
    let run_id = run_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("run")
        .to_string();

    let options = GovernanceOptions {
        image: args.image.clone(),
        exec_time_limit_secs: args.time_limit,
        review_time_limit_secs: args.review_time_limit,
        port_base: 13100,
        settle_grace_seconds: 20.0,
        ctl_enabled: !args.no_ctl,
    };
    let mut run = GovernanceRun::new(run_id, request.clone(), options);
    let ctx = GovernanceContext {
        run_dir: run_dir.clone(),
        planner_model: planner.clone(),
        executor_model: executor.clone(),
        reviewer_model: reviewer.clone(),
    };

    println!(
        "[alfred] 治理环启动（planner={} executor={} reviewer={}）",
        planner.inspect_model_id(),
        executor.inspect_model_id(),
        reviewer.inspect_model_id()
    );
    println!("[alfred] run_dir  : {}", run_dir.display());
    audit(&run_dir, "governance_started", &serde_json::json!({ "request_id": request.id }))?;

    run_governance_loop(&mut run, &ctx)?;
    // P3 崩溃恢复显式化：run_governance_loop 每次状态转移后已 persist state.json
    // （转移已写 audit，persist 廉价）；此处末尾 persist 是安全网，保证挂起/终态
    // 落盘。孤儿容器对账是显式已知限制（见 .plans/R5报告.md 已知边界）。
    persist_governance_run(&run_dir, &run)?;
    audit(
        &run_dir,
        "governance_paused",
        &serde_json::json!({ "state": state_label(run.state()) }),
    )?;

    println!();
    println!("[alfred] 当前状态 : {}（attempts={}/{}）", state_label(run.state()), run.attempts_used, run.mechanical_budget);
    if let Some(v) = run.plan_verdicts.last() {
        println!("[alfred] 最近计划审查 : {} ({})", if v.pass { "PASS" } else { "FAIL" }, truncate(&v.reason, 160));
    }
    if let Some(v) = run.exec_verdicts.last() {
        println!("[alfred] 最近执行审查 : {:?} (failure_class={:?})", v.value, v.failure_class);
        println!("  rationale     : {}", truncate(&v.explanation, 200));
    }
    println!("[alfred] state.json : {}/state.json", run_dir.display());
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
