//! `alfred plan-review` 子命令（R2）。
//!
//! 读取 OwnerRequest JSON + DagSpec JSON → 独立 eval（主模型 = 审查者）
//! 判忠实度 → PlanVerdict 落盘 run 目录（verdict.json + state.json）。

use std::path::PathBuf;

use anyhow::{Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_executor::config::{load_executor_model, load_reviewer_model};
use alfred_reviewer::plan_review::{default_review_dir, execute_plan_review, PlanReviewOptions};
use clap::Args;

#[derive(Args, Debug)]
pub struct PlanReviewArgs {
    /// OwnerRequest JSON 文件路径。
    #[arg(long)]
    pub request: PathBuf,

    /// DagSpec JSON 文件路径。
    #[arg(long)]
    pub dagspec: PathBuf,

    /// 运行目录（缺省自动创建于 $ALFRED_STATE_DIR 或 ~/.local/state/alfred/runs）。
    #[arg(long)]
    pub run_dir: Option<PathBuf>,

    /// 单样本时间上限（秒）。
    #[arg(long, default_value_t = 300)]
    pub time_limit: u32,

    /// 关闭 `inspect ctl` 观测轮询。
    #[arg(long)]
    pub no_ctl: bool,
}

pub fn plan_review(args: PlanReviewArgs) -> Result<()> {
    let req_text = std::fs::read_to_string(&args.request)
        .with_context(|| format!("read request {}", args.request.display()))?;
    let request: OwnerRequest = serde_json::from_str(&req_text)
        .with_context(|| format!("parse OwnerRequest {}", args.request.display()))?;

    let dag_text = std::fs::read_to_string(&args.dagspec)
        .with_context(|| format!("read dagspec {}", args.dagspec.display()))?;
    let dagspec: DagSpec = serde_json::from_str(&dag_text)
        .with_context(|| format!("parse DagSpec {}", args.dagspec.display()))?;

    // 计划审查主模型 = 审查者（roles.reviewer）
    let reviewer = load_reviewer_model()?;
    // 异构降级警告：reviewer 与 executor 同 provider 时异构性打折扣
    if let Ok(executor) = load_executor_model() {
        if reviewer.provider == executor.provider {
            eprintln!(
                "[alfred] warn: reviewer 与 executor 同 provider '{}'——异构审查降级",
                reviewer.provider
            );
        }
    }

    let run_dir = args.run_dir.clone().unwrap_or_else(default_review_dir);
    let opts = PlanReviewOptions {
        run_dir: run_dir.clone(),
        time_limit_secs: args.time_limit,
        ctl_enabled: !args.no_ctl,
        // R6c：独立 plan-review 走旧 eval 直判路径（无治理上下文/无容器）
        container: None,
    };

    println!("alfred plan-review: DagSpec vs OwnerRequest 忠实度 (reviewer model: {})", reviewer.inspect_model_id());
    println!("  run_dir : {}", run_dir.display());
    println!("  request : {}", request.id);

    let outcome = execute_plan_review(&opts, &reviewer, &request, &dagspec, None)?;

    println!();
    println!("eval status      : {}", outcome.eval_status);
    match &outcome.verdict {
        Some(v) => {
            println!("plan verdict     : {} ({})", if v.pass { "PASS" } else { "FAIL" }, if v.pass { "忠实" } else { "打回" });
            println!("  reason         : {}", truncate(&v.reason, 300));
        }
        None => println!("plan verdict     : unscored ({})", outcome.unscored_reason.as_deref().unwrap_or("none")),
    }
    println!("verdict.json     : {}/verdict.json", run_dir.display());
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
