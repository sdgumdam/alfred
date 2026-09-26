//! 治理环驱动（编排器状态机驱动核心）。
//!
//! `run_governance_loop` 是编排器状态机的驱动循环：从当前状态出发，一路
//! 推进到挂起态（PlanRejected / Escalated）或终态（Completed / Abandoned）。
//! owner 交互入口 = `alfred chat`（`chat.rs` 持续会话 REPL，复用本库 +
//! `feed_owner_message`）；`alfred run/feed/status` 为脚本/e2e 技术 driver。
//! 每次状态进入打一行 `[orchestrator]` 流转状态行（owner 可见的协调者路由行为）
//! ——S2b 事件化：全部 `[orchestrator]` 点位经 [`orchestrator_notice`] 单一出口，
//! REPL/driver 打终端（逐字节不变）、TUI 走 [`ChatEvent`] 事件通道。
//!
//! 确定性：状态转移全部经 `GovernanceRun.apply()`（alfred-core 状态机），
//! 每次转移落 audit.jsonl + persist state.json（P3 崩溃恢复显式化）；机械失败
//! 重跑预算 N=2（§3.3，M3 起 per-node）；审查本身出错（unscored / driver error）
//! → 升级属主（§六继承项，不悄悄放行）。M3 起多节点 DAG 拓扑序调度：Executing
//! 自环逐节点推进（completed_nodes 持久断点续跑），全图完成才进 ExecReviewing。
use std::path::{Path, PathBuf};

use crate::chat_events::{ChatEvent, ChatEventSender};
use crate::governance_intent::{commit_intent, ConverseMaintain, Effects, StepIntent, VerdictKind};

use alfred_core::conversation::{
    append_to_disk, load_conversation, ConversationRole, ConversationSource,
};
use alfred_core::governance::{
    GovernanceAblation, GovernanceEvent, GovernanceOptions, GovernanceRun, GovernanceState,
    OwnerDecision,
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
    /// TUI 事件通道（S2b 通知事件化）：`None` = REPL / CLI driver（run/feed）
    /// ——`[orchestrator]` 通知打终端（与 HEAD 逐字节一致）；`Some` = TUI 治理
    /// worker——通知经 [`ChatEvent::OrchestratorNotice`] 事件进左列（载荷=去前缀
    /// 正文，契约见 chat_events.rs）。`build_governance_context` 恒 None，
    /// chat.rs 会话侧按 sink 接入。
    pub events: Option<ChatEventSender>,
}

/// 治理环 owner 通知单一出口（S2b 事件化）：governance.rs 全部 `[orchestrator]`
/// 点位（流转状态行 / 机械重跑提示 / contract_fault 预标注 / 规划失败与审查
/// 宿主失败升级块）经此发射。
///
/// - REPL / CLI driver（`ctx.events` 缺席）：终端 `println!` 带 `[orchestrator] `
///   前缀——与 HEAD 逐字节一致（e2e chat.sh 硬底线；run/feed driver 同面）。
/// - TUI（通道在场）：发 [`ChatEvent::OrchestratorNotice`]，载荷=去前缀纯正文
///   （契约见 chat_events.rs：`[orchestrator]` 前缀由 sink 渲染时统一加回）；
///   不打终端——TUI 期间 stdout 已被捕获管道接管，捕获透传路径已退役
///   （chat_tui drainer 只留 [chat]/[pi] 残留兜底）。
pub(crate) fn orchestrator_notice(ctx: &GovernanceContext, body: &str) {
    match &ctx.events {
        None => println!("[orchestrator] {body}"),
        Some(tx) => {
            tx.send(ChatEvent::OrchestratorNotice(body.to_string()));
        }
    }
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
    loop {
        audit(
            &ctx.run_dir,
            "state_entered",
            &serde_json::json!({ "state": state_label(run.state()) }),
        )?;
        // 自主流转呈现（工单③）：每次状态进入打一行 [orchestrator] 状态行——
        // owner（chat 终端 / CLI driver）看到协调者的路由行为（S2b 经单一出口
        // 事件化，REPL 逐字节不变）。
        orchestrator_notice(ctx, orchestrator_status_line(run.state()));
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
                        // 收表（步骤④）：PlanningError 降级走 commit_intent 单点
                        // （audit → apply → persist 同 HEAD 序列；随后 inline return
                        // ——Planning 态降级是 loop 内联返回路径，非继续流转）。
                        commit_intent(
                            run,
                            ctx,
                            StepIntent::Escalate {
                                event: GovernanceEvent::PlanningError,
                                audit_name: "planning_error_escalated".into(),
                                audit_data: None,
                                reason: format!("{e:#}"),
                            },
                        )?;
                        orchestrator_notice(
                            ctx,
                            &format!(
                                "规划失败已升级属主（state=Escalated，挂起）。\n\
                                 \x20 run_dir: {}；等待属主拍板（retry/revise/abandon）。",
                                ctx.run_dir.display()
                            ),
                        );
                        return Ok(None);
                    }
                }
            }
            alfred_core::governance::GovernanceState::PlanReviewing => {
                // 9/3 欠账：计划审查失败（reviewer 宿主驱动超时/崩溃/verdict 落盘
                // 失败）不走 Err 穿出卡死——与 exec_review_step 同构治理降级：
                // fail_review 已落盘 outcome（timed_out/error），此处落升级事件
                // （escalation_source=plan_review）挂起属主拍板，不悄悄放行。
                if let Err(e) = plan_review_step(run, ctx) {
                    review_host_failure_escalate(run, ctx, "plan_review", e)?;
                }
            }
            alfred_core::governance::GovernanceState::PlanRejected => {
                // 挂起呈现已由循环顶 [orchestrator] 状态行承担。
                return Ok(None);
            }
            alfred_core::governance::GovernanceState::Executing => {
                // 执行失败路径（机械重跑/硬错误升级）在 execution_step 内处理；
                // 成功 outcome 无消费方（R6d 起执行容器只出产物，执行审查输入全
                // 在磁盘）——不再跨态传递进程内数据（磁盘重入，见 exec_review_step）。
                execution_step(run, ctx)?;
            }
            alfred_core::governance::GovernanceState::ExecReviewing => {
                // 9/3 欠账（用户死锁链 run-18d1c83accf4c04002）：执行审查失败
                // （reviewer 宿主驱动超时/失败）不再 Err 穿出卡死在 exec_reviewing
                // ——execute_exec_review 失败路径已落盘 outcome（timed_out/error，
                // 容器时代 fail_exec_review 语义），此处走治理降级：ExecReviewError
                // → Escalated（escalation_source=execution，§六继承项"审查出错升级
                // 不悄悄放行"），属主拍板续跑。
                if let Err(e) = exec_review_step(run, ctx) {
                    review_host_failure_escalate(run, ctx, "exec_review", e)?;
                }
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
/// （run_maintain 内部落 role=maintain 记录）+ **推进持久审计基线**（维护成功
/// 后才推进——失败/崩溃行不丢，下轮补提取）。维护失败**显式报错**——记忆坏了
/// 要可见，不悄悄放行（无静默出口）。
///
/// `owner_message` = 本轮规划器 converse 的属主消息原文——维护者据此把原始
/// 需求/属主补充固化进 key_conclusions（原始用例修复：轮1建图失败无答复时，
/// 需求此前从未进会话记忆，下轮规划器"无需求基线"）。
pub(crate) fn maintain_after_converse(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    read_paths: Vec<String>,
    owner_message: &str,
    reply_summary: &str,
) -> Result<()> {
    let trigger = MaintainTrigger::ConverseDone {
        read_paths,
        owner_message: owner_message.to_string(),
        reply_summary: reply_summary.to_string(),
    };
    run.session_doc = run_maintain(
        &maintainer_opts(run, ctx),
        &ctx.planner_model,
        &run.session_doc,
        &trigger,
    )?;
    // 基线推进到当前审计末尾（维护成功 = 增量已被消费）。
    let consumed = alfred_planner::host::snapshot_audit_lines(&ctx.run_dir);
    alfred_planner::host::write_audit_baseline(&ctx.run_dir, consumed)?;
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
pub(crate) fn maintain_after_plan_review(
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
    // 基线推进到当前审计末尾（审查期 planner 未跑，行数应不变；防御性推进）。
    let consumed = alfred_planner::host::snapshot_audit_lines(&ctx.run_dir);
    alfred_planner::host::write_audit_baseline(&ctx.run_dir, consumed)?;
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
    // key_file_paths 真实数据源（维护者重做）：从**持久审计基线**（上轮维护成功
    // 后推进的行数）增量提取 allow read 宿主路径——崩溃/维护失败不丢行，跨进程
    // 轮次间隙的审计行照常进下轮增量。
    let audit_baseline = alfred_planner::host::read_audit_baseline(&ctx.run_dir);
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
            let node_summaries: Vec<String> = dagspec
                .nodes
                .iter()
                .map(|n| format!("{}:{}", n.id, n.summary))
                .collect();
            let planning_done_data = serde_json::json!({
                "request_id": dagspec.request_id,
                "node_count": dagspec.nodes.len(),
                "nodes": node_summaries,
                "record": record_path,
            });
            let read_paths =
                alfred_planner::host::extract_allow_read_paths(&ctx.run_dir, audit_baseline);
            // v3 分通道收集：决策纯——通道只收集不执行（flush 在 commit_intent，
            // 通道序 = HEAD 副作用序：审计 → conversation 轮 → dagspec 落盘/注入
            // → ConverseDone 维护）。
            let plan_reply = format_plan_reply(&dagspec);
            let mut effects = Effects::default();
            effects.audit("planning_done", planning_done_data);
            effects.conversation_turn(
                ConversationRole::Planner,
                plan_reply.clone(),
                ConversationSource::ConverseReply,
            );
            effects.dagspec = Some(dagspec);
            effects.converse_maintain(ConverseMaintain {
                read_paths,
                owner_message,
                reply_summary: plan_reply,
            });
            let intent = StepIntent::Proceed {
                event: GovernanceEvent::PlanProduced,
                effects,
            };
            commit_intent(run, ctx, intent)?;
            Ok(None)
        }
        // ---- §2.4 答复分支：纯文本答复给属主，不强制产 DagSpec（对话继续） ----
        ConverseOutcome::Reply { reply, record_path } => {
            let read_paths =
                alfred_planner::host::extract_allow_read_paths(&ctx.run_dir, audit_baseline);
            // v3 分通道收集：conversation 轮 + 审计 + ConverseDone 维护触发
            // 全部收集进通道（flush 在 commit_intent：turn 落盘 → 审计 → 维护）。
            let mut effects = Effects::default();
            effects.audit(
                "converse_reply",
                serde_json::json!({ "record": record_path }),
            );
            effects.conversation_turn(
                ConversationRole::Planner,
                reply.clone(),
                ConversationSource::ConverseReply,
            );
            effects.converse_maintain(ConverseMaintain {
                read_paths,
                owner_message,
                reply_summary: reply.clone(),
            });
            let intent = StepIntent::Reply {
                text: reply.clone(),
                effects,
            };
            commit_intent(run, ctx, intent)
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
    let intent = match outcome.verdict {
        Some(v) => {
            // verdict 归档由 commit_intent 的 verdicts 通道执行（分通道 flush）。
            let mut effects = Effects::default();
            effects.verdicts.push(VerdictKind::Plan(v.clone()));
            if v.pass {
                effects.audit(
                    "plan_review_passed",
                    serde_json::json!({ "reason": v.reason }),
                );
                StepIntent::Proceed {
                    event: GovernanceEvent::PlanReviewPassed,
                    effects,
                }
            } else if matches!(run.options.ablation, Some(GovernanceAblation::AuditOnly)) {
                // A3 仅审计不强制处置：打回事实照常归档（verdict 入历史 +
                // 审计），但"计划打回→强制重规划"处置断开——改走合法通过
                // 转移（PlanReviewPassed），计划照常进入执行。属主重规划
                // 维护触发（PlanReviewed，服务于打回重规划轮）随处置一并
                effects.audit(
                    "plan_review_rejected",
                    serde_json::json!({ "reason": v.reason.clone() }),
                );
                // 审计显式区分：verdict_outcome = 审查真实结论（rejected）；
                // applied_event 是 A3 消融分支的合法转移，不是审查裁决——
                // "review passed"不得被读成原裁判结果（Main 硬约束）。
                effects.audit(
                    "ablation_a3_disposal_disconnected",
                    serde_json::json!({
                        "verdict_outcome": "rejected",
                        "transition_source": "ablation_a3_audit_only",
                        "would_event": "PlanReviewRejected",
                        "applied_event": "PlanReviewPassed",
                        "reason": v.reason,
                    }),
                );
                StepIntent::Proceed {
                    event: GovernanceEvent::PlanReviewPassed,
                    effects,
                }
            } else {
                effects.audit(
                    "plan_review_rejected",
                    serde_json::json!({ "reason": v.reason.clone() }),
                );
                effects.plan_reviewed_maintain(v.reason);
                StepIntent::Proceed {
                    event: GovernanceEvent::PlanReviewRejected,
                    effects,
                }
            }
        }
        None => {
            // §六继承项：审查本身出错（unscored/driver error）→ 必须升级，不悄悄放行。
            let reason = outcome
                .unscored_reason
                .or(outcome.error)
                .unwrap_or_else(|| "plan review unscored".to_string());
            let mut effects = Effects::default();
            effects.audit(
                "plan_review_error_escalated",
                serde_json::json!({ "reason": reason }),
            );
            StepIntent::Proceed {
                event: GovernanceEvent::PlanReviewError,
                effects,
            }
        }
    };
    commit_intent(run, ctx, intent)?;
    Ok(())
}

/// 执行成功/失败路由（机械重跑/硬错误升级）全在函数内处理；R6d 起执行容器只出
/// 产物、执行审查输入全在磁盘——成功 outcome 无进程内消费方，不再返回。
///
/// M3 多节点拓扑序调度：Executing 态自环逐节点推进——每轮取依赖序中首个
/// 未完成节点（[`pending_node`] 单一真源，前置满足性由拓扑序保证）执行；
/// 节点成功 → `completed_nodes` 持久 + 自环 [`GovernanceEvent::ExecutionNodeCompleted`]
/// 推进下一节点；**全图完成 → `ExecutionSucceeded` → ExecReviewing**（C 转移
/// 语义 = 全图完成）。所有节点共享同一 run/ws（挂载面不变——下游节点天然
/// 看到上游产物）。断点恢复：`completed_nodes` 落 state.json，崩溃后从已完成
/// 节点续跑；机械重跑预算 per-node（节点完成即重置 attempts_used）。
///
/// G1（driver 孤儿检测）：执行前查下一 exec 目录的孤儿签名（驱动已启动未
/// 收尾——[`orphaned_driver_run`]）——上个进程在 poll 中途死亡（TUI 退出收割 /
/// kill -9）时驱动与容器成孤儿、done 永不出现，state.json 永停 executing。
/// 检出即按 crashed 语义路由 [`execution_failure_intent`]（机械重跑/耗尽
/// 升级），不无限等也不覆写孤儿目录。
fn execution_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
    let dagspec = run
        .dagspec
        .clone()
        .context("governance state Executing without dagspec")?;
    if dagspec.nodes.is_empty() {
        bail!("dagspec has no nodes");
    }
    let node = match pending_node(&dagspec, &run.completed_nodes)? {
        Some(node) => node,
        None => {
            // Executing 态全图已完成 = 图级重跑周期（ExecReviewMechanicalRetry
            // 回跳 / 属主 retry 自执行审查升级重入）——清空完成集从头推进
            // （HEAD 单节点"重入执行即重跑"语义的图级推广）。
            audit(
                &ctx.run_dir,
                "execution_graph_rerun",
                &serde_json::json!({ "completed_nodes": run.completed_nodes.len() }),
            )?;
            run.completed_nodes.clear();
            pending_node(&dagspec, &run.completed_nodes)?.context("dagspec has no nodes")?
        }
    };
    // 本轮尝试号（per-node 机械重试计数 attempts_used + 1；节点完成时归零）。
    let attempt = run.attempts_used + 1;
    let assignment = alfred_core::TaskAssignment {
        task_id: node.id.clone(),
        handler: "run_inspect_eval".to_string(),
        contract: node.contract.clone(),
        sandbox: node.sandbox.clone(),
    };
    // G1（driver 孤儿检测）：下一 exec 目录已有"驱动已启动未收尾"痕迹 = 上个
    // 进程在 poll 中途死亡——本进程续跑必须接住，视为机械失败（crashed 语义：
    // 与驱动进程内崩溃同路由，error 非 timeout 不放大时间上限）走既有
    // mechanical_retry/耗尽路径。孤儿目录序号消费掉：重跑落新 exec 目录，
    // 孤儿目录（driver.py/audit/done 痕迹）不被覆写，取证面保留。
    let orphan_dir = ctx.run_dir.join(format!("exec-{}", run.execution_count + 1));
    if orphaned_driver_run(&orphan_dir) {
        run.execution_count += 1;
        // G1 孤儿恢复 = 状态处置 + 资源回收两件事。先按孤儿 exec 目录里
        // driver 落的精确项目身份记录（driver.project.json，sample_init 返回
        // 即写）把 compose 项目 down 掉——现成 Inspect project_cleanup，精确
        // 关联本孤儿，非全局扫描；无记录/回收失败 → 审计如实，不阻塞状态
        // 路由。被杀驱动的容器不因恢复而泄漏，也不碰其他 run 的项目。
        let reclaim = alfred_executor::driver::reclaim_orphaned_project(&orphan_dir);
        match reclaim {
            Ok(fact) => {
                audit(&ctx.run_dir, "orphaned_project_reclaim", &fact)?;
            }
            Err(e) => {
                audit(
                    &ctx.run_dir,
                    "orphaned_project_reclaim",
                    &serde_json::json!({
                        "reclaimed": false,
                        "error": format!("{e:#}"),
                    }),
                )?;
            }
        }
        // done 记录 status="cancelled" = 可捕获中断已完整收尾（shield 清理 +
        // done 证书都在）：非机械失败，不自动重跑（取消不冒充可重试的瞬时
        // 故障）——升级属主，失败原样保留；无 done（SIGKILL 类）维持既有
        // crashed 语义机械重跑/耗尽升级。
        let done_cancelled = alfred_executor::driver::read_done_marker(
            &orphan_dir.join("driver.done.json"),
        )
        .ok()
        .flatten()
        .map(|done| done.status == "cancelled")
        .unwrap_or(false);
        let (failure_status, err) = if done_cancelled {
            (
                None,
                anyhow::anyhow!(
                    "orphaned driver run {} was cancelled (driver done record \
                     carries status 'cancelled'): interrupted execution, not a \
                     mechanical failure",
                    orphan_dir.display()
                ),
            )
        } else {
            (
                Some("crashed"),
                anyhow::anyhow!(
                    "orphaned driver run {}: previous process died mid-poll \
                     (driver launched without done/state record)",
                    orphan_dir.display()
                ),
            )
        };
        let intent = execution_failure_intent(run, &dagspec, &node, failure_status, &err, true);
        commit_intent(run, ctx, intent)?;
        return Ok(());
    }
    run.execution_count += 1;
    let exec_dir = ctx.run_dir.join(format!("exec-{}", run.execution_count));
    // M3：节点执行轨迹——node_started 直写（容器长跑前落盘，中途崩溃审计
    // 可见"开始了没完成"）；node_completed / execution_succeeded 经 effects
    // 通道在 commit 时落（决策纯范式）。
    audit(
        &ctx.run_dir,
        "node_started",
        &serde_json::json!({ "node_id": node.id, "attempt": attempt }),
    )?;
    let opts = RunOptions {
        run_dir: exec_dir.clone(),
        // R6e：执行产物落 run 级单一持久 ws（`<run>/ws`，git 基线），exec-N 只做
        // 记录（driver.py/compose/state.json）不挂产物。M3：全部节点共享此
        // ws——下游节点天然看到上游产物（挂载面不变）。
        workspace_dir: ctx.run_dir.join("ws"),
        image: run.options.image.clone(),
        // 任务环境接线（G1 native_inspect 真实环境）：外层实验传入的原任务
        // compose + 其 SAMPLE_METADATA 插值键（state.json 绑定，续跑/孤儿
        // 恢复同一环境）。None = 内置 network none 单容器（既有行为）。
        env_compose: run.options.env_compose.clone().map(std::path::PathBuf::from),
        env_metadata: run.options.env_metadata.clone(),
        assignment,
        // A：节点契约声明优先（planner 大参考卷按规模声明 / timed_out 自适应
        // 放大写回 run.dagspec），未声明回退治理缺省 exec_time_limit_secs
        // （CLI --time-limit，缺省 600——兼容既有契约）。真源
        // PlanNode::resolved_time_limit_secs。
        time_limit_secs: node.resolved_time_limit_secs(run.options.exec_time_limit_secs),
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
            // M3 per-node 预算：节点完成 → 机械重跑预算重置（下一节点全新
            // 预算；exec 审查侧机械重跑沿用该计数，见 exec_review_step）。
            run.attempts_used = 0;
            run.completed_nodes.push(node.id.clone());
            // v3 分通道收集：单条事件主审计（pre_apply 段）。
            let mut effects = Effects::default();
            effects.audit(
                "node_completed",
                serde_json::json!({
                    "node_id": node.id,
                    "attempt": attempt,
                    "eval_status": outcome.eval_status,
                    "artifact_changes": outcome.artifact.as_ref().map(|a| a.changes.len()),
                }),
            );
            // 全图完成门：依赖序推进无待执行节点 → ExecutionSucceeded（→
            // ExecReviewing；HEAD execution_succeeded 审计形态不变，task_id =
            // 完成全图的节点）。
            let event = if pending_node(&dagspec, &run.completed_nodes)?.is_none() {
                effects.audit(
                    "execution_succeeded",
                    serde_json::json!({
                        "task_id": node.id,
                        "eval_status": outcome.eval_status,
                        "artifact_changes": outcome.artifact.as_ref().map(|a| a.changes.len()),
                    }),
                );
                GovernanceEvent::ExecutionSucceeded
            } else {
                GovernanceEvent::ExecutionNodeCompleted
            };
            let intent = StepIntent::Proceed { event, effects };
            commit_intent(run, ctx, intent)?;
            Ok(())
        }
        Err(e) => {
            // 机械失败判定：driver timed_out / error / crash（读 exec 子 run 的
            // state.json 的 eval_status）。
            let failure_status = exec_failure_status(&exec_dir)?;
            let intent = execution_failure_intent(
                run,
                &dagspec,
                &node,
                failure_status.as_deref(),
                &e,
                false,
            );
            commit_intent(run, ctx, intent)?;
            Ok(())
        }
    }
}

/// G1：exec 子目录的 driver 孤儿签名——审计含 `container_driver_launched`
/// （驱动子进程已 spawn）且无 state.json（execute_run 的任何收尾路径——
/// 成功写 state / 机械失败 fail_run——都会落 state.json）。
///
/// = 上个进程在 poll 中途死亡（TUI 退出收割 / kill -9 alfred）：驱动与容器
/// 成孤儿，done 永不出现；state.json 停 executing，无人推进。已收尾（有
/// state.json）或未启动过驱动（无 launched 审计，如 spawn 前失败）都不是
/// 孤儿——前者已定论，后者无孤儿进程需要接住。
fn orphaned_driver_run(exec_dir: &Path) -> bool {
    if exec_dir.join("state.json").exists() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(exec_dir.join("audit.jsonl")) else {
        return false;
    };
    text.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v["event"].as_str().map(|e| e == "container_driver_launched"))
            .unwrap_or(false)
    })
}

/// 执行失败的路由意图（决策纯——副作用经 [`Effects`] 通道收集，提交统一在
/// [`commit_intent`]）。
///
/// 机械失败（`failure_status = Some`：timed_out / error / crashed，eval_status
/// 读自 exec 子 run 的 state.json，或 G1 孤儿检测的 crashed 语义）→
/// [`route_mechanical_failure`] 路由（timed_out 自适应放大 / error·crashed 同
/// 契约重跑 / 预算耗尽升级）；非机械硬错误（`None`，如非默认沙箱档案）→
/// 升级属主，不悄悄放行。execution_step 的 Err 分支与 G1 孤儿检测共用——
/// 同一失败只走一条路由真源。
///
/// `orphaned` = G1 孤儿路径标记：mechanical_retry 审计带 `orphaned_driver`
/// 字段（与驱动进程内崩溃的 crashed 区分取证）。
fn execution_failure_intent(
    run: &mut GovernanceRun,
    dagspec: &alfred_core::DagSpec,
    node: &alfred_core::PlanNode,
    failure_status: Option<&str>,
    err: &anyhow::Error,
    orphaned: bool,
) -> StepIntent {
    // done=cancelled 是外部中断的如实终态（可捕获中断已走 shield 清理 +
    // done 证书）：与 None 同路升级属主，不自动重跑——取消不冒充可重试的
    // 机械瞬时故障，失败原样保留（audit 的 error 串携带 cancelled 事实）。
    let mechanical_status = failure_status.filter(|s| *s != "cancelled");
    let Some(status) = mechanical_status else {
        // 非机械的硬错误（如非默认沙箱档案 / driver done=cancelled）→ 升级
        // 属主，不悄悄放行。
        let mut effects = Effects::default();
        effects.audit(
            "execution_hard_error_escalated",
            serde_json::json!({
                "node_id": node.id,
                "error": format!("{err:#}"),
            }),
        );
        return StepIntent::Proceed {
            event: GovernanceEvent::ExecutionFailedEscalate,
            effects,
        };
    };
    match route_mechanical_failure(
        status,
        dagspec,
        // M3：放大/重跑落点 = 当前失败节点（多节点图非首节点）。
        &node.id,
        run.options.exec_time_limit_secs,
        run.attempts_used,
        run.mechanical_budget,
    ) {
        MechanicalFailureRouting::Retry {
            attempt,
            amplified_dagspec,
            time_limit_adjusted,
        } => {
            run.attempts_used += 1;
            // HEAD 顺序（execution_step 特有）：apply(ExecutionFailedRetry)
            // 在前 → mechanical_retry 审计在后（audits 通道 post_apply 段
            // 保序）→ println 重跑提示（post_apply_notices 通道）。
            let mut effects = Effects::default();
            // C：放大后的 dagspec 经通道写回（dagspec.json 落盘 +
            // run.dagspec 注入，与 PlanProduced 同机制）——下轮
            // execution_step 用节点新值重新渲染 driver.py（execute_run
            // 每次 exec-N 全新生成）。
            if let Some(dagspec) = amplified_dagspec {
                effects.dagspec = Some(dagspec);
            }
            let mut data = serde_json::json!({
                "node_id": node.id,
                "attempt": attempt,
                "budget": run.mechanical_budget,
                "error": format!("{err:#}"),
                "failure_status": status,
            });
            if orphaned {
                data["orphaned_driver"] = serde_json::json!(true);
            }
            let notice = match time_limit_adjusted {
                Some((from, to)) => {
                    data["time_limit_adjusted"] =
                        serde_json::json!({ "from": from, "to": to });
                    format!(
                        "节点 {} 执行超时（timed_out），自适应放大时间上限重跑（{attempt}/{}，time_limit {from}s→{to}s）：{err}",
                        node.id,
                        run.mechanical_budget
                    )
                }
                None => format!(
                    "节点 {} 执行机械失败，按同一契约重跑（{attempt}/{}）：{err}",
                    node.id,
                    run.mechanical_budget
                ),
            };
            effects.post_apply_audit("mechanical_retry", data);
            // S2b：notice 通道载荷=去前缀正文（前缀由 sink 加回，
            // 见 Effects::post_apply_notice 文档）。
            effects.post_apply_notice(notice);
            StepIntent::Proceed {
                event: GovernanceEvent::ExecutionFailedRetry,
                effects,
            }
        }
        MechanicalFailureRouting::Exhausted {
            final_time_limit_secs,
        } => {
            // 放大后仍耗尽 → 照旧升级；data 带最终预算（属主决策
            // 信息充分：知道系统已把上限抬到哪、仍不够）。
            let mut effects = Effects::default();
            effects.audit(
                "mechanical_budget_exhausted_escalated",
                serde_json::json!({
                    "node_id": node.id,
                    "error": format!("{err:#}"),
                    "failure_status": status,
                    "time_limit_secs": final_time_limit_secs,
                }),
            );
            StepIntent::Proceed {
                event: GovernanceEvent::ExecutionFailedEscalate,
                effects,
            }
        }
    }
}

/// M3：依赖序待执行节点选择（[`alfred_core::DagSpec::next_pending_node`] 单一
/// 真源；坏图随 M1 校验显式 Err 穿出，治理侧不重复报错路径）。返回 owned
/// 节点（TaskAssignment 消费）；`None` = 全图已完成。
fn pending_node(
    dagspec: &alfred_core::DagSpec,
    completed: &[String],
) -> Result<Option<alfred_core::PlanNode>> {
    dagspec
        .next_pending_node(completed)
        .map_err(|e| anyhow::anyhow!(e))
        .map(|node| node.cloned())
}

/// ExecReviewing：执行审查改调宿主 pi reviewer（ws 全量自由读 + 对话记录）→ §3.3 路由。
///
/// R6d：不再读执行容器内嵌 verdict（scorer 已移除，执行容器只出产物）。
/// 执行审查由 `execute_exec_review`（alfred-reviewer）由宿主 pi 判产物 vs 验收
/// 标准——reviewer 全量自由读 run/ws（执行者产物，git 基线），含超过旧 scorer
/// 4000B/文件截断的内容。
/// 离线回退（ALFRED_OFFLINE=1 或 ALFRED_EXEC_REVIEW_OFFLINE=1）：不跑 pi——
/// 执行无审查结论 → 升级属主（§六继承项，不悄悄放行）。
///
/// 磁盘重入（9/3 欠账，用户死锁链修复②）：执行审查输入（契约/挂载语义/ws/
/// 对话记录）全部在磁盘（run 目录 contract.json / dagspec / ws / exec-N），本步
/// 不读任何进程内执行 outcome——进程在 Executing → ExecReviewing 之间崩溃后，
/// 从 state.json=exec_reviewing 重入照常重跑执行审查（R6d 起执行容器不携带审查
/// 结论，outcome 仅是历史跨态约束，已无消费方）。
fn exec_review_step(run: &mut GovernanceRun, ctx: &GovernanceContext) -> Result<()> {
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
        // M4：执行审查输入 = 全图契约——单节点 = 首节点契约（既有形态），多节点
        // = 全节点契约拼接（execute_exec_review / exec_review_inputs 构建
        // dagspec.json 输入：每节点 prompt/验收标准 + sandbox.workspace_subdirs
        // 产物归属 + edges 依赖序），reviewer 一次看全图判"全图执行忠实度"。
        // 旧形态 nodes.first() 对全 ws 判产物 = 多节点漏审下游节点。
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
            &dagspec,
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

    let intent = match verdict {
        Some(v) => {
            // verdict 归档由 commit_intent 的 verdicts 通道执行（分通道 flush）。
            let decision = alfred_core::route(&v).map_err(|e| anyhow::anyhow!(e))?;
            match decision {
                // A3 仅审计不强制处置：非 C verdict 照常归档（verdict 入历史，
                // 原生 route() 结论与判分依据落审计——audit 照写），但 verdict
                // 驱动的指定强制处置（机械重跑 / 机械耗尽升级 / 语义升级 /
                // contract_fault 预标注属主）全部断开——改走合法通过转移
                // （ExecReviewPassed → Completed），执行产物按原样进入最终交付。
                // C（Advance）不属处置，走原路。执行机械失败重跑（execution_step
                // 读执行驱动状态判定）与审查宿主失败升级（Err 降级路径）不在
                // 本档断开——那是错误路径不是审查处置，"escalated 仍是真实
                // 结局"保持。离线回退仍 unscored→升级，不以离线重放冒充在线。
                _ if matches!(run.options.ablation, Some(GovernanceAblation::AuditOnly))
                    && !matches!(
                        decision,
                        alfred_core::RoutingDecision::Advance
                    ) =>
                {
                    let mut effects = Effects::default();
                    effects.verdicts.push(VerdictKind::Exec(v.clone()));
                    effects.audit(
                        "ablation_a3_disposal_disconnected",
                        serde_json::json!({
                            // 审计显式区分：verdict_outcome = 审查真实判分
                            // （I/P）；applied_event 是 A3 消融分支的合法转移，
                            // 不是审查裁决——"review passed"不得被读成原裁判
                            // 结果（Main 硬约束）。
                            "verdict_outcome": format!("{:?}", v.value),
                            "transition_source": "ablation_a3_audit_only",
                            "value": format!("{:?}", v.value),
                            "failure_class": format!("{:?}", v.failure_class),
                            "explanation": v.explanation,
                            "would_route": format!("{:?}", decision),
                            "applied_event": "ExecReviewPassed",
                        }),
                    );
                    StepIntent::Proceed {
                        event: GovernanceEvent::ExecReviewPassed,
                        effects,
                    }
                }
                alfred_core::RoutingDecision::Advance => {
                    let mut effects = Effects::default();
                    effects.verdicts.push(VerdictKind::Exec(v.clone()));
                    effects.audit(
                        "exec_review_passed",
                        serde_json::json!({
                            "value": "C",
                            "explanation": v.explanation,
                        }),
                    );
                    StepIntent::Proceed {
                        event: GovernanceEvent::ExecReviewPassed,
                        effects,
                    }
                }
                alfred_core::RoutingDecision::MechanicalRetry => {
                    if !run.mechanical_exhausted() {
                        run.attempts_used += 1;
                        let mut effects = Effects::default();
                        effects.verdicts.push(VerdictKind::Exec(v.clone()));
                        effects.audit(
                            "mechanical_retry_from_verdict",
                            serde_json::json!({
                                "attempt": run.attempts_used,
                                "budget": run.mechanical_budget,
                            }),
                        );
                        StepIntent::Proceed {
                            event: GovernanceEvent::ExecReviewMechanicalRetry,
                            effects,
                        }
                    } else {
                        let mut effects = Effects::default();
                        effects.verdicts.push(VerdictKind::Exec(v.clone()));
                        effects.audit(
                            "mechanical_budget_exhausted_escalated",
                            serde_json::json!({
                                "value": format!("{:?}", v.value),
                                "failure_class": format!("{:?}", v.failure_class),
                            }),
                        );
                        StepIntent::Proceed {
                            event: GovernanceEvent::ExecReviewMechanicalEscalate,
                            effects,
                        }
                    }
                }
                alfred_core::RoutingDecision::Escalate {
                    suggest_contract_change,
                } => {
                    if suggest_contract_change {
                        orchestrator_notice(
                            ctx,
                            "执行审查 contract_fault：预标注『建议改契约』，升级属主。",
                        );
                    }
                    let mut effects = Effects::default();
                    effects.verdicts.push(VerdictKind::Exec(v.clone()));
                    effects.audit(
                        "exec_review_escalated",
                        serde_json::json!({
                            "value": format!("{:?}", v.value),
                            "failure_class": format!("{:?}", v.failure_class),
                            "suggest_contract_change": suggest_contract_change,
                            "explanation": v.explanation,
                        }),
                    );
                    StepIntent::Proceed {
                        event: GovernanceEvent::ExecReviewSemanticEscalate,
                        effects,
                    }
                }
            }
        }
        None => {
            // §六继承项：执行审查本身出错（unscored / 离线回退）→ 升级，不悄悄放行。
            let mut effects = Effects::default();
            effects.audit(
                "exec_review_error_escalated",
                serde_json::json!({ "reason": unscored_reason }),
            );
            StepIntent::Proceed {
                event: GovernanceEvent::ExecReviewError,
                effects,
            }
        }
    };
    commit_intent(run, ctx, intent)?;
    Ok(())
}

/// 计划/执行审查宿主驱动失败 → 治理降级（9/3 欠账，用户自由使用路径死锁修复）。
///
/// reviewer 宿主 pi 超时/崩溃/产出失败时，`execute_plan_review` /
/// `execute_exec_review` 的失败路径已把 outcome 落盘到审查目录（state.json +
/// verdict.json，eval_status=timed_out/error，容器时代 fail_exec_review 语义）。
/// 本函数在编排层接住穿出的 Err：落升级审计 → apply 升级事件（PlanReviewError /
/// ExecReviewError → Escalated，来源 plan_review/execution）→ persist——run 挂起
/// 属主拍板续跑，绝不 Err 穿出把 run 卡死在流转中间态。
///
/// 升级事件本身失败（审计/转移/persist Err）才向上穿出：磁盘不可写时静默吞掉
/// 等于丢状态，宁可显式失败。
fn review_host_failure_escalate(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    mode: &str,
    e: anyhow::Error,
) -> Result<()> {
    let event = match mode {
        "plan_review" => GovernanceEvent::PlanReviewError,
        "exec_review" => GovernanceEvent::ExecReviewError,
        other => bail!("review_host_failure_escalate: 未知审查模式 {other:?}"),
    };
    // 收表（步骤④）：降级也走 commit_intent 单点——review_host_failure_escalated
    // 审计 → apply 升级事件 → persist（HEAD 提交序列同构；notice 在 persist 后）。
    commit_intent(
        run,
        ctx,
        StepIntent::Escalate {
            event,
            audit_name: "review_host_failure_escalated".into(),
            audit_data: Some(serde_json::json!({
                "mode": mode,
                "error": format!("{e:#}"),
            })),
            reason: format!("{mode}: {e:#}"),
        },
    )?;
    orchestrator_notice(
        ctx,
        &format!(
            "{}审查失败已升级属主（state=Escalated，挂起；审查 outcome 已落盘）。\n\
             \x20 run_dir: {}；等待属主拍板（retry/revise/abandon）。",
            if mode == "plan_review" {
                "计划"
            } else {
                "执行"
            },
            ctx.run_dir.display()
        ),
    );
    Ok(())
}

/// 读 exec 子 run 的 state.json 的失败形态（C：细分 timed_out vs error/crashed）。
///
/// `Some(status)` = 机械失败（eval_status 即容器驱动状态：timed_out / error /
/// crashed，!= success）；`None` = 无 state.json（execute_run 写盘前硬失败，
/// 非机械）或 success。
fn exec_failure_status(exec_dir: &Path) -> Result<Option<String>> {
    let path = exec_dir.join("state.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        // 无 state.json = execute_run 在写盘前就硬失败（非机械）。
        return Ok(None);
    };
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let status = v["run"]["eval_status"].as_str().unwrap_or("success");
    Ok((status != "success").then(|| status.to_string()))
}

// ---- C：timed_out 自适应重跑（治理层修复） ----

/// timed_out 重跑时间上限放大上限（秒）：×2 放大不越过此值。
const EXEC_TIME_LIMIT_AMPLIFY_CAP_SECS: u32 = 3600;

/// 机械失败的路由决策（C：timed_out 与 error/crashed 分流）。
#[derive(Debug, Clone, PartialEq)]
enum MechanicalFailureRouting {
    /// 预算内重跑。`amplified_dagspec` = 时间上限已放大的节点计划（timed_out
    /// 且未到放大上限时 Some，经 effects.dagspec 通道写回 dagspec.json +
    /// run.dagspec；error/crashed / 已到上限 = None，同契约原样重跑）；
    /// `time_limit_adjusted` = (from, to) 放大轨迹（audit 用）。
    Retry {
        attempt: u32,
        amplified_dagspec: Option<alfred_core::DagSpec>,
        time_limit_adjusted: Option<(u32, u32)>,
    },
    /// 预算耗尽 → 升级属主。`final_time_limit_secs` = 最终生效预算（含历次
    /// 放大；属主决策信息充分）。
    Exhausted { final_time_limit_secs: u32 },
}

/// 机械失败重跑决策（纯函数，execution_step 消费）。
///
/// timed_out：预算硬死线是根因——同死线重跑必再超，**当前失败节点**
/// （`failing_node_id`，M3 多节点图非首节点）的时间上限自适应放大（×2，
/// cap [`EXEC_TIME_LIMIT_AMPLIFY_CAP_SECS`]）写回 dagspec 后重跑；
/// error/crashed：与时间预算无关，同契约原样重跑。预算耗尽（attempts_used ≥
/// mechanical_budget，镜像 [`GovernanceRun::mechanical_exhausted`]）→ 升级，
/// 带最终预算。
fn route_mechanical_failure(
    failure_status: &str,
    dagspec: &alfred_core::DagSpec,
    failing_node_id: &str,
    exec_time_limit_secs: u32,
    attempts_used: u32,
    mechanical_budget: u32,
) -> MechanicalFailureRouting {
    let current = dagspec
        .nodes
        .iter()
        .find(|n| n.id == failing_node_id)
        .map(|n| n.resolved_time_limit_secs(exec_time_limit_secs))
        .unwrap_or(exec_time_limit_secs);
    if attempts_used >= mechanical_budget {
        return MechanicalFailureRouting::Exhausted {
            final_time_limit_secs: current,
        };
    }
    let mut amplified_dagspec = None;
    let mut time_limit_adjusted = None;
    if failure_status == "timed_out" {
        let to = (current.saturating_mul(2)).min(EXEC_TIME_LIMIT_AMPLIFY_CAP_SECS);
        if to > current {
            let mut amplified = dagspec.clone();
            // 放大写回失败节点（找到才落 amplified——与 time_limit_adjusted
            // 轨迹一致，不出现"声明调整了却没写进"的失配）。
            if let Some(node) = amplified.nodes.iter_mut().find(|n| n.id == failing_node_id) {
                node.time_limit_secs = Some(to);
                amplified_dagspec = Some(amplified);
                time_limit_adjusted = Some((current, to));
            }
        }
    }
    MechanicalFailureRouting::Retry {
        attempt: attempts_used + 1,
        amplified_dagspec,
        time_limit_adjusted,
    }
}

/// 落盘 dagspec.json。
pub(crate) fn write_dagspec(run_dir: &Path, dagspec: &alfred_core::DagSpec) -> Result<()> {
    let text = serde_json::to_string_pretty(dagspec).context("serialize dagspec")?;
    std::fs::write(run_dir.join("dagspec.json"), text).context("write dagspec.json")
}

/// 落盘 run 级契约 contract.json（矩阵 §1.1 第 7 行：planner 挂"自己写的契约" ro 回看）。
///
/// 投影真源 = [`alfred_core::DagSpec::contract_json`]（首节点契约全字段，与
/// reviewer 输入 contract.json 同源）。dagspec 每次落定即重写——重规划轮 planner
/// 读到的恒为最新一轮自己写的契约。
pub(crate) fn write_run_contract(run_dir: &Path, dagspec: &alfred_core::DagSpec) -> Result<()> {
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
    atomic_write(&run_dir.join(name), &text).with_context(|| format!("write {name}"))
}

/// 原子落盘（temp + rename，S4）：写同目录 `<file>.tmp.<pid>` 后 rename 覆盖
/// ——读端（state.json 断点 reload / 看板轮询 / reviewer 挂载）恒见完整旧值
/// 或完整新值，绝无半截文件窗口。对冲存量撕裂窗口：TUI 退出不 join 治理
/// worker（P3 语义，进程退出即收割），非原子写在击杀瞬间可留半截
/// state.json——reload 失败 = run 不可续（会话死锁）。tmp 名带 pid：两个
/// 写者（跨进程同 run）各写各的 tmp，rename 原子性不被共享 tmp 破坏。
fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .with_context(|| format!("atomic write 路径无名 {}", path.display()))?;
    let tmp = path.with_file_name(format!("{file_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {} → {}", tmp.display(), path.display()))
}

/// 把 converse 产出的 DagSpec 格式化为语义回复（对话记录 converse.reply 轮的 content）。
///
/// M4-a：conversation.json 只承载 owner↔planner 语义轮次——落计划摘要，不落
/// 原始建图指令 JSON（中间指令属实现细节，已在 llm-calls/ 审计，避免冗余）。
pub(crate) fn format_plan_reply(dagspec: &alfred_core::DagSpec) -> String {
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
/// exec-verdicts.json，矩阵 §1.1 第 8 行 reviewer 挂载输入）。全量重写走
/// [`atomic_write`]（S4 原子写）——TUI 退出击杀 worker 的撕裂窗口对冲。
pub fn persist_governance_run(run_dir: &Path, run: &GovernanceRun) -> Result<()> {
    let text = serde_json::to_string_pretty(run).context("serialize governance state")?;
    atomic_write(&run_dir.join("state.json"), &text).context("write governance state.json")?;
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
        // S2b：driver / REPL 面恒 None（通知打终端）；TUI 由 chat.rs 会话侧接入。
        events: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::{Contract, DagSpec, PlanNode};

    fn temp_exec_dir(tag: &str, state_json: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "alfred-exec-failure-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = state_json {
            std::fs::write(dir.join("state.json"), text).unwrap();
        }
        dir
    }

    fn single_node_dagspec(time_limit_secs: Option<u32>) -> DagSpec {
        let contract = Contract {
            prompt: "p".into(),
            acceptance_criteria: "a".into(),
            reviewer_models: vec![],
        };
        let mut node = PlanNode::new("task-1", "s", contract);
        node.time_limit_secs = time_limit_secs;
        DagSpec::new("req-1", vec![node])
    }

    #[test]
    fn exec_failure_status_distinguishes_timed_out_error_and_success() {
        // C：细分机械失败形态（timed_out / error / crashed = Some；success /
        // 无 state.json = None 非机械）。
        let mk = |status: &str| {
            format!(
                r#"{{"run": {{"run_id": "exec-1", "eval_status": "{status}"}}}}"#
            )
        };
        let dir = temp_exec_dir("timed-out", Some(&mk("timed_out")));
        assert_eq!(
            exec_failure_status(&dir).unwrap().as_deref(),
            Some("timed_out")
        );
        let dir = temp_exec_dir("error", Some(&mk("error")));
        assert_eq!(exec_failure_status(&dir).unwrap().as_deref(), Some("error"));
        let dir = temp_exec_dir("crashed", Some(&mk("crashed")));
        assert_eq!(
            exec_failure_status(&dir).unwrap().as_deref(),
            Some("crashed")
        );
        let dir = temp_exec_dir("success", Some(&mk("success")));
        assert_eq!(exec_failure_status(&dir).unwrap(), None);
        let dir = temp_exec_dir("missing", None);
        assert_eq!(exec_failure_status(&dir).unwrap(), None);
    }

    #[test]
    fn timeout_retry_amplifies_time_limit() {
        // C：timed_out ×2 放大（cap 3600）——600→1200、2400→3600（4800 截到
        // cap）、声明 1800→3600；dagspec 节点字段被写回（放大落点）。
        let dag = single_node_dagspec(None);
        let r = route_mechanical_failure("timed_out", &dag, "task-1", 600, 0, 2);
        match &r {
            MechanicalFailureRouting::Retry {
                attempt,
                amplified_dagspec,
                time_limit_adjusted,
            } => {
                assert_eq!(*attempt, 1);
                assert_eq!(*time_limit_adjusted, Some((600, 1200)));
                assert_eq!(
                    amplified_dagspec.as_ref().unwrap().nodes[0].time_limit_secs,
                    Some(1200)
                );
            }
            other => panic!("expected Retry, got {other:?}"),
        }

        let dag = single_node_dagspec(Some(2400));
        let r = route_mechanical_failure("timed_out", &dag, "task-1", 600, 1, 2);
        match &r {
            MechanicalFailureRouting::Retry {
                time_limit_adjusted, ..
            } => assert_eq!(*time_limit_adjusted, Some((2400, 3600))),
            other => panic!("expected Retry, got {other:?}"),
        }

        let dag = single_node_dagspec(Some(1800));
        let r = route_mechanical_failure("timed_out", &dag, "task-1", 600, 0, 2);
        match &r {
            MechanicalFailureRouting::Retry {
                time_limit_adjusted, ..
            } => assert_eq!(*time_limit_adjusted, Some((1800, 3600))),
            other => panic!("expected Retry, got {other:?}"),
        }

        // 已到 cap：无法再放大 → 同契约原样重跑（无放大轨迹）。
        let dag = single_node_dagspec(Some(3600));
        let r = route_mechanical_failure("timed_out", &dag, "task-1", 600, 0, 2);
        match &r {
            MechanicalFailureRouting::Retry {
                amplified_dagspec,
                time_limit_adjusted,
                ..
            } => {
                assert_eq!(*time_limit_adjusted, None);
                assert!(amplified_dagspec.is_none());
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn error_and_crash_retry_same_contract() {
        // C：error/crashed 与时间预算无关 → 同契约重跑（无放大、无 dagspec 写回）。
        for status in ["error", "crashed"] {
            let dag = single_node_dagspec(None);
            let r = route_mechanical_failure(status, &dag, "task-1", 600, 0, 2);
            match &r {
                MechanicalFailureRouting::Retry {
                    amplified_dagspec,
                    time_limit_adjusted,
                    ..
                } => {
                    assert_eq!(*time_limit_adjusted, None, "status={status}");
                    assert!(amplified_dagspec.is_none(), "status={status}");
                }
                other => panic!("expected Retry, got {other:?}"),
            }
        }
    }

    #[test]
    fn exhausted_escalation_carries_final_budget() {
        // C：放大后仍耗尽 → 升级带最终预算（含历次放大；未放大 = 治理缺省）。
        let dag = single_node_dagspec(Some(2400));
        let r = route_mechanical_failure("timed_out", &dag, "task-1", 600, 2, 2);
        assert_eq!(
            r,
            MechanicalFailureRouting::Exhausted {
                final_time_limit_secs: 2400
            }
        );

        let dag = single_node_dagspec(None);
        let r = route_mechanical_failure("timed_out", &dag, "task-1", 600, 2, 2);
        assert_eq!(
            r,
            MechanicalFailureRouting::Exhausted {
                final_time_limit_secs: 600
            }
        );
    }

    // ---------- M3 多节点：拓扑序调度 / 断点续跑 / 单节点等价 ----------

    fn dag_node(id: &str) -> PlanNode {
        PlanNode::new(
            id,
            "s",
            Contract {
                prompt: "p".into(),
                acceptance_criteria: "a".into(),
                reviewer_models: vec![],
            },
        )
    }

    fn dag_edge(from: &str, to: &str) -> alfred_core::Edge {
        alfred_core::Edge {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn multi_node_advances_in_topological_order_and_resumes() {
        // M3：2 节点顺序执行——依赖序推进（b 声明在前但依赖 a → 先 a）；
        // 断点恢复（completed=[a] → 从 b 续，不重跑 a）；全图完成门（[a,b]
        // → None → ExecutionSucceeded 路径）。
        let mut dag = DagSpec::new("req-1", vec![dag_node("b"), dag_node("a")]);
        dag.edges = vec![dag_edge("a", "b")];
        assert_eq!(pending_node(&dag, &[]).unwrap().unwrap().id, "a");
        // 断点恢复：首节点已持久完成 → 直接取第二节点。
        assert_eq!(
            pending_node(&dag, &["a".to_string()]).unwrap().unwrap().id,
            "b"
        );
        assert!(pending_node(&dag, &["a".to_string(), "b".to_string()])
            .unwrap()
            .is_none());

        // 无 edges 的多节点（孤立节点合法）：按声明序推进。
        let dag = DagSpec::new("req-1", vec![dag_node("x"), dag_node("y")]);
        assert_eq!(pending_node(&dag, &[]).unwrap().unwrap().id, "x");
        assert_eq!(
            pending_node(&dag, &["x".to_string()]).unwrap().unwrap().id,
            "y"
        );
        assert!(pending_node(&dag, &["y".to_string(), "x".to_string()])
            .unwrap()
            .is_none());
    }

    #[test]
    fn single_node_advancement_matches_head() {
        // M3：单节点 dagspec 推进行为与 HEAD 等价——空完成集 → 唯一节点；
        // 完成 → 全图完成门即开（一轮执行即 ExecutionSucceeded，无自环轮）。
        let dag = single_node_dagspec(None);
        assert_eq!(pending_node(&dag, &[]).unwrap().unwrap().id, "task-1");
        assert!(pending_node(&dag, &["task-1".to_string()])
            .unwrap()
            .is_none());
    }

    #[test]
    fn cyclic_dagspec_rejected_via_m1_single_truth() {
        // M3：环拒绝走 M1 `topological_order` 单一真源（治理侧不重复报错
        // 路径）——execution_step 入口经 pending_node 显式 Err 穿出，不静默
        // 截断。
        let mut dag = DagSpec::new("req-1", vec![dag_node("a"), dag_node("b")]);
        dag.edges = vec![dag_edge("a", "b"), dag_edge("b", "a")];
        let err = pending_node(&dag, &[]).unwrap_err();
        assert!(err.to_string().contains("cycle"), "err = {err:#}");

        // 重复节点 id：同 id 节点无法区分（id 键控完成记账），M1 校验拒绝。
        let dag = DagSpec::new("req-1", vec![dag_node("dup"), dag_node("dup")]);
        let err = pending_node(&dag, &[]).unwrap_err();
        assert!(
            err.to_string().contains("duplicate node id 'dup'"),
            "err = {err:#}"
        );
    }

    #[test]
    fn timeout_retry_amplifies_failing_node_not_first() {
        // M3：多节点图 timed_out 放大写回**当前失败节点**（非首节点）——
        // 声明序 [a, b]、b 失败：b 放大 600→1200，a 不动。
        let mut dag = DagSpec::new("req-1", vec![dag_node("a"), dag_node("b")]);
        dag.edges = vec![dag_edge("a", "b")];
        let time_limit = |dag: &DagSpec, id: &str| {
            dag.nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .time_limit_secs
        };
        let r = route_mechanical_failure("timed_out", &dag, "b", 600, 0, 2);
        match &r {
            MechanicalFailureRouting::Retry {
                amplified_dagspec,
                time_limit_adjusted,
                ..
            } => {
                assert_eq!(*time_limit_adjusted, Some((600, 1200)));
                let amplified = amplified_dagspec.as_ref().unwrap();
                assert_eq!(time_limit(amplified, "a"), None);
                assert_eq!(time_limit(amplified, "b"), Some(1200));
            }
            other => panic!("expected Retry, got {other:?}"),
        }

        // 失败的是首节点 a：只放大 a，b 不动。
        let r = route_mechanical_failure("timed_out", &dag, "a", 600, 0, 2);
        match &r {
            MechanicalFailureRouting::Retry {
                amplified_dagspec, ..
            } => {
                let amplified = amplified_dagspec.as_ref().unwrap();
                assert_eq!(time_limit(amplified, "a"), Some(1200));
                assert_eq!(time_limit(amplified, "b"), None);
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    /// S4 原子写：persist 落盘完整可读回（断点续跑 reload 路径）+ 无 temp
    /// 残留（半截内容只存在于 `.tmp.<pid>`，rename 后消失）+ 覆写完整替换。
    /// 撕裂窗口本身（击杀在写中途）无法确定性复现——锁定的是"完成态契约"：
    /// 任何完成 persist 的 run 目录，state.json / verdict 投影恒完整可解析。
    #[test]
    fn persist_state_and_verdicts_atomic_no_torn_window() {
        let dir = temp_exec_dir("persist-atomic", None);
        let request = OwnerRequest::new("req-atomic", "标题", "描述", "验收");
        let run = GovernanceRun::new("run-atomic", request, GovernanceOptions::default());
        persist_governance_run(&dir, &run).unwrap();

        // 三文件完整可读回（load = 会话 reload 单一真源路径）。
        assert!(load_governance_run(&dir).is_ok(), "state.json 原子落盘可读回");
        for name in [PLAN_VERDICTS_FILE, EXEC_VERDICTS_FILE] {
            let text = std::fs::read_to_string(dir.join(name)).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), serde_json::json!([]),
                "{name} 投影完整可解析");
        }

        // 无 temp 残留（rename 完成态）。
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "原子写完成态无 temp 残留：{leftovers:?}");

        // 覆写完整替换（第二次 persist 后仍完整——旧值不与新值拼接）。
        persist_governance_run(&dir, &run).unwrap();
        assert!(load_governance_run(&dir).is_ok(), "覆写后仍完整可读回");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---------- G1：driver 孤儿检测（进程死亡无 done → 机械失败路由） ----------

    /// 造一个"驱动已启动未收尾"的孤儿 exec 目录（run_started + launched 审计、
    /// 无 state.json——上个进程 poll 中途死亡的落盘形态）。
    fn orphan_exec_dir(run_dir: &Path, n: u32) -> PathBuf {
        let dir = run_dir.join(format!("exec-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("audit.jsonl"),
            concat!(
                r#"{"data":{"run_id":"exec-N","task_id":"task-1"},"event":"run_started","ts":"t"}"#,
                "\n",
                r#"{"data":{"pid":123,"done_marker":"x"},"event":"container_driver_launched","ts":"t"}"#,
                "\n",
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn orphan_signature_is_launched_without_state_json() {
        // G1 孤儿签名 = container_driver_launched 审计 + 无 state.json（任何
        // 收尾路径都落 state.json）。已收尾 / 未启动过驱动 / 目录不存在 都不是
        // 孤儿——前者已定论，后者无孤儿进程要接住（重跑即覆写，HEAD 语义）。
        let dir = temp_exec_dir("g1-signature", None);

        // 驱动已启动未收尾 → 孤儿。
        let orphan = orphan_exec_dir(&dir, 1);
        assert!(orphaned_driver_run(&orphan), "launched + 无 state.json = 孤儿");

        // 已收尾（state.json 在，无论成败）→ 非孤儿。
        let concluded = orphan_exec_dir(&dir, 2);
        std::fs::write(
            concluded.join("state.json"),
            r#"{"run": {"eval_status": "crashed"}}"#,
        )
        .unwrap();
        assert!(
            !orphaned_driver_run(&concluded),
            "launched + state.json = 已收尾，非孤儿"
        );

        // spawn 前失败（无 launched 审计，如 compose 生成失败）→ 非孤儿
        //（无驱动进程/容器需要接住，走既有硬错误路径）。
        let pre_spawn = dir.join("exec-3");
        std::fs::create_dir_all(&pre_spawn).unwrap();
        std::fs::write(
            pre_spawn.join("audit.jsonl"),
            r#"{"data":{"run_id":"exec-3"},"event":"run_started","ts":"t"}"#,
        )
        .unwrap();
        assert!(
            !orphaned_driver_run(&pre_spawn),
            "无 launched 审计 = 未启动驱动，非孤儿"
        );

        // 目录不存在（全新 run / 全部已收尾）→ 非孤儿。
        assert!(!orphaned_driver_run(&dir.join("exec-99")), "目录不存在 = 非孤儿");

        // done.json 在而 state.json 缺（进程死在 done 落盘与收尾之间）→ 仍按
        // 孤儿接住：outcome 未采集未提交，同按 crashed 重跑（宁可重跑不丢账）。
        let done_orphan = orphan_exec_dir(&dir, 4);
        std::fs::write(
            done_orphan.join("driver.done.json"),
            r#"{"event":"done","status":"success"}"#,
        )
        .unwrap();
        assert!(
            orphaned_driver_run(&done_orphan),
            "done 在但未收尾（无 state.json）= 孤儿"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Executing 态 run + 治理上下文（execution_step 黑盒驱动的最小装配——
    /// 模型字段不被孤儿路径消费，占位即可）。
    fn run_in_executing(tag: &str) -> (GovernanceRun, GovernanceContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "alfred-g1-{tag}-{}",
            alfred_core::util::short_id("t")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut run = GovernanceRun::new(
            "run-g1",
            OwnerRequest::new("req-g1", "标题", "描述", "验收"),
            GovernanceOptions::default(),
        );
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        run.apply(GovernanceEvent::PlanReviewPassed).unwrap();
        run.dagspec = Some(single_node_dagspec(None));
        let model = ExecutorModel {
            provider: "p".into(),
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 8192,
            context_window: None,
            raw_id: true,
        };
        let ctx = GovernanceContext {
            run_dir: dir.clone(),
            planner_model: model.clone(),
            executor_model: model.clone(),
            reviewer_model: model,
            append_system_prompt: String::new(),
            events: None,
        };
        (run, ctx, dir)
    }

    fn run_audit_events(dir: &Path) -> Vec<(String, Value)> {
        std::fs::read_to_string(dir.join("audit.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (v["event"].as_str().unwrap().to_string(), v["data"].clone())
            })
            .collect()
    }

    #[test]
    fn orphaned_driver_routes_mechanical_retry_and_consumes_exec_index() {
        // G1 主路径：续跑进程在 execution_step 检出孤儿 exec 目录（上个进程
        // poll 中途死亡、驱动无 done）→ 按 crashed 语义走既有 mechanical_retry
        // 路由——孤儿尝试计入预算、状态自环 Executing 续跑、孤儿目录序号消费
        // （重跑落新目录，取证面不被覆写）、persist 可读回（断点续跑契约）。
        // 孤儿路径在 execute_run 之前短路——本测试不真起容器。
        let (mut run, ctx, dir) = run_in_executing("retry");
        let orphan = orphan_exec_dir(&dir, 1); // execution_count=0 → 下一目录 exec-1
        let orphan_audit_before = std::fs::read_to_string(orphan.join("audit.jsonl")).unwrap();

        execution_step(&mut run, &ctx).unwrap();

        // 自环 Executing：机械重跑已记账（孤儿尝试 = 一次机械失败）。
        assert_eq!(run.state(), GovernanceState::Executing);
        assert_eq!(run.attempts_used, 1, "孤儿尝试计入机械预算");
        assert_eq!(run.execution_count, 1, "孤儿目录序号已消费（重跑落 exec-2）");
        // mechanical_retry 审计：crashed + orphaned_driver 标记（与进程内崩溃
        // 的 crashed 区分取证）。
        let events = run_audit_events(&dir);
        let retry = events
            .iter()
            .find(|(name, _)| name == "mechanical_retry")
            .expect("mechanical_retry audit");
        assert_eq!(retry.1["failure_status"], "crashed");
        assert_eq!(retry.1["orphaned_driver"], true);
        assert_eq!(retry.1["node_id"], "task-1");
        assert_eq!(retry.1["attempt"], 1);
        // 孤儿目录不被覆写（无 state.json 写入、审计不变）。
        assert!(!orphan.join("state.json").exists(), "孤儿目录不被覆写");
        assert_eq!(
            std::fs::read_to_string(orphan.join("audit.jsonl")).unwrap(),
            orphan_audit_before,
            "孤儿目录审计痕迹保持原样"
        );
        // persist 落盘可读回（崩溃恢复 / TUI resume 契约）。
        let reloaded = load_governance_run(&dir).unwrap();
        assert_eq!(reloaded.state(), GovernanceState::Executing);
        assert_eq!(reloaded.attempts_used, 1);
        assert_eq!(reloaded.execution_count, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn orphaned_driver_exhausts_budget_and_escalates() {
        // G1 耗尽路径：孤儿反复出现（环境性 kill -9）→ 预算耗尽走既有升级
        // （Escalated + mechanical_budget_exhausted_escalated），不无限重跑。
        let (mut run, ctx, dir) = run_in_executing("exhausted");

        orphan_exec_dir(&dir, 1);
        execution_step(&mut run, &ctx).unwrap(); // 失败 1 → 重跑（attempt 1/2）
        assert_eq!(run.state(), GovernanceState::Executing);
        orphan_exec_dir(&dir, 2);
        execution_step(&mut run, &ctx).unwrap(); // 失败 2 → 重跑（attempt 2/2）
        assert_eq!(run.state(), GovernanceState::Executing);
        orphan_exec_dir(&dir, 3);
        execution_step(&mut run, &ctx).unwrap(); // 失败 3 → 预算耗尽 → 升级
        assert_eq!(
            run.state(),
            GovernanceState::Escalated,
            "孤儿耗尽机械预算 → 升级属主"
        );
        assert_eq!(run.attempts_used, 2);
        let events = run_audit_events(&dir);
        assert!(
            events
                .iter()
                .any(|(name, data)| name == "mechanical_budget_exhausted_escalated"
                    && data["failure_status"] == "crashed"),
            "耗尽升级审计带 crashed 失败形态"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn orphaned_driver_cancelled_done_escalates_without_retry() {
        // G1 可捕获中断的恢复语义：孤儿目录带 done(status="cancelled",
        // cleanup_status="released")（方法层取消向原生子树发 SIGTERM → 驱动
        // 走既有 shield 清理 + done 证书）= 外部中断已如实收尾——不是可重试
        // 的机械瞬时故障：升级属主、失败原样保留，不自动重跑已取消的执行。
        // 资源回收按 done 证书短路（本测试无 driver.project.json：证书即
        // released 事实，审计如实记录，不猜项目名）。
        let (mut run, ctx, dir) = run_in_executing("cancelled-done");
        let orphan = orphan_exec_dir(&dir, 1); // execution_count=0 → 下一目录 exec-1
        std::fs::write(
            orphan.join("driver.done.json"),
            r#"{"event":"done","status":"cancelled","cleanup_status":"released"}"#,
        )
        .unwrap();

        execution_step(&mut run, &ctx).unwrap();

        assert_eq!(
            run.state(),
            GovernanceState::Escalated,
            "cancelled 孤儿 → 升级属主，不自动重跑"
        );
        assert_eq!(run.attempts_used, 0, "外部取消不计入机械预算");
        let events = run_audit_events(&dir);
        assert!(
            events
                .iter()
                .any(|(name, data)| name == "execution_hard_error_escalated"
                    && data["error"].as_str().unwrap().contains("cancelled")),
            "升级审计携带 cancelled 事实"
        );
        assert!(
            events
                .iter()
                .any(|(name, data)| name == "orphaned_project_reclaim"
                    && data["reason"]
                        .as_str()
                        .unwrap()
                        .contains("certifies cleanup released")),
            "回收审计如实记录 done 证书（不重复 down）"
        );
        assert!(
            !events.iter().any(|(name, _)| name == "mechanical_retry"),
            "取消不触发机械重跑"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }


    #[test]
    fn timed_out_amplification_reaches_next_attempt_render_input() {
        // G2 回归锁（放大值透传链）：timed_out 机械重跑的放大值必须一路传到
        // 下一轮 execute_run 的渲染输入——放大只落 dagspec 一处（run.dagspec
        // 单一真源），execution_step 取 node.resolved_time_limit_secs 渲染
        // driver.py。链路：route_mechanical_failure 放大 →
        // execution_failure_intent（effects.dagspec 通道）→ commit_intent flush
        //（dagspec.json 落盘 + run.dagspec 注入）→ 下一轮 pending_node →
        // resolved_time_limit_secs = 放大值（跨进程 reload 同值）。真跑渲染
        // 断言（exec-N/driver.py 渲染放大后的 TIME_LIMIT）由 e2e r3 case2 覆盖。
        let (mut run, ctx, dir) = run_in_executing("g2-amplify");
        let dag = run.dagspec.clone().unwrap(); // task-1 未声明 → 治理缺省 600
        let node = dag.nodes[0].clone();

        // exec-1 timed_out 失败的路由（execute_run Err 分支同构——真源
        // execution_failure_intent，非重抄）。
        let err = anyhow::anyhow!("container driver timed out after 600s");
        let intent =
            execution_failure_intent(&mut run, &dag, &node, Some("timed_out"), &err, false);
        commit_intent(&mut run, &ctx, intent).unwrap();

        // 写回双落点：dagspec.json 磁盘 + run.dagspec 进程内均为放大值。
        let on_disk: DagSpec =
            serde_json::from_str(&std::fs::read_to_string(dir.join("dagspec.json")).unwrap())
                .unwrap();
        assert_eq!(
            on_disk.nodes[0].time_limit_secs,
            Some(1200),
            "dagspec.json 落盘放大值"
        );
        assert_eq!(
            run.dagspec.as_ref().unwrap().nodes[0].time_limit_secs,
            Some(1200),
            "run.dagspec 注入放大值"
        );

        // 下一轮 execution_step 的渲染输入（pending_node → resolved，即
        // RunOptions.time_limit_secs 的唯一来源）= 放大值，不回落治理缺省 600。
        let next = pending_node(run.dagspec.as_ref().unwrap(), &run.completed_nodes)
            .unwrap()
            .unwrap();
        assert_eq!(
            next.resolved_time_limit_secs(run.options.exec_time_limit_secs),
            1200,
            "下轮渲染输入必须取放大值（单一真源 run.dagspec）"
        );
        // 跨进程面：persist → reload 后渲染输入同值（TUI/chat resume 路径）。
        let reloaded = load_governance_run(&dir).unwrap();
        let next = pending_node(
            reloaded.dagspec.as_ref().unwrap(),
            &reloaded.completed_nodes,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            next.resolved_time_limit_secs(reloaded.options.exec_time_limit_secs),
            1200,
            "reload 后渲染输入同值"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
