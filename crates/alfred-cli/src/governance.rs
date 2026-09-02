//! 治理环驱动（编排器状态机驱动核心）。
//!
//! `run_governance_loop` 是编排器状态机的驱动循环：从当前状态出发，一路
//! 推进到挂起态（PlanRejected / Escalated）或终态（Completed / Abandoned）。
//! 库调用方（codux driver）以非 CLI 形式驱动它——alfred-cli 不再是 owner
//! 交互入口（删 alfred CLI 六命令后，owner 交互走 codux 终端）。
//!
//! 确定性：状态转移全部经 `GovernanceRun.apply()`（alfred-core 状态机），
//! 每次转移落 audit.jsonl + persist state.json（P3 崩溃恢复显式化）；机械失败
//! 重跑预算 N=2（§3.3）；审查本身出错（unscored / driver error）→ 升级属主
//! （§六继承项，不悄悄放行）。单节点骨架显式拒绝多节点 DAG（P2，不静默截断）。
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::conversation::{
    append_to_disk, load_conversation, ConversationRole, ConversationSource,
};
use alfred_core::governance::{GovernanceEvent, GovernanceRun, GovernanceState, OwnerDecision};
use alfred_core::util::now_rfc3339;
use alfred_executor::config::ExecutorModel;
use alfred_executor::run::{execute_run, RunOptions};
use alfred_planner::converse::{converse, ConverseOptions, ConverseOutcome};
use alfred_planner::disguise::disguise_rejection;
use alfred_planner::maintain::{maintain, MaintainOptions, MaintainTrigger};
use alfred_reviewer::exec_review::{execute_exec_review, ExecReviewOptions};
use alfred_reviewer::plan_review::{execute_plan_review, PlanReviewOptions};
use alfred_reviewer::ReviewerContainerOptions;
use serde_json::Value;

/// 治理环驱动上下文（模型配置 + run 目录）。
#[derive(Debug, Clone)]
pub struct GovernanceContext {
    pub run_dir: PathBuf,
    pub planner_model: ExecutorModel,
    pub executor_model: ExecutorModel,
    pub reviewer_model: ExecutorModel,
    /// codux wrapper 注入的项目上下文（`--append-system-prompt`，经
    /// `ALFRED_APPEND_SYSTEM_PROMPT` 读入）；追加到 planner pi 的 converse
    /// system prompt（P2-1：内存注入端到端生效）。空串 = 不注入。
    pub append_system_prompt: String,
}

/// 从挂起/初始状态推进治理环，直到挂起态或终态。
///
/// 返回 `Ok(Some(reply))` = 规划器答复了属主（§2.4 Reply 分支，state=Planning
/// 停驻、对话继续）——reply 文本 surface 给调用方（driver 打印到 stdout，owner
/// 终端直读，P2-2）；`Ok(None)` = 推进到挂起态/终态（无待显示答复）。
pub fn run_governance_loop(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<Option<String>> {
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
                // §2.4 两分支：planning_step 返回 Ok(Some(reply)) = 规划器答复了属主
                // （对话继续，不产计划）——停止编排环，等属主下一轮消息；答复文本
                // 返回调用方（driver 打印，owner 终端直读，P2-2）。
                match planning_step(run, ctx) {
                    Ok(None) => {}
                    Ok(Some(reply)) => return Ok(Some(reply)),
                    Err(e) => {
                        audit(
                            &ctx.run_dir,
                            "planning_error_escalated",
                            &serde_json::json!({ "error": format!("{e:#}") }),
                        )?;
                        run.apply(GovernanceEvent::PlanningError)?;
                        persist_governance_run(&ctx.run_dir, run)?;
                        println!(
                            "[alfred] 规划失败已升级属主（state=Escalated，挂起）。\n\
                             \x20 run_dir: {}；等待属主拍板（retry/revise/abandon）。",
                            ctx.run_dir.display()
                        );
                        return Ok(None);
                    }
                }
            }
            alfred_core::governance::GovernanceState::PlanReviewing => {
                plan_review_step(run, ctx)?
            }
            alfred_core::governance::GovernanceState::PlanRejected => {
                println!(
                    "[alfred] 计划被打回（state=PlanRejected，挂起）。\n\
                     \x20 run_dir: {}；等待属主拍板（retry/revise/abandon）。",
                    ctx.run_dir.display()
                );
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Executing => {
                pending_exec = execution_step(run, ctx)?;
            }
            alfred_core::governance::GovernanceState::ExecReviewing => {
                exec_review_step(run, ctx, &mut pending_exec)?;
            }
            alfred_core::governance::GovernanceState::Completed => {
                println!("[alfred] 全流程完成（Completed）。验收 C 推进到终点。");
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Escalated => {
                println!(
                    "[alfred] 已升级属主（state=Escalated，挂起）。\n\
                     \x20 run_dir: {}；等待属主拍板（retry/revise/abandon）。",
                    ctx.run_dir.display()
                );
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Abandoned => {
                println!("[alfred] 属主放弃（Abandoned，终态）。");
                return Ok(None);
            }
        }
        // P3 修复：每次状态转移后 persist state.json（转移已写 audit，persist 廉价）。
        // 进程在下一转移前崩溃也能从最新状态续跑（崩溃恢复显式化）。
        persist_governance_run(&ctx.run_dir, run)?;
    }
}

/// feed_owner_message 的返回：新状态 + 规划器答复（§2.4 Reply 分支有答复时
/// `reply = Some(text)`，surface 给调用方终端显示；否则 `None`）。
#[derive(Debug, Clone)]
pub struct FeedOutcome {
    pub state: GovernanceState,
    pub reply: Option<String>,
}

/// 喂属主消息 → 续跑治理环（库调用方/codux driver 入口，非 CLI 子命令）。
///
/// 属主第二句：planner 是容器里能多轮对话的 pi，owner 直接跟它说话——本 API 是
/// "库调用方喂属主消息"的形式：设置本轮属主消息、维护者②固化关键结论、落对话
/// 记录、按挂起态路由续跑治理环，返回新状态给调用方（codux 终端显示）。
///
/// 1. **Abandon 前置路由（P2b）**：`decision=Abandon` 不要求消息、不跑 maintain②、
///    不落 owner.message 轮——直接 `apply(OwnerAbandon)` 进终态。属主放弃恒可选：
///    即使 run 已坏（planner 容器故障/不可用）也拦不住属主放弃。
/// 2. 有消息 → `run.owner_message = message`（下一轮 `planning_step` converse 读它）、
///    maintain②（`OwnerMessage` → `key_conclusions`，把属主消息固化为关键结论）、
///    落 conversation.json（`ConversationSource::OwnerMessage`，属主轮次）。
///    **P3a**：`PlanRejected+Retry` 打回信号伪装（属主 08-18「肯定要做润色伪装」）——
///    `disguise_rejection` 把审查者拒绝理由转写为属主口吻消息，伪装即本轮属主消息
///    驱动重规划（不依赖属主另附消息）。
/// 3. 挂起态路由 + 续跑 `run_governance_loop`：
///    - Planning（converse 答复分支停驻）→ 无状态转移，直接续跑（对话继续）；
///    - PlanRejected/Escalated → 按 `OwnerDecision` 路由：Revise → `OwnerRevise`
///      （回 Planning 重新规划，喂 owner_message 续 converse）；Retry →
///      `OwnerRetry`（按升级来源路由：重入执行/重审同一计划/重新规划）；
///      Abandon → `OwnerAbandon`（终态，不续跑）。
/// 4. **P3b**：升级拍板（`decision != Revise`，即 Retry/Abandon）→ 补落
///    `panel.decision` 轮（§二.8：升级拍板时的属主决策；reviewer 挂载对话记录
///    区分拍板与普通对话消息）。
/// 5. 返回 `FeedOutcome { state, reply }` 给调用方——`reply` = planner 答复文本
///    （§2.4 Reply 分支，driver 打印到 stdout，owner 终端直读，P2-2）；无答复则 `None`。
pub fn feed_owner_message(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    message: &str,
    decision: OwnerDecision,
) -> Result<FeedOutcome> {
    let state = run.state();
    // P2a 修复：挂起态可拍板；Planning 态（converse 答复后停驻）可 revise（续入对话）
    // 或 abandon（放弃——属主放弃恒可选，Skeleton §3.2 三选一）。
    let allowed = state.is_suspended()
        || (state == GovernanceState::Planning
            && matches!(
                decision,
                OwnerDecision::Revise | OwnerDecision::Abandon
            ));
    if !allowed {
        bail!(
            "feed_owner_message: 决策 {:?} 不适用于当前状态 {:?}（仅挂起态可拍板；Planning 态仅 revise/abandon 可操作）",
            decision,
            state
        );
    }

    // P2b：Abandon 前置路由——不要求消息、不跑 maintain②、不落 owner.message 轮。
    // decision=Abandon 直接 apply(OwnerAbandon) 进终态，不触碰 planner（run 已坏也
    // 能弃）。Planning 态无 (Planning, OwnerAbandon) 转移会在这里显式报错。
    if decision == OwnerDecision::Abandon {
        // P3b：升级拍板（abandon）→ 补落 panel.decision 轮（§二.8：升级拍板时的
        // 属主决策；reviewer 挂载对话记录可见拍板）。不触碰 planner（P2b）。
        append_to_disk(
            &ctx.run_dir,
            &run.run_id,
            ConversationRole::Owner,
            panel_decision_text(decision),
            ConversationSource::PanelDecision,
        )
        .map_err(anyhow::Error::msg)
        .context("append panel.decision to conversation.json")?;
        audit(
            &ctx.run_dir,
            "feed_owner_message",
            &serde_json::json!({
                "decision": format!("{decision:?}"),
                "from_state": state_label(state),
            }),
        )?;
        run.apply(GovernanceEvent::OwnerAbandon)?;
        persist_governance_run(&ctx.run_dir, run)?;
        audit(
            &ctx.run_dir,
            "governance_paused",
            &serde_json::json!({ "state": state_label(run.state()) }),
        )?;
        return Ok(FeedOutcome {
            state: run.state(),
            reply: None,
        });
    }

    // Retry 消息可选（重跑不必然带新指令）；Revise 需非空消息（喂 planner 新指令）。
    let message = message.trim();
    if message.is_empty() && decision == OwnerDecision::Revise {
        bail!("feed_owner_message: revise 决策需要非空属主消息");
    }

    // P3a：PlanRejected+Retry 打回信号伪装（属主 08-18「肯定要做润色伪装」）——
    // 用 disguise_rejection 把审查者拒绝理由转写为属主口吻消息驱动重规划（旧
    // decide retry 语义：伪装即本轮属主消息，不依赖属主另附消息）。其余决策用
    // 属主原话。
    let driving_message: String = if state == GovernanceState::PlanRejected
        && decision == OwnerDecision::Retry
    {
        let dagspec = run
            .dagspec
            .clone()
            .context("no dagspec in PlanRejected")?;
        let reason = run
            .plan_verdicts
            .last()
            .map(|v| v.reason.clone())
            .unwrap_or_default();
        disguise_rejection(&run.request, &dagspec, &reason).map_err(anyhow::Error::msg)?
    } else {
        message.to_string()
    };

    // 有消息 → maintain② + 设 owner_message + 落对话轮；无消息（Retry）→ 跳过消息轮。
    if !driving_message.is_empty() {
        // 1. maintain②：属主消息固化为关键结论（先 maintain——喂旧 doc，得新 doc）。
        // maintain 失败（如 LLM 输出格式漂移致解析失败）→ 回退旧 session_doc，
        // 不废整条 run；审计记 maintain_warning（trigger=owner_message），治理环
        // 照常推进（与 plan_review_step ① 的回退同构）。
        match maintain(
            &MaintainOptions {
                run_dir: ctx.run_dir.clone(),
                model: ctx.planner_model.clone(),
                container: alfred_planner::container::PlannerContainerOptions::from_governance(
                    ctx.run_dir.clone(),
                    &run.options,
                ),
            },
            &run.session_doc,
            MaintainTrigger::OwnerMessage {
                message: driving_message.clone(),
            },
        ) {
            Ok(updated) => run.session_doc = updated,
            Err(e) => {
                audit(
                    &ctx.run_dir,
                    "maintain_warning",
                    &serde_json::json!({
                        "trigger": "owner_message",
                        "error": format!("{e:#}"),
                        "fallback": "keep_previous_session_doc",
                    }),
                )?;
            }
        }

        // 2. 设置 owner_message（planning_step 下一轮 converse 读它；重规划/改需求语义）。
        run.owner_message = Some(driving_message.clone());

        // 3. 落 conversation.json（属主轮次，reviewer 挂载输入数据源，§二.8）。
        append_to_disk(
            &ctx.run_dir,
            &run.run_id,
            ConversationRole::Owner,
            driving_message.clone(),
            ConversationSource::OwnerMessage,
        )
        .map_err(anyhow::Error::msg)
        .context("append owner.message to conversation.json")?;
    }

    // P3b：升级拍板（decision != Revise → Retry）→ 补落 panel.decision 轮
    // （§二.8：升级拍板时的属主决策；reviewer 挂载对话记录区分拍板与普通消息）。
    if decision != OwnerDecision::Revise {
        append_to_disk(
            &ctx.run_dir,
            &run.run_id,
            ConversationRole::Owner,
            panel_decision_text(decision),
            ConversationSource::PanelDecision,
        )
        .map_err(anyhow::Error::msg)
        .context("append panel.decision to conversation.json")?;
    }

    audit(
        &ctx.run_dir,
        "feed_owner_message",
        &serde_json::json!({
            "decision": format!("{decision:?}"),
            "from_state": state_label(state),
            "message": driving_message,
            "disguised": state == GovernanceState::PlanRejected && decision == OwnerDecision::Retry,
        }),
    )?;

    // 4. 挂起态路由（Planning 态无状态转移——对话继续，直接续跑 planning_step）。
    if state != GovernanceState::Planning {
        match decision {
            OwnerDecision::Revise => run.apply(GovernanceEvent::OwnerRevise)?,
            OwnerDecision::Retry => {
                // P1 修复：Escalated+OwnerRetry 按升级来源路由（GovernanceRun::apply
                // 在升级事件落来源）；attempts 重置——重入执行/重审/重规划都是新周期。
                run.attempts_used = 0;
                run.apply(GovernanceEvent::OwnerRetry)?;
            }
            // P2b：Abandon 已在前面前置处理返回，到不了这里（防御：显式报错不静默）。
            OwnerDecision::Abandon => {
                bail!("feed_owner_message: Abandon 应已前置处理（内部状态不一致）")
            }
        }
    }

    // 5. 续跑（从新状态推进到下一个挂起/终态）。
    persist_governance_run(&ctx.run_dir, run)?;
    let reply = run_governance_loop(run, ctx)?;
    persist_governance_run(&ctx.run_dir, run)?;
    audit(
        &ctx.run_dir,
        "governance_paused",
        &serde_json::json!({ "state": state_label(run.state()) }),
    )?;
    Ok(FeedOutcome {
        state: run.state(),
        reply,
    })
}

/// Planning：converse（会话文档 + 属主消息 → §2.4 两分支）+ E5 reviewer_models 注入。
///
/// 返回 `Ok(None)` = 产出计划（已 apply PlanProduced → PlanReviewing，编排环继续）；
/// `Ok(Some(reply))` = 规划器答复了属主（纯文本答复，不产计划——对话继续，状态仍
/// Planning，编排环返回调用方；答复文本 surface 给调用方（driver 打印，P2-2），
/// 调用方经下一轮属主消息（revise 语义）续入对话（P1-2）。
fn planning_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<Option<String>> {
	let owner_message = match &run.owner_message {
		Some(m) => m.clone(),
		None => alfred_planner::format_request_message(&run.request),
	};
	let opts = ConverseOptions {
		run_dir: ctx.run_dir.clone(),
		model: ctx.planner_model.clone(),
		container: alfred_planner::container::PlannerContainerOptions::from_governance(
			ctx.run_dir.clone(),
			&run.options,
		),
		append_system_prompt: ctx.append_system_prompt.clone(),
	};
	let outcome = converse(&opts, &run.request, &run.session_doc, &owner_message)?;
	match outcome {
		// ---- §2.4 建图指令分支：DagSpec 交编排器接管（计划审查 → 执行） ----
		ConverseOutcome::Instructions { dagspec, record_path } => {
			// R6a：对话记录——converse 落定后 append（reviewer 挂载输入数据源，§二.8）。
			// M4-a：conversation.json 只承载语义轮次——落 planner 的语义回复（计划摘要），
			// 不落原始建图指令 JSON（中间指令属实现细节，已在 llm-calls/ 审计）。
			append_to_disk(
				&ctx.run_dir,
				&run.run_id,
				ConversationRole::Planner,
				format_plan_reply(&dagspec),
				ConversationSource::ConverseReply,
			)
			.map_err(anyhow::Error::msg)
			.context("append converse.reply to conversation.json")?;
			let mut dagspec = dagspec;
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
					"record": record_path,
				}),
			)?;
			run.dagspec = Some(dagspec);
			run.apply(GovernanceEvent::PlanProduced)?;
			Ok(None)
		}
		// ---- §2.4 答复分支：纯文本答复给属主，不强制产 DagSpec（对话继续） ----
		ConverseOutcome::Reply { reply, record_path } => {
			// M4-a：conversation.json 落规划器原话答复（语义轮次），不产计划。
			append_to_disk(
				&ctx.run_dir,
				&run.run_id,
				ConversationRole::Planner,
				reply.clone(),
				ConversationSource::ConverseReply,
			)
			.map_err(anyhow::Error::msg)
			.context("append converse.reply (reply branch) to conversation.json")?;
			audit(
				&ctx.run_dir,
				"converse_reply",
				&serde_json::json!({ "record": record_path }),
			)?;
			Ok(Some(reply))
		}
	}
}

/// PlanReviewing：reviewer 容器判忠实度 → pass/打回/出错升级。
fn plan_review_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
    let dagspec = run
        .dagspec
        .clone()
        .context("governance state PlanReviewing without dagspec")?;
    let review_dir = ctx.run_dir.join("plan-review");
    let opts = PlanReviewOptions {
        run_dir: review_dir.clone(),
        time_limit_secs: run.options.review_time_limit_secs,
        // R6c：reviewer 容器路径（ws 全量 ro + 对话记录）；离线（ALFRED_OFFLINE=1）
        // 由 execute_plan_review 内部跳过（unscored → 升级属主）。
        container: ReviewerContainerOptions::from_governance(review_dir, &run.options),
    };
    let outcome = execute_plan_review(
        &opts,
        &ctx.reviewer_model,
        &run.request,
        &dagspec,
        Some(&run.session_doc),
    )?;
    match outcome.verdict {
        Some(v) => {
            run.plan_verdicts.push(v.clone());
            // 维护者 ①：计划审查结论落定后更新会话文档。maintain 失败（如 LLM
            // 输出格式漂移致解析失败）→ 回退旧 session_doc，不废整条 run；审计
            // 记 maintain_warning，治理环照常推进（plan_review_passed 照发）。
            match maintain(
                &MaintainOptions {
                    run_dir: ctx.run_dir.clone(),
                    model: ctx.planner_model.clone(),
                    container: alfred_planner::container::PlannerContainerOptions::from_governance(
                        ctx.run_dir.clone(),
                        &run.options,
                    ),
                },
                &run.session_doc,
                MaintainTrigger::PlanReviewed {
                    verdict: v.clone(),
                    plan: dagspec,
                },
            ) {
                Ok(updated) => run.session_doc = updated,
                Err(e) => {
                    audit(
                        &ctx.run_dir,
                        "maintain_warning",
                        &serde_json::json!({
                            "trigger": "plan_reviewed",
                            "error": format!("{e:#}"),
                            "fallback": "keep_previous_session_doc",
                        }),
                    )?;
                }
            }
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
            // §六继承项：审查本身出错（unscored/driver error）→ 必须升级，不悄悄放行。
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
        // R6e：执行产物落 run 级单一持久 ws（`<run>/ws`，git 基线），exec-N 只做
        // 记录（driver.py/compose/state.json）不挂产物。
        workspace_dir: ctx.run_dir.join("ws"),
        image: run.options.image.clone(),
        assignment,
        time_limit_secs: run.options.exec_time_limit_secs,
        port_base: run.options.port_base,
        settle_grace_seconds: run.options.settle_grace_seconds,
        ctl_enabled: run.options.ctl_enabled,
    };
    match execute_run(&opts, &ctx.executor_model, &run.request) {
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
            // 机械失败判定：driver error / timeout / crash（读 exec 子 run 的 state.json）。
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

/// ExecReviewing：执行审查改调 reviewer 容器（ws 全量 ro + 对话记录）→ §3.3 路由。
///
/// R6d：不再读执行容器内嵌 verdict（scorer 已移除，执行容器只出产物）。
/// 执行审查由 `execute_exec_review`（alfred-reviewer）在独立 reviewer 容器内
/// 判产物 vs 验收标准——容器挂 **ws 全量 ro**（执行者产物 run/ws，git 基线），
/// 审查者自己读 ws 全量（含超过旧 scorer 4000B/文件截断的内容）。
/// 离线回退（ALFRED_OFFLINE=1 或 ALFRED_EXEC_REVIEW_OFFLINE=1）：不跑容器
/// （无 docker）——执行容器无审查结论 → 升级属主（§六继承项，不悄悄放行）。
fn exec_review_step(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    pending: &mut Option<alfred_executor::run::RunOutcome>,
) -> Result<()> {
    // 消费执行 outcome（Executing → ExecReviewing 跨态传递；R6d 后执行容器
    // 不携带审查结论，仅保留跨态约束）。
    pending
        .take()
        .context("governance state ExecReviewing without execution outcome")?;
    let offline = std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1")
        || std::env::var("ALFRED_EXEC_REVIEW_OFFLINE").as_deref() == Ok("1");

    let (verdict, unscored_reason) = if offline {
        (
            None,
            "offline: 执行审查容器跳过（ALFRED_OFFLINE/ALFRED_EXEC_REVIEW_OFFLINE=1，执行 eval 无内嵌 scorer）"
                .to_string(),
        )
    } else {
        let dagspec = run
            .dagspec
            .clone()
            .context("governance state ExecReviewing without dagspec")?;
        let node = dagspec
            .nodes
            .first()
            .context("dagspec has no nodes (exec review)")?
            .clone();
        let contract = node.contract;
        let conversation = load_conversation(&ctx.run_dir)
            .map_err(anyhow::Error::msg)
            .ok()
            .flatten();
        // R6e：执行审查看 run 级单一持久 ws（git 基线）——executor 产物在 run/ws，
        // 不再挂 exec-{n}/workspace；reviewer 挂 ws 全量 ro 自己看 git diff。
        let ws_dir = ctx.run_dir.join("ws");
        let exec_review_dir = ctx.run_dir.join("exec-review");
        let opts = ExecReviewOptions::from_governance(exec_review_dir, ws_dir, &run.options);
        let outcome = execute_exec_review(
            &opts,
            &ctx.reviewer_model,
            &run.request,
            &contract,
            &node.sandbox.workspace_subdirs,
            conversation.as_ref(),
        )?;
        (
            outcome.verdict,
            outcome
                .unscored_reason
                .or(outcome.error)
                .unwrap_or_else(|| "exec review unscored".to_string()),
        )
    };

    match verdict {
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
            // §六继承项：执行审查本身出错（unscored / 离线回退）→ 升级，不悄悄放行。
            audit(
                &ctx.run_dir,
                "exec_review_error_escalated",
                &serde_json::json!({ "reason": unscored_reason }),
            )?;
            run.apply(GovernanceEvent::ExecReviewError)?;
        }
    }
    Ok(())
}

/// 读 exec 子 run 的 state.json，判是否为机械失败（state.json 的 eval_status 即容器驱动状态，!= success）。
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

/// 把 converse 产出的 DagSpec 格式化为语义回复（对话记录 converse.reply 轮的 content）。
///
/// M4-a：conversation.json 只承载 owner↔planner 语义轮次——落计划摘要，不落
/// 原始建图指令 JSON（中间指令属实现细节，已在 llm-calls/ 审计，避免冗余）。
fn format_plan_reply(dagspec: &alfred_core::DagSpec) -> String {
    let nodes: Vec<String> = dagspec
        .nodes
        .iter()
        .map(|n| format!("{}: {}", n.id, n.summary))
        .collect();
    format!("计划（{} 节点）：{}", dagspec.nodes.len(), nodes.join("；"))
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

/// 属主决策的对话记录标签（conversation.json panel.decision 轮 content，§二.8）。
fn panel_decision_text(d: OwnerDecision) -> &'static str {
    match d {
        OwnerDecision::Retry => "重跑（retry）",
        OwnerDecision::Revise => "改需求重新规划（revise）",
        OwnerDecision::Abandon => "放弃（abandon）",
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
