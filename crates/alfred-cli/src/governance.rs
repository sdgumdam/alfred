//! 治理环驱动（编排器状态机驱动核心）。
//!
//! `run_governance_loop` 是编排器状态机的驱动循环：从当前状态出发，一路
//! 推进到挂起态（PlanRejected / Escalated）或终态（Completed / Abandoned）。
//! owner 交互入口 = `alfred chat`（`chat.rs` 持续会话 REPL，复用本库 +
//! `feed_owner_message`）；`alfred run/feed/status` 为脚本/e2e 技术 driver。
//! 每次状态进入打印 `[orchestrator]` 流转状态行（owner 可见的协调者路由行为）。
//!
//! 确定性：状态转移全部经 `GovernanceRun.apply()`（alfred-core 状态机），
//! 每次转移落 audit.jsonl + persist state.json（P3 崩溃恢复显式化）；机械失败
//! 重跑预算 N=2（§3.3）；审查本身出错（unscored / driver error）→ 升级属主
//! （§六继承项，不悄悄放行）。单节点骨架显式拒绝多节点 DAG（P2，不静默截断）。
use std::path::{Path, PathBuf};

use alfred_core::conversation::{
    append_to_disk, load_conversation, ConversationRole, ConversationSource,
};
use alfred_core::governance::{
    GovernanceEvent, GovernanceOptions, GovernanceRun, GovernanceState, OwnerDecision,
};
use alfred_core::request::OwnerRequest;
use alfred_core::util::now_rfc3339;
use alfred_executor::agt::resolve_agt_source;
use alfred_executor::config::{
    load_executor_model, load_planner_model, load_reviewer_model, ExecutorModel,
};
use alfred_executor::run::{ensure_run_workspace, execute_run, RunOptions};
use alfred_planner::converse::{converse, ConverseOptions, ConverseOutcome};
use alfred_planner::disguise::disguise_rejection;
use alfred_planner::host::PlannerHostOptions;
use alfred_planner::maintain::{run_maintain, MaintainTrigger};
use alfred_reviewer::exec_review::{execute_exec_review, ExecReviewOptions};
use alfred_reviewer::host::{ReviewerHostOptions, EXEC_VERDICTS_FILE, PLAN_VERDICTS_FILE};
use alfred_reviewer::plan_review::{execute_plan_review, PlanReviewOptions};
use anyhow::{bail, Context, Result};
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
pub fn run_governance_loop(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
) -> Result<Option<String>> {
    // 执行结果跨态传递（Executing → ExecReviewing）。
    let mut pending_exec: Option<alfred_executor::run::RunOutcome> = None;

    loop {
        audit(
            &ctx.run_dir,
            "state_entered",
            &serde_json::json!({ "state": state_label(run.state()) }),
        )?;
        // 自主流转呈现（工单③）：每次状态进入打一行 [orchestrator] 状态行——
        // owner（chat 终端 / CLI driver）看到协调者的路由行为。
        println!("[orchestrator] {}", orchestrator_status_line(run.state()));
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
                            "[orchestrator] 规划失败已升级属主（state=Escalated，挂起）。\n\
                             \x20 run_dir: {}；等待属主拍板（retry/revise/abandon）。",
                            ctx.run_dir.display()
                        );
                        return Ok(None);
                    }
                }
            }
            alfred_core::governance::GovernanceState::PlanReviewing => plan_review_step(run, ctx)?,
            alfred_core::governance::GovernanceState::PlanRejected => {
                // 挂起呈现已由循环顶 [orchestrator] 状态行承担。
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Executing => {
                pending_exec = execution_step(run, ctx)?;
            }
            alfred_core::governance::GovernanceState::ExecReviewing => {
                exec_review_step(run, ctx, &mut pending_exec)?;
            }
            alfred_core::governance::GovernanceState::Completed => {
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Escalated => {
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Abandoned => {
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
/// 1. **Abandon 前置路由（P2b）**：`decision=Abandon` 不要求消息、不落
///    owner.message 轮——直接 `apply(OwnerAbandon)` 进终态。属主放弃恒可选：
///    即使 run 已坏（planner 容器故障/不可用）也拦不住属主放弃。
/// 2. 有消息 → `run.owner_message = message`（下一轮 `planning_step` converse 读它）、
///    落 conversation.json（`ConversationSource::OwnerMessage`，属主轮次）。
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
            && matches!(decision, OwnerDecision::Revise | OwnerDecision::Abandon));
    if !allowed {
        bail!(
            "feed_owner_message: 决策 {:?} 不适用于当前状态 {:?}（仅挂起态可拍板；Planning 态仅 revise/abandon 可操作）",
            decision,
            state
        );
    }

    // P2b：Abandon 前置路由——不要求消息、不落 owner.message 轮。
    // decision=Abandon 直接 apply(OwnerAbandon) 进终态，不触碰 planner（run 已坏也
    // 能弃）。Planning 态走 P2a 转移 (Planning, OwnerAbandon) → Abandoned（属主
    // 放弃恒可选——转移表已支持，chat 的对话态放弃出口即走此行）。
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
    let driving_message: String =
        if state == GovernanceState::PlanRejected && decision == OwnerDecision::Retry {
            let dagspec = run.dagspec.clone().context("no dagspec in PlanRejected")?;
            let reason = run
                .plan_verdicts
                .last()
                .map(|v| v.reason.clone())
                .unwrap_or_default();
            disguise_rejection(&run.request, &dagspec, &reason).map_err(anyhow::Error::msg)?
        } else {
            message.to_string()
        };

    // 有消息 → 设 owner_message + 落对话轮；无消息（Retry）→ 跳过消息轮。
    if !driving_message.is_empty() {
        // 设置 owner_message（planning_step 下一轮 converse 读它；重规划/改需求语义）。
        run.owner_message = Some(driving_message.clone());

        // 落 conversation.json（属主轮次，reviewer 挂载输入数据源，§二.8）。
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

/// 维护者选项（converse 同款派生面：run 级 pi-config / AGT / 项目根 cwd）。
fn maintainer_opts(run: &GovernanceRun, ctx: &GovernanceContext) -> PlannerHostOptions {
    PlannerHostOptions::from_governance(ctx.run_dir.clone(), &run.options)
}

/// 触发滚动维护（ConverseDone）：收割更新后的会话文档 + 落 audit + llm-calls
/// （run_maintain 内部落 role=maintain 记录）。维护失败**显式报错**——记忆坏了
/// 要可见，不悄悄放行（无静默出口）。
fn maintain_after_converse(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    read_paths: Vec<String>,
    reply_summary: &str,
) -> Result<()> {
    let trigger = MaintainTrigger::ConverseDone {
        read_paths,
        reply_summary: reply_summary.to_string(),
    };
    run.session_doc = run_maintain(
        &maintainer_opts(run, ctx),
        &ctx.planner_model,
        &run.session_doc,
        &trigger,
    )?;
    audit(
        &ctx.run_dir,
        "maintain_done",
        &serde_json::json!({
            "trigger": "converse_done",
        }),
    )?;
    Ok(())
}

/// 触发审查意见维护（PlanReviewed）：审查理由先经 `disguise_rejection` 转写为
/// 属主口吻中性文本（disguise 投影——维护者不可见审查语义），再喂维护者落
/// review_summary（磁盘真源字段名不变，投影层才改名 owner_feedback）。
fn maintain_after_plan_review(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    reason: &str,
) -> Result<()> {
    let dagspec = run
        .dagspec
        .clone()
        .context("maintain_after_plan_review without dagspec")?;
    let disguised =
        disguise_rejection(&run.request, &dagspec, reason).map_err(anyhow::Error::msg)?;
    let trigger = MaintainTrigger::PlanReviewed {
        disguised_review: disguised,
    };
    run.session_doc = run_maintain(
        &maintainer_opts(run, ctx),
        &ctx.planner_model,
        &run.session_doc,
        &trigger,
    )?;
    audit(
        &ctx.run_dir,
        "maintain_done",
        &serde_json::json!({
            "trigger": "plan_reviewed",
        }),
    )?;
    Ok(())
}

/// Planning：converse（会话文档 + 属主消息 → §2.4 两分支）+ E5 reviewer_models 注入。
///
/// 返回 `Ok(None)` = 产出计划（已 apply PlanProduced → PlanReviewing，编排环继续）；
/// `Ok(Some(reply))` = 规划器答复了属主（纯文本答复，不产计划——对话继续，状态仍
/// Planning，编排环返回调用方；答复文本 surface 给调用方（driver 打印，P2-2），
/// 调用方经下一轮属主消息（revise 语义）续入对话（P1-2）。
fn planning_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<Option<String>> {
    // key_file_paths 真实数据源（维护者重做）：converse 前快照 AGT 审计行数，
    // converse 落定后增量提取本轮 allow read 宿主路径（确定性提取+去重）。
    let audit_lines_before = alfred_planner::host::snapshot_audit_lines(&ctx.run_dir);
    let owner_message = match &run.owner_message {
        Some(m) => m.clone(),
        None => alfred_planner::format_request_message(&run.request),
    };
    let opts = ConverseOptions {
        run_dir: ctx.run_dir.clone(),
        model: ctx.planner_model.clone(),
        host: alfred_planner::host::PlannerHostOptions::from_governance(
            ctx.run_dir.clone(),
            &run.options,
        ),
        append_system_prompt: ctx.append_system_prompt.clone(),
    };
    let outcome = converse(&opts, &run.request, &run.session_doc, &owner_message)?;
    match outcome {
        // ---- §2.4 建图指令分支：DagSpec 交编排器接管（计划审查 → 执行） ----
        ConverseOutcome::Instructions {
            dagspec,
            record_path,
        } => {
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
            // 矩阵 §1.1 第 7 行：planner 回看"自己写的契约"——run 级 contract.json 由
            // 容器驱动首轮落 `{}` 占位（run_planner_container），dagspec 落定时这里写真
            // 内容（首节点契约投影）。必须在 E5 注入**前**写：reviewer_models 是系统
            // 注入的审查者信息，规划器不感知（注入后版本只进 dagspec.json 供审查/编排）。
            write_run_contract(&ctx.run_dir, &dagspec)?;
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
            // ConverseDone 滚动维护（每轮 converse 落定后）：key_file_paths（本轮
            // AGT 审计 allow read 增量）+ key_conclusions（计划摘要）——下轮 converse
            // 即用上新记忆。
            let read_paths =
                alfred_planner::host::extract_allow_read_paths(&ctx.run_dir, audit_lines_before);
            maintain_after_converse(run, ctx, read_paths, &format_plan_reply(&dagspec))?;
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
            // ConverseDone 滚动维护：答复文本作为 reply_summary（key_conclusions 语义）。
            let read_paths =
                alfred_planner::host::extract_allow_read_paths(&ctx.run_dir, audit_lines_before);
            maintain_after_converse(run, ctx, read_paths, &reply)?;
            Ok(Some(reply))
        }
    }
}

/// reviewer 宿主 pi 的 cwd（治理对象项目根）。
///
/// 宿主形态：run 目录挂项目根之下（如 `<项目>/alfred-runs/<run_id>/`），项目根
/// = run 目录所属仓库根——向上找 Cargo.toml/pyproject.toml/.git 边界，兜底 run
/// 目录父目录链上第一个存在的祖先（保证 pi cwd 在项目内，宿主材料/run 产物都
/// 经绝对路径可达）。
fn project_root_for(run_dir: &Path) -> PathBuf {
    let mut cur = run_dir.to_path_buf();
    while let Some(parent) = cur.parent() {
        cur = parent.to_path_buf();
        let has_marker = cur.join(".git").exists()
            || cur.join("Cargo.toml").exists()
            || cur.join("package.json").exists()
            || cur.join("pyproject.toml").exists();
        if has_marker {
            return cur;
        }
    }
    // 无边界标记（如 e2e 临时目录直接落在 ~ 下）：退回 run 目录父目录。
    run_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| run_dir.to_path_buf())
}

/// PlanReviewing：宿主 pi reviewer 判忠实度 → pass/打回/出错升级。
fn plan_review_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
    let dagspec = run
        .dagspec
        .clone()
        .context("governance state PlanReviewing without dagspec")?;
    let review_dir = ctx.run_dir.join("plan-review");
    let opts = PlanReviewOptions {
        run_dir: review_dir.clone(),
        time_limit_secs: run.options.review_time_limit_secs,
        // 宿主 pi 路径（reviewer 全可见 + AGT 拦写；离线 ALFRED_OFFLINE=1 由
        // execute_plan_review 内部跳过 → unscored → 升级属主）。
        host: ReviewerHostOptions::from_governance(
            review_dir,
            project_root_for(&ctx.run_dir),
            &run.options,
        ),
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
                // PlanReviewed 维护（审查结论落定后）：拒绝理由经 disguise 投影
                // （属主口吻中性转写——维护者零 reviewer 痕迹）落 review_summary。
                maintain_after_plan_review(run, ctx, &v.reason)?;
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
        // AGT 拦写层（executor 行，属主拍板项：权限控制不让写文件 + 默认启用）：
        // 与 planner/reviewer 共用解析语义——`ALFRED_AGT_DISABLE=1` 关，
        // `ALFRED_AGT_DIR` 显式目录覆盖，未设 = 内置默认策略（alfred_core::agt）。
        agt: resolve_agt_source(),
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
                        "[orchestrator] 执行机械失败，按同一契约重跑（{}/{}）：{e}",
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

/// ExecReviewing：执行审查改调宿主 pi reviewer（ws 全量自由读 + 对话记录）→ §3.3 路由。
///
/// R6d：不再读执行容器内嵌 verdict（scorer 已移除，执行容器只出产物）。
/// 执行审查由 `execute_exec_review`（alfred-reviewer）由宿主 pi 判产物 vs 验收
/// 标准——reviewer 全量自由读 run/ws（执行者产物，git 基线），含超过旧 scorer
/// 4000B/文件截断的内容。
/// 离线回退（ALFRED_OFFLINE=1 或 ALFRED_EXEC_REVIEW_OFFLINE=1）：不跑 pi——
/// 执行无审查结论 → 升级属主（§六继承项，不悄悄放行）。
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
            "offline: 执行审查宿主 pi 跳过（ALFRED_OFFLINE/ALFRED_EXEC_REVIEW_OFFLINE=1，执行 eval 无内嵌 scorer）"
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
        // R6e：执行审查看 run 级单一持久 ws（git 基线）——executor 产物在 run/ws；
        // reviewer 全量自由读 ws 自己看 git diff。
        let ws_dir = ctx.run_dir.join("ws");
        let exec_review_dir = ctx.run_dir.join("exec-review");
        let opts = ExecReviewOptions::from_governance(
            exec_review_dir,
            ws_dir,
            project_root_for(&ctx.run_dir),
            &run.options,
        );
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
                            "[orchestrator] 执行审查 contract_fault：预标注『建议改契约』，升级属主。"
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
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let status = v["run"]["eval_status"].as_str().unwrap_or("success");
    Ok(status != "success")
}

/// 落盘 dagspec.json。
fn write_dagspec(run_dir: &Path, dagspec: &alfred_core::DagSpec) -> Result<()> {
    let text = serde_json::to_string_pretty(dagspec).context("serialize dagspec")?;
    std::fs::write(run_dir.join("dagspec.json"), text).context("write dagspec.json")
}

/// 落盘 run 级契约 contract.json（矩阵 §1.1 第 7 行：planner 挂"自己写的契约" ro 回看）。
///
/// 投影真源 = [`alfred_core::DagSpec::contract_json`]（首节点契约全字段，与
/// reviewer 输入 contract.json 同源）。dagspec 每次落定即重写——重规划轮 planner
/// 读到的恒为最新一轮自己写的契约。
fn write_run_contract(run_dir: &Path, dagspec: &alfred_core::DagSpec) -> Result<()> {
    let text = dagspec.contract_json().context("serialize run contract")?;
    std::fs::write(run_dir.join("contract.json"), text).context("write contract.json")
}

/// 落盘 run 级 verdict 历史文件（矩阵 §1.1 第 8 行：审查记录/verdict）。
fn write_verdict_history<T: serde::Serialize>(
    run_dir: &Path,
    name: &str,
    verdicts: &[T],
) -> Result<()> {
    let text =
        serde_json::to_string_pretty(verdicts).with_context(|| format!("serialize {name}"))?;
    std::fs::write(run_dir.join(name), text).with_context(|| format!("write {name}"))
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
    serde_json::from_str(&text)
        .with_context(|| format!("parse governance state {}", path.display()))
}

/// 落盘治理环 state.json + run 级 verdict 历史投影（plan-verdicts.json /
/// exec-verdicts.json，矩阵 §1.1 第 8 行 reviewer 挂载输入）。
pub fn persist_governance_run(run_dir: &Path, run: &GovernanceRun) -> Result<()> {
    let text = serde_json::to_string_pretty(run).context("serialize governance state")?;
    std::fs::write(run_dir.join("state.json"), text).context("write governance state.json")?;
    // verdict 历史单一真源 = state.json 的 plan_verdicts/exec_verdicts：每次
    // persist 整体重写（不追加不删改），与状态机持久化同生命周期——崩溃恢复后
    // 仍同步。reviewer 容器（container.rs verdict_history_mounts）按存在性挂 ro。
    write_verdict_history(run_dir, PLAN_VERDICTS_FILE, &run.plan_verdicts)?;
    write_verdict_history(run_dir, EXEC_VERDICTS_FILE, &run.exec_verdicts)?;
    Ok(())
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

/// [orchestrator] 流转状态行文本（工单③：owner 可见的编排器路由行为）。
/// 每次 `run_governance_loop` 状态进入打一行；chat / run / feed 共用同一呈现。
fn orchestrator_status_line(s: alfred_core::governance::GovernanceState) -> &'static str {
    use alfred_core::governance::GovernanceState::*;
    match s {
        Planning => "规划中（planner converse 建图/对话）…",
        PlanReviewing => "计划审查中…",
        PlanRejected => "计划被打回（PlanRejected），挂起等待属主拍板。",
        Executing => "执行中（沙箱容器）…",
        ExecReviewing => "执行审查中…",
        Completed => "全流程完成（Completed）：验收 C 推进到终点。",
        Escalated => "已升级属主（Escalated），挂起等待属主拍板。",
        Abandoned => "属主放弃（Abandoned，终态）。",
    }
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

/// 默认治理 runs 基目录（`$ALFRED_STATE_DIR` 或 `~/.local/state/alfred/runs`）——
/// `run-<id>` 子目录的父目录；`chat` 的 run 发现扫描这里（P3 发现规则）。
pub fn default_governance_base() -> PathBuf {
    std::env::var("ALFRED_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            PathBuf::from(home).join(".local/state/alfred/runs")
        })
}

/// 默认治理 run 目录（runs 基目录下新起 `run-<id>`，id 纳秒戳唯一）。
pub fn default_governance_dir() -> PathBuf {
    default_governance_base().join(alfred_core::util::short_id("run"))
}

/// 初始化治理 run 目录 + run 实体（`cmd_run` 与 `chat` 需求收集共用的单一初始化
/// 路径，不复制第二份）：建目录 + R6e 单一持久 ws（git 基线）+ request.json 落盘 +
/// R6a 对话记录 request.submit 首轮 + GovernanceRun 构造 + governance_started 审计。
/// 调用方拿到 run 后自行 `run_governance_loop` 推进并 `persist_governance_run`。
pub fn init_governance_run(
    run_dir: &Path,
    request: OwnerRequest,
    options: GovernanceOptions,
) -> Result<GovernanceRun> {
    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
    // R6e：治理 run 初始化单一持久 ws（git init 基线快照）——三容器共享此 ws。
    ensure_run_workspace(run_dir)?;
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
    // R6a：对话记录（reviewer 挂载输入数据源，§二.8）——初始需求提交轮。
    append_to_disk(
        run_dir,
        &run_id,
        ConversationRole::Owner,
        alfred_planner::format_request_message(&request),
        ConversationSource::RequestSubmit,
    )
    .map_err(anyhow::Error::msg)
    .context("append request.submit to conversation.json")?;
    let run = GovernanceRun::new(run_id, request, options);
    audit(
        run_dir,
        "governance_started",
        &serde_json::json!({ "request_id": run.request.id }),
    )?;
    Ok(run)
}

/// 组装治理驱动上下文（模型配置 + codux wrapper 注入的系统提示）——
/// `cmd_run` / `cmd_feed` / `chat` 共用，单一真源。
pub fn build_governance_context(run_dir: &Path) -> Result<GovernanceContext> {
    Ok(GovernanceContext {
        run_dir: run_dir.to_path_buf(),
        planner_model: load_planner_model()?,
        executor_model: load_executor_model()?,
        reviewer_model: load_reviewer_model()?,
        append_system_prompt: std::env::var("ALFRED_APPEND_SYSTEM_PROMPT").unwrap_or_default(),
    })
}
