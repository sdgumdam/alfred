//! `alfred decide` 子命令（R3：属主拍板后从挂起态续跑）。
//!
//! §3.2 环节 3/6：计划打回或执行升级时挂起，属主三选一（重跑 retry / 改需求
//! 或改契约重新规划 revise / 放弃 abandon），决定传回编排器执行：
//! - PlanRejected + retry → 伪装消息重规划（P7）；
//! - PlanRejected / Escalated + revise → 属主补充新需求（maintain ②）→ 重规划；
//! - Escalated + retry → 重入执行循环（重跑预算重置）；
//! - 任一 + abandon → 终止。
//!
//! 续跑从 state.json 恢复状态机；模型配置（planner/executor/reviewer）在
//! decide 时重新从 config/env 读取（config 是全局唯一真源，非 run 局部）。

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use alfred_core::governance::{GovernanceState, OwnerDecision};
use alfred_executor::config::{load_executor_model, load_planner_model, load_reviewer_model};
use alfred_planner::disguise::disguise_rejection;
use alfred_planner::maintain::{maintain, MaintainOptions, MaintainTrigger};
use clap::{Args, ValueEnum};

use super::governance::{
    audit, load_governance_run, persist_governance_run, run_governance_loop, state_label,
    GovernanceContext,
};

#[derive(Args, Debug)]
pub struct DecideArgs {
    /// 治理环运行目录（含 state.json）。
    #[arg(long)]
    pub run_dir: PathBuf,

    /// 属主决策。
    #[arg(long, value_enum)]
    pub decision: DecideChoice,

    /// revise 时属主补充的新需求/新消息文件（文本）。
    #[arg(long)]
    pub message: Option<PathBuf>,

    /// 覆盖沙箱镜像（decide retry 时可用）。
    #[arg(long)]
    pub image: Option<String>,

    /// 覆盖执行 eval 时间上限（秒）。
    #[arg(long)]
    pub time_limit: Option<u32>,

    /// 覆盖计划审查时间上限（秒）。
    #[arg(long)]
    pub review_time_limit: Option<u32>,

    /// 覆盖 ctl 轮询开关（配合 --no-ctl）。
    #[arg(long)]
    pub no_ctl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DecideChoice {
    /// 重跑（PlanRejected → 伪装重规划；Escalated → 重入执行）。
    Retry,
    /// 改需求/改契约重新规划（需 --message）。
    Revise,
    /// 放弃（终止）。
    Abandon,
}

impl From<DecideChoice> for OwnerDecision {
    fn from(d: DecideChoice) -> Self {
        match d {
            DecideChoice::Retry => OwnerDecision::Retry,
            DecideChoice::Revise => OwnerDecision::Revise,
            DecideChoice::Abandon => OwnerDecision::Abandon,
        }
    }
}

pub fn decide(args: DecideArgs) -> Result<()> {
    let run_dir = &args.run_dir;
    let mut run = load_governance_run(run_dir)?;
    if let Some(img) = &args.image {
        run.options.image = img.clone();
    }
    if let Some(tl) = args.time_limit {
        run.options.exec_time_limit_secs = tl;
    }
    if let Some(rtl) = args.review_time_limit {
        run.options.review_time_limit_secs = rtl;
    }
    if args.no_ctl {
        run.options.ctl_enabled = false;
    }

    let state = run.state();
    if !state.is_suspended() {
        bail!(
            "decide {} 不适用于当前状态 {:?}（仅挂起态可拍板）",
            choice_label(args.decision),
            state
        );
    }
    let decision: OwnerDecision = args.decision.into();

    // 属主拍板（§3.2 环节 3/6）。
    match (state, decision) {
        (GovernanceState::PlanRejected, OwnerDecision::Retry) => {
            let dagspec = run.dagspec.clone().context("no dagspec in PlanRejected")?;
            let reason = run
                .plan_verdicts
                .last()
                .map(|v| v.reason.clone())
                .unwrap_or_default();
            let disguised = disguise_rejection(&run.request, &dagspec, &reason)
                .map_err(|e| anyhow::anyhow!(e))?;
            run.owner_message = Some(disguised.clone());
            run.apply(alfred_core::governance::GovernanceEvent::OwnerRetry)?;
            audit(
                run_dir,
                "decide_retry_plan",
                &serde_json::json!({ "disguised_message": disguised }),
            )?;
            println!("[alfred] 属主拍板：重跑（计划打回）→ 伪装消息重规划。");
            println!("[alfred] 伪装消息: {}", truncate(&disguised, 200));
        }
        (GovernanceState::PlanRejected, OwnerDecision::Revise)
        | (GovernanceState::Escalated, OwnerDecision::Revise) => {
            let msg_path = args
                .message
                .as_ref()
                .context("revise 需要 --message <文件>（属主补充的新需求/新消息）")?;
            let msg = std::fs::read_to_string(msg_path)
                .with_context(|| format!("read message {}", msg_path.display()))?;
            let msg = msg.trim().to_string();
            if msg.is_empty() {
                bail!("--message 文件为空");
            }
            // 维护者 ②：属主补充新需求后更新会话文档。
            let planner_model = load_planner_model()?;
            run.session_doc = maintain(
                &MaintainOptions {
                    run_dir: run_dir.clone(),
                    model: planner_model.clone(),
                },
                &run.session_doc,
                MaintainTrigger::OwnerMessage { message: msg.clone() },
            )?;
            run.owner_message = Some(msg.clone());
            run.apply(alfred_core::governance::GovernanceEvent::OwnerRevise)?;
            audit(
                run_dir,
                "decide_revise",
                &serde_json::json!({ "message": msg }),
            )?;
            println!("[alfred] 属主拍板：改需求重新规划。");
        }
        (GovernanceState::PlanRejected, OwnerDecision::Abandon)
        | (GovernanceState::Escalated, OwnerDecision::Abandon) => {
            run.apply(alfred_core::governance::GovernanceEvent::OwnerAbandon)?;
            audit(run_dir, "decide_abandon", &serde_json::json!({}))?;
            println!("[alfred] 属主拍板：放弃（终止）。");
        }
        (GovernanceState::Escalated, OwnerDecision::Retry) => {
            // 属主拍板重跑 → 重入执行循环（重跑预算重置为新周期）。
            run.attempts_used = 0;
            run.apply(alfred_core::governance::GovernanceEvent::OwnerRetry)?;
            audit(run_dir, "decide_retry_exec", &serde_json::json!({}))?;
            println!("[alfred] 属主拍板：重跑执行。");
        }
        _ => unreachable!("suspended state matched above"),
    }

    // 续跑（从新状态推进到下一个挂起/终态）。
    let planner_model = load_planner_model()?;
    let executor_model = load_executor_model()?;
    let reviewer_model = load_reviewer_model()?;
    if reviewer_model.provider == executor_model.provider {
        eprintln!(
            "[alfred] warn: reviewer 与 executor 同 provider '{}'——异构审查降级",
            reviewer_model.provider
        );
    }
    let ctx = GovernanceContext {
        run_dir: run_dir.clone(),
        planner_model,
        executor_model,
        reviewer_model,
    };
    persist_governance_run(run_dir, &run)?;
    run_governance_loop(&mut run, &ctx)?;
    persist_governance_run(run_dir, &run)?;
    audit(
        run_dir,
        "governance_paused",
        &serde_json::json!({ "state": state_label(run.state()) }),
    )?;

    println!();
    println!(
        "[alfred] 当前状态 : {}（attempts={}/{}）",
        state_label(run.state()),
        run.attempts_used,
        run.mechanical_budget
    );
    println!("[alfred] state.json : {}/state.json", run_dir.display());
    Ok(())
}

pub(crate) fn choice_label(d: DecideChoice) -> &'static str {
    match d {
        DecideChoice::Retry => "retry",
        DecideChoice::Revise => "revise",
        DecideChoice::Abandon => "abandon",
    }
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
