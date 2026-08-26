//! 治理环驱动（R3 编排 CLI 核心）。
//!
//! `run_governance_loop` 是编排器状态机的驱动循环：从当前状态出发，一路
//! 推进到挂起态（PlanRejected / Escalated）或终态（Completed / Abandoned）。
//! `alfred run`（初始）与 `alfred decide`（续跑）都调用它。
//!
//! 确定性：状态转移全部经 `GovernanceRun.apply()`（alfred-core 状态机），
//! 每次转移落 audit.jsonl + persist state.json（P3 崩溃恢复显式化）；机械失败
//! 重跑预算 N=2（§3.3）；审查本身出错（unscored / eval error）→ 升级属主
//! （§六继承项，不悄悄放行）。单节点骨架显式拒绝多节点 DAG（P2，不静默截断）。
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::governance::{GovernanceEvent, GovernanceRun};
use alfred_core::util::now_rfc3339;
use alfred_executor::config::ExecutorModel;
use alfred_executor::run::{execute_run, RunOptions};
use alfred_planner::converse::{converse, ConverseOptions};
use alfred_planner::maintain::{maintain, MaintainOptions, MaintainTrigger};
use alfred_reviewer::plan_review::{execute_plan_review, PlanReviewOptions};
use serde_json::Value;

/// 治理环驱动上下文（模型配置 + run 目录）。
#[derive(Debug, Clone)]
pub struct GovernanceContext {
    pub run_dir: PathBuf,
    pub planner_model: ExecutorModel,
    pub executor_model: ExecutorModel,
    pub reviewer_model: ExecutorModel,
}

/// 从挂起/初始状态推进治理环，直到挂起态或终态。
pub fn run_governance_loop(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
    // 执行结果跨态传递（Executing → ExecReviewing）。
    let mut pending_exec: Option<alfred_executor::run::RunOutcome> = None;

    loop {
        audit(
            &ctx.run_dir,
            "state_entered",
            &serde_json::json!({ "state": state_label(run.state()) }),
        )?;
        match run.state() {
            alfred_core::governance::GovernanceState::Planning => {
                // P3 修复：规划侧失败（converse 出错）→ 升级属主（不悄悄放行），
                // 落盘后可恢复（decide retry/revise/abandon 续跑）。
                if let Err(e) = planning_step(run, ctx) {
                    audit(
                        &ctx.run_dir,
                        "planning_error_escalated",
                        &serde_json::json!({ "error": format!("{e:#}") }),
                    )?;
                    run.apply(GovernanceEvent::PlanningError)?;
                    persist_governance_run(&ctx.run_dir, run)?;
                    println!(
                        "[alfred] 规划失败已升级属主（state=Escalated，挂起）。\n\
                         \x20 运行 `alfred decide --run-dir {} --decision retry|revise|abandon` 续跑。",
                        ctx.run_dir.display()
                    );
                    return Ok(());
                }
            }
            alfred_core::governance::GovernanceState::PlanReviewing => {
                plan_review_step(run, ctx)?
            }
            alfred_core::governance::GovernanceState::PlanRejected => {
                println!(
                    "[alfred] 计划被打回（state=PlanRejected，挂起）。\n\
                     \x20 运行 `alfred decide --run-dir {} --decision retry|revise|abandon` 续跑。",
                    ctx.run_dir.display()
                );
                return Ok(());
            }
            alfred_core::governance::GovernanceState::Executing => {
                pending_exec = execution_step(run, ctx)?;
            }
            alfred_core::governance::GovernanceState::ExecReviewing => {
                exec_review_step(run, ctx, &mut pending_exec)?;
            }
            alfred_core::governance::GovernanceState::Completed => {
                println!("[alfred] 全流程完成（Completed）。验收 C 推进到终点。");
                return Ok(());
            }
            alfred_core::governance::GovernanceState::Escalated => {
                println!(
                    "[alfred] 已升级属主（state=Escalated，挂起）。\n\
                     \x20 运行 `alfred decide --run-dir {} --decision retry|revise|abandon` 续跑。",
                    ctx.run_dir.display()
                );
                return Ok(());
            }
            alfred_core::governance::GovernanceState::Abandoned => {
                println!("[alfred] 属主放弃（Abandoned，终态）。");
                return Ok(());
            }
        }
        // P3 修复：每次状态转移后 persist state.json（转移已写 audit，persist 廉价）。
        // 进程在下一转移前崩溃也能从最新状态续跑（崩溃恢复显式化）。
        persist_governance_run(&ctx.run_dir, run)?;
    }
}

/// Planning：converse（会话文档 + 属主消息 → DagSpec）+ E5 reviewer_models 注入。
fn planning_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
    let owner_message = match &run.owner_message {
        Some(m) => m.clone(),
        None => alfred_planner::format_request_message(&run.request),
    };
    let opts = ConverseOptions {
        run_dir: ctx.run_dir.clone(),
        model: ctx.planner_model.clone(),
    };
    let outcome = converse(&opts, &run.request, &run.session_doc, &owner_message)?;
    let mut dagspec = outcome.dagspec;
    // E5：reviewer_models 由系统从 config roles.reviewer 注入（规划器不感知审查者）。
    for node in &mut dagspec.nodes {
        node.contract.reviewer_models = vec![ctx.reviewer_model.model.clone()];
    }
    write_dagspec(&ctx.run_dir, &dagspec)?;
    let node_summaries: Vec<String> = dagspec
        .nodes
        .iter()
        .map(|n| format!("{}:{}", n.id, n.summary))
        .collect();
    audit(
        &ctx.run_dir,
        "planning_done",
        &serde_json::json!({
            "request_id": dagspec.request_id,
            "node_count": dagspec.nodes.len(),
            "nodes": node_summaries,
            "record": outcome.record_path,
        }),
    )?;
    run.dagspec = Some(dagspec);
    run.apply(GovernanceEvent::PlanProduced)?;
    Ok(())
}

/// PlanReviewing：独立 eval 判忠实度 → pass/打回/出错升级。
fn plan_review_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
    let dagspec = run
        .dagspec
        .clone()
        .context("governance state PlanReviewing without dagspec")?;
    let review_dir = ctx.run_dir.join("plan-review");
    let opts = PlanReviewOptions {
        run_dir: review_dir,
        time_limit_secs: run.options.review_time_limit_secs,
        ctl_enabled: run.options.ctl_enabled,
    };
    let outcome = execute_plan_review(
        &opts,
        &ctx.reviewer_model,
        &run.request,
        &dagspec,
        Some(&run.session_doc),
        run.owner_message.as_deref(),
    )?;
    match outcome.verdict {
        Some(v) => {
            run.plan_verdicts.push(v.clone());
            // 维护者 ①：计划审查结论落定后更新会话文档。
            run.session_doc = maintain(
                &MaintainOptions {
                    run_dir: ctx.run_dir.clone(),
                    model: ctx.planner_model.clone(),
                },
                &run.session_doc,
                MaintainTrigger::PlanReviewed {
                    verdict: v.clone(),
                    plan: dagspec,
                },
            )?;
            if v.pass {
                audit(
                    &ctx.run_dir,
                    "plan_review_passed",
                    &serde_json::json!({ "reason": v.reason }),
                )?;
                run.apply(GovernanceEvent::PlanReviewPassed)?;
            } else {
                audit(
                    &ctx.run_dir,
                    "plan_review_rejected",
                    &serde_json::json!({ "reason": v.reason }),
                )?;
                run.apply(GovernanceEvent::PlanReviewRejected)?;
            }
        }
        None => {
            // §六继承项：审查本身出错（unscored/eval error）→ 必须升级，不悄悄放行。
            let reason = outcome
                .unscored_reason
                .or(outcome.error)
                .unwrap_or_else(|| "plan review unscored".to_string());
            audit(
                &ctx.run_dir,
                "plan_review_error_escalated",
                &serde_json::json!({ "reason": reason }),
            )?;
            run.apply(GovernanceEvent::PlanReviewError)?;
        }
    }
    Ok(())
}

/// 返回执行结果（成功时 Some）；失败路径在函数内处理路由（重跑/升级）并返回 None。
fn execution_step(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    ) -> Result<Option<alfred_executor::run::RunOutcome>> {
    let dagspec = run
        .dagspec
        .clone()
        .context("governance state Executing without dagspec")?;
    // P2 修复：单节点骨架显式拒绝多节点 DAG——不静默截断（无静默出口）。
    // 正常流程在计划提交（converse validate_dagspec）即拦截；此处是旧 run 目录
    // 已有历史多节点计划的防御纵深（宁可显式报错，不悄悄只跑第一个节点）。
    if dagspec.nodes.len() != 1 {
        bail!(
            "dagspec has {} nodes; 多节点 DAG 本骨架不支持（单节点验证范围）",
            dagspec.nodes.len()
        );
    }
    let node = dagspec
        .nodes
        .first()
        .context("dagspec has no nodes")?
        .clone();
    let assignment = alfred_core::TaskAssignment {
        task_id: node.id.clone(),
        handler: "run_inspect_eval".to_string(),
        contract: node.contract.clone(),
        sandbox: node.sandbox.clone(),
    };
    run.execution_count += 1;
    let exec_dir = ctx.run_dir.join(format!("exec-{}", run.execution_count));
    let opts = RunOptions {
        run_dir: exec_dir.clone(),
        image: run.options.image.clone(),
        assignment,
        time_limit_secs: run.options.exec_time_limit_secs,
        port_base: run.options.port_base,
        settle_grace_seconds: run.options.settle_grace_seconds,
        ctl_enabled: run.options.ctl_enabled,
    };
    match execute_run(&opts, &ctx.executor_model, Some(&ctx.reviewer_model), &run.request) {
        Ok(outcome) => {
            audit(
                &ctx.run_dir,
                "execution_succeeded",
                &serde_json::json!({
                    "task_id": node.id,
                    "eval_status": outcome.eval_status,
                    "artifact_changes": outcome.artifact.as_ref().map(|a| a.changes.len()),
                }),
            )?;
            run.apply(GovernanceEvent::ExecutionSucceeded)?;
            Ok(Some(outcome))
        }
        Err(e) => {
            // 机械失败判定：eval error / timeout / crash（读 exec 子 run 的 state.json）。
            let mechanical = exec_state_is_mechanical(&exec_dir)?;
            if mechanical {
                if !run.mechanical_exhausted() {
                    run.attempts_used += 1;
                    run.apply(GovernanceEvent::ExecutionFailedRetry)?;
                    audit(
                        &ctx.run_dir,
                        "mechanical_retry",
                        &serde_json::json!({ "attempt": run.attempts_used, "budget": run.mechanical_budget, "error": format!("{e:#}") }),
                    )?;
                    println!(
                        "[alfred] 执行机械失败，按同一契约重跑（{}/{}）：{e}",
                        run.attempts_used, run.mechanical_budget
                    );
                } else {
                    run.apply(GovernanceEvent::ExecutionFailedEscalate)?;
                    audit(
                        &ctx.run_dir,
                        "mechanical_budget_exhausted_escalated",
                        &serde_json::json!({ "error": format!("{e:#}") }),
                    )?;
                }
            } else {
                // 非机械的硬错误（如非默认沙箱档案）→ 升级属主，不悄悄放行。
                audit(
                    &ctx.run_dir,
                    "execution_hard_error_escalated",
                    &serde_json::json!({ "error": format!("{e:#}") }),
                )?;
                run.apply(GovernanceEvent::ExecutionFailedEscalate)?;
            }
            Ok(None)
        }
    }
}

/// ExecReviewing：读执行审查结论（内嵌 scorer 已判 C/I/P）→ §3.3 路由。
fn exec_review_step(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    pending: &mut Option<alfred_executor::run::RunOutcome>,
) -> Result<()> {
    let outcome = pending
        .take()
        .context("governance state ExecReviewing without execution outcome")?;
    match outcome.verdict.clone() {
        Some(v) => {
            run.exec_verdicts.push(v.clone());
            let decision = alfred_core::route(&v).map_err(|e| anyhow::anyhow!(e))?;
            match decision {
                alfred_core::RoutingDecision::Advance => {
                    audit(
                        &ctx.run_dir,
                        "exec_review_passed",
                        &serde_json::json!({ "value": "C", "explanation": v.explanation }),
                    )?;
                    run.apply(GovernanceEvent::ExecReviewPassed)?;
                }
                alfred_core::RoutingDecision::MechanicalRetry => {
                    if !run.mechanical_exhausted() {
                        run.attempts_used += 1;
                        run.apply(GovernanceEvent::ExecReviewMechanicalRetry)?;
                        audit(
                            &ctx.run_dir,
                            "mechanical_retry_from_verdict",
                            &serde_json::json!({ "attempt": run.attempts_used, "budget": run.mechanical_budget }),
                        )?;
                    } else {
                        run.apply(GovernanceEvent::ExecReviewMechanicalEscalate)?;
                        audit(
                            &ctx.run_dir,
                            "mechanical_budget_exhausted_escalated",
                            &serde_json::json!({ "value": format!("{:?}", v.value), "failure_class": format!("{:?}", v.failure_class) }),
                        )?;
                    }
                }
                alfred_core::RoutingDecision::Escalate {
                    suggest_contract_change,
                } => {
                    run.apply(GovernanceEvent::ExecReviewSemanticEscalate)?;
                    audit(
                        &ctx.run_dir,
                        "exec_review_escalated",
                        &serde_json::json!({
                            "value": format!("{:?}", v.value),
                            "failure_class": format!("{:?}", v.failure_class),
                            "suggest_contract_change": suggest_contract_change,
                            "explanation": v.explanation,
                        }),
                    )?;
                    if suggest_contract_change {
                        println!(
                            "[alfred] 执行审查 contract_fault：预标注『建议改契约』，升级属主。"
                        );
                    }
                }
            }
        }
        None => {
            // §六继承项：执行审查本身出错（unscored）→ 升级，不悄悄放行。
            let reason = outcome
                .verdict_unscored_reason
                .or(outcome.error)
                .unwrap_or_else(|| "exec review unscored".to_string());
            audit(
                &ctx.run_dir,
                "exec_review_error_escalated",
                &serde_json::json!({ "reason": reason }),
            )?;
            run.apply(GovernanceEvent::ExecReviewError)?;
        }
    }
    Ok(())
}

/// 读 exec 子 run 的 state.json，判是否为机械失败（eval_status != success）。
fn exec_state_is_mechanical(exec_dir: &Path) -> Result<bool> {
    let path = exec_dir.join("state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        // 无 state.json = execute_run 在写盘前就硬失败（非机械）。
        return Ok(false);
    };
    let v: Value = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", path.display()))?;
    let status = v["run"]["eval_status"].as_str().unwrap_or("success");
    Ok(status != "success")
}

/// 落盘 dagspec.json。
fn write_dagspec(run_dir: &Path, dagspec: &alfred_core::DagSpec) -> Result<()> {
    let text = serde_json::to_string_pretty(dagspec).context("serialize dagspec")?;
    std::fs::write(run_dir.join("dagspec.json"), text).context("write dagspec.json")
}

/// 读治理环 state.json。
pub fn load_governance_run(run_dir: &Path) -> Result<GovernanceRun> {
    let path = run_dir.join("state.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read governance state {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse governance state {}", path.display()))
}

/// 落盘治理环 state.json。
pub fn persist_governance_run(run_dir: &Path, run: &GovernanceRun) -> Result<()> {
    let text = serde_json::to_string_pretty(run).context("serialize governance state")?;
    std::fs::write(run_dir.join("state.json"), text).context("write governance state.json")
}

/// 追加一行审计事件。
pub fn audit(run_dir: &Path, event: &str, data: &Value) -> Result<()> {
    let line = serde_json::json!({
        "ts": now_rfc3339(),
        "event": event,
        "data": data,
    });
    let path = run_dir.join("audit.jsonl");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("append {}", path.display()))?;
    Ok(())
}

/// 状态短标签（audit/打印用）。
pub fn state_label(s: alfred_core::governance::GovernanceState) -> &'static str {
    use alfred_core::governance::GovernanceState::*;
    match s {
        Planning => "planning",
        PlanReviewing => "plan_reviewing",
        PlanRejected => "plan_rejected",
        Executing => "executing",
        ExecReviewing => "exec_reviewing",
        Completed => "completed",
        Escalated => "escalated",
        Abandoned => "abandoned",
    }
}

/// 默认治理 run 目录（`$ALFRED_STATE_DIR` 或 `~/.local/state/alfred/runs`）。
pub fn default_governance_dir() -> PathBuf {
    let base = std::env::var("ALFRED_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            PathBuf::from(home).join(".local/state/alfred/runs")
        });
    base.join(alfred_core::util::short_id("run"))
}
