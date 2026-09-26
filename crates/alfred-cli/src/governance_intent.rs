//! StepIntent — 治理环 step 函数的意图描述 + 单点提交序列 `commit_intent`。
//!
//! 重构方案（.plans/重构方案-驱动层.md）步骤②/③/④ 的核心抽象：step 函数
//! （planning/plan_review/execution/exec_review）不再各自内联"副作用 → apply →
//! persist"，而是**纯决策**：读磁盘输入 → LLM 驱动（产物由驱动函数落盘）→
//! 返回意图 + [`Effects`] 分通道副作用收集器（v3 修订，属主 9/7 拍板：每类
//! 副作用独立通道 monad 管理——"决策纯"：step 只收集不执行）。由本模块
//! [`commit_intent`] 统一 flush：
//!
//! ```text
//! commit_intent(run, ctx, intent)：
//!   flush ①（apply 前，HEAD 副作用序，各通道独立函数）
//!     verdicts 入 run 数组 → audits(转移前) 按序 emit → turns 落盘
//!       → dagspec 落盘+注入 → maintain_triggers 执行（LLM 调用；
//!         maintain_done 审计自然交织在收集审计之后，失败 audit 经治理
//!         降级的新一轮 commit_intent 进 audits 通道保交织序）
//!   2. run.apply(event)（Reply 停驻无事件）
//!   flush ②（apply 后）：audits(转移后) 按序 emit → persist → notices
//!     （经 governance::orchestrator_notice 单一出口：REPL 打终端 / TUI 事件化）
//! ```
//!
//! 副作用失败（维护者停摆等）→ `Err` 穿出 → `run_governance_loop` 治理降级
//! （9265791 统一出口），不静默跳过。行为等价由 tests/e2e/equiv.sh 判定
//! （5 场景 canonical 比对 + HEAD 事件-副作用断言表）。
//!
//! **审计名的单一真源**：同一状态机事件可对应多个审计名（如
//! `ExecutionFailedEscalate` → `mechanical_budget_exhausted_escalated` 或
//! `execution_hard_error_escalated`，按机械/硬错误分支二选一）——所以
//! [`StepIntent::Proceed`] 由 step 显式收集审计名 + data，不按事件名推导。
use crate::governance::{
    audit, maintain_after_converse, maintain_after_plan_review, orchestrator_notice,
    persist_governance_run, write_dagspec, GovernanceContext,
};
use alfred_core::conversation::{append_to_disk, ConversationRole, ConversationSource};
use alfred_core::governance::{GovernanceEvent, GovernanceRun};
use alfred_core::{DagSpec, PlanVerdict};
use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::path::Path;

/// step 函数产出的意图（v3 修订：决策纯——step 只收集 [`Effects`]，flush
/// 统一在 [`commit_intent`]）。
#[derive(Debug, Clone)]
pub enum StepIntent {
    /// 正常转移：flush 副作用通道 → apply(event) → persist。
    Proceed {
        event: GovernanceEvent,
        effects: Effects,
    },
    /// Reply 停驻（§2.4 答复分支）：flush 副作用通道（conversation 轮 + 审计 +
    /// ConverseDone 维护）后**无事件**——state 停留 Planning，治理环返回调用方
    /// （text 呈现给属主）。
    Reply { text: String, effects: Effects },
    /// 治理降级（§六继承项）：step 宿主驱动失败，fail_* outcome 已由驱动落盘。
    /// `audit_name` 承载降级审计名（planning_error_escalated /
    /// plan_review_error_escalated / review_host_failure_escalated / …）。
    /// 注意：PlanningError 走 loop 内联 return 路径（HEAD 语义），由调用方区分。
    Escalate {
        event: GovernanceEvent,
        audit_name: String,
        /// 审计 data。`None` = 默认 `{ "error": reason }`（HEAD planning_error_
        /// escalated 单字段形态）；`Some(v)` = 显式 data
        /// （review_host_failure_escalated 的 `{mode, error}` 双字段形态）。
        audit_data: Option<Value>,
        /// 错误文本（data 未显式给时落 `"error"` 键）。
        reason: String,
    },
}

/// step 的分通道副作用收集器（v3 修订，属主 9/7 拍板：每类副作用独立通道
/// monad 管理——决策纯化）。新增副作用类型 = 加通道字段 + flush 分支，不碰
/// 其它通道（"改不动"的结构解药）。
#[derive(Debug, Clone, Default)]
pub struct Effects {
    /// **审计通道（有序 Writer——顺序是唯一不可动不变式）**：flush 按收集序
    /// 逐条 emit。带时点分节：`pre_apply` 段在 apply 前落、`post_apply` 段在
    /// apply 后落（HEAD 少数派顺序：`mechanical_retry` 在 apply/attempts 调整
    /// 之后——段内保收集序，段间由 [`commit_intent`] 固定 pre→post）。
    pub audits: EffectsAudits,
    /// 审查结论归档通道（plan/exec verdict 各自入队，flush 时 push 进 run 历史
    /// ——apply 前生效，HEAD 顺序）。
    pub verdicts: Vec<VerdictKind>,
    /// conversation 轮通道（flush 按收集序落 conversation.json）。
    pub conversation_turns: Vec<ConversationTurnSpec>,
    /// 建图产物通道（PlanProduced 路径）：dagspec 落盘 + run.dagspec 注入
    /// （reviewer_models 注入后的最终形态）——apply 前生效（HEAD 顺序）。
    pub dagspec: Option<DagSpec>,
    /// 维护触发通道：收集指令延迟执行（flush 时才跑维护者 LLM 调用，apply 前）；
    /// 其失败的 audit 经治理降级新一轮 `commit_intent` 追加进 audits 通道保交织序。
    pub maintain_triggers: Vec<MaintainTrigger>,
    /// apply 后属主可见提示通道（execution_step 机械重跑提示；转移生效后
    /// 呈现——persist 之后，与 HEAD println 时机一致）。**载荷=去前缀纯正文**
    /// （S2b 契约：`[orchestrator]` 前缀由 sink 渲染时统一加回，见
    /// chat_events.rs）——flush 经 [`orchestrator_notice`] 单一出口：REPL 打
    /// 终端（前缀拼回，逐字节不变）、TUI 发 [`ChatEvent::OrchestratorNotice`]。
    pub post_apply_notices: Vec<String>,
}

/// 审计通道的分节收集器：pre_apply（转移前）→ post_apply（转移后），各段内
/// 保收集序。flattened 迭代序 = emit 序。
#[derive(Debug, Clone, Default)]
pub struct EffectsAudits {
    /// apply 前落盘的审计（事件主审计：planning_done / plan_review_passed / …）。
    pub pre_apply: Vec<(String, Value)>,
    /// apply 后落盘的审计（HEAD `mechanical_retry*` 在 apply/attempts 调整之后
    /// 落——少数派顺序，段内保收集序；绝大多数 step 只有 pre_apply 段）。
    pub post_apply: Vec<(String, Value)>,
}

impl EffectsAudits {
    /// 收集一条 apply 前审计（主通道入口）。
    pub fn pre(&mut self, name: impl Into<String>, data: Value) {
        self.pre_apply.push((name.into(), data));
    }
    /// 收集一条 apply 后审计（HEAD 机械重跑审计的少数派时点）。
    pub fn post(&mut self, name: impl Into<String>, data: Value) {
        self.post_apply.push((name.into(), data));
    }
}

impl Effects {
    /// 收集一条 apply 前审计。
    pub fn audit(&mut self, name: impl Into<String>, data: Value) {
        self.audits.pre(name, data);
    }
    /// 收集一条 apply 后审计（HEAD 机械重跑审计的少数派时点）。
    pub fn post_apply_audit(&mut self, name: impl Into<String>, data: Value) {
        self.audits.post(name, data);
    }
    /// 收集一条 conversation 轮。
    pub fn conversation_turn(
        &mut self,
        role: ConversationRole,
        content: String,
        source: ConversationSource,
    ) {
        self.conversation_turns.push(ConversationTurnSpec {
            role,
            content,
            source,
        });
    }
    /// 收集一条 ConverseDone 维护触发（reply_summary 由 step 定格：建图分支 =
    /// `format_plan_reply(dagspec)` 计划摘要，Reply 分支 = 规划器答复原文）。
    pub fn converse_maintain(&mut self, trigger: ConverseMaintain) {
        self.maintain_triggers
            .push(MaintainTrigger::ConverseDone(trigger));
    }
    /// 收集 PlanReviewed 维护触发（拒绝理由原文；disguise 投影在 flush 时于
    /// maintain_after_plan_review 内做——HEAD 语义不变）。
    pub fn plan_reviewed_maintain(&mut self, reason: impl Into<String>) {
        self.maintain_triggers
            .push(MaintainTrigger::PlanReviewed(reason.into()));
    }
    /// 收集一条 apply 后属主可见提示（去前缀纯正文——前缀由 sink 加回）。
    pub fn post_apply_notice(&mut self, notice: impl Into<String>) {
        self.post_apply_notices.push(notice.into());
    }
}

/// conversation 轮通道的落盘规格（flush 时经 `append_to_disk` 逐轮落）。
#[derive(Debug, Clone)]
pub struct ConversationTurnSpec {
    pub role: ConversationRole,
    pub content: String,
    pub source: ConversationSource,
}

/// 维护触发通道条目（收集指令延迟执行，flush 时才跑维护者 LLM 调用，apply 前）。
#[derive(Debug, Clone)]
pub enum MaintainTrigger {
    /// ConverseDone 滚动维护（HEAD `maintain_after_converse` 三参；reply_summary
    /// 由 step 收集时定格——建图分支 = 计划摘要，Reply 分支 = 答复原文）。
    ConverseDone(ConverseMaintain),
    /// PlanReviewed 审查意见维护（plan_review_rejected 路径：拒绝理由原文）。
    PlanReviewed(String),
}

/// 审查结论归档通道（区分 push 进哪条 verdict 历史）。
#[derive(Debug, Clone)]
pub enum VerdictKind {
    Plan(PlanVerdict),
    Exec(alfred_core::ExecVerdict),
}

/// ConverseDone 滚动维护的输入（HEAD `maintain_after_converse` 三参 +
/// reply_summary 由 step 收集时定格）。
#[derive(Debug, Clone)]
pub struct ConverseMaintain {
    pub read_paths: Vec<String>,
    pub owner_message: String,
    /// 维护者 memory 的本轮答复摘要：建图分支 = 计划摘要（key_conclusions
    /// 语义），Reply 分支 = 规划器答复原文。
    pub reply_summary: String,
}

/// 单点提交序列：副作用通道 flush（apply 前）→ apply → apply 后通道 flush
/// → persist。
///
/// 返回 `Some(reply)` = Reply 停驻（治理环把答复 surface 给调用方并停驻）；
/// `None` = 已完成转移+persist（或 Escalate 降级），治理环继续。
pub fn commit_intent(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    intent: StepIntent,
) -> Result<Option<String>> {
    match intent {
        // Reply 停驻：副作用通道 flush（无 apply/无 persist）后把答复 surface
        // 给调用方（HEAD 顺序：conversation 轮 → 审计 → ConverseDone 维护）。
        StepIntent::Reply { text, effects } => {
            flush_verdicts(run, &effects)?;
            flush_audits(run, ctx, &effects.audits.pre_apply)?;
            flush_conversation_turns(run, ctx, &effects.conversation_turns)?;
            flush_maintain_triggers(run, ctx, &effects.maintain_triggers)?;
            Ok(Some(text))
        }
        StepIntent::Escalate {
            event,
            audit_name,
            audit_data,
            reason,
        } => {
            let data = audit_data.unwrap_or_else(|| json!({ "error": reason }));
            audit(&ctx.run_dir, &audit_name, &data)?;
            run.apply(event)?;
            persist_governance_run(&ctx.run_dir, run)?;
            Ok(None)
        }
        StepIntent::Proceed { event, effects } => {
            // ---- flush ①：副作用通道（apply 前，HEAD 语义；通道序 = HEAD
            // 副作用序：审计 → conversation 轮 → dagspec.json 落盘 → 维护触发
            // → run.dagspec 注入——注入在维护**之后**：维护者停摆失败时
            // run.dagspec 保持 None、dagspec.json 已在（HEAD S3 行为）） ----
            flush_verdicts(run, &effects)?;
            flush_audits(run, ctx, &effects.audits.pre_apply)?;
            flush_conversation_turns(run, ctx, &effects.conversation_turns)?;
            write_effects_dagspec(&ctx.run_dir, &effects.dagspec)?;
            flush_maintain_triggers(run, ctx, &effects.maintain_triggers)?;
            inject_run_dagspec(run, &effects.dagspec);
            // ---- 2. 状态机转移 ----
            run.apply(event)?;
            // ---- flush ②：apply 后通道（HEAD 少数派时点：mechanical_retry* 审计
            // 在 apply/attempts 调整之后落；notice 在 persist 之后呈现） ----
            flush_audits(run, ctx, &effects.audits.post_apply)?;
            persist_governance_run(&ctx.run_dir, run)?;
            for notice in &effects.post_apply_notices {
                orchestrator_notice(ctx, notice);
            }
            Ok(None)
        }
    }
}

/// verdicts 通道 flush：审查结论入 run 历史（apply 前——HEAD 顺序，审计与
/// persist 投影都消费它）。
fn flush_verdicts(run: &mut GovernanceRun, effects: &Effects) -> Result<()> {
    for verdict in &effects.verdicts {
        match verdict {
            VerdictKind::Plan(v) => run.plan_verdicts.push(v.clone()),
            VerdictKind::Exec(v) => run.exec_verdicts.push(v.clone()),
        }
    }
    Ok(())
}

/// audits 通道 flush：按收集序逐条 emit（有序 Writer——顺序是唯一不可动
/// 不变式）。pre/post 两段由 [`commit_intent`] 分别调用，段内保序。
fn flush_audits(
    _run: &GovernanceRun,
    ctx: &GovernanceContext,
    audits: &[(String, Value)],
) -> Result<()> {
    for (name, data) in audits {
        audit(&ctx.run_dir, name, data)?;
    }
    Ok(())
}

/// dagspec 通道 flush 之一：dagspec.json 落盘（HEAD 顺序——在维护触发之前，
/// 维护者停摆失败时磁盘产物已在）。
fn write_effects_dagspec(run_dir: &Path, dagspec: &Option<DagSpec>) -> Result<()> {
    if let Some(dagspec) = dagspec {
        write_dagspec(run_dir, dagspec)?;
    }
    Ok(())
}

/// dagspec 通道 flush 之二：run.dagspec 注入（HEAD 顺序——在维护触发之后，
/// 维护者停摆失败时 run.dagspec 保持 None）。
fn inject_run_dagspec(run: &mut GovernanceRun, dagspec: &Option<DagSpec>) {
    if let Some(dagspec) = dagspec {
        run.dagspec = Some(dagspec.clone());
    }
}

/// conversation_turns 通道 flush：按收集序逐轮落 conversation.json。
fn flush_conversation_turns(
    run: &GovernanceRun,
    ctx: &GovernanceContext,
    turns: &[ConversationTurnSpec],
) -> Result<()> {
    for turn in turns {
        append_to_disk(
            &ctx.run_dir,
            &run.run_id,
            turn.role,
            turn.content.clone(),
            turn.source,
        )
        .map_err(anyhow::Error::msg)
        .context("append conversation turn to conversation.json")?;
    }
    Ok(())
}

/// maintain_triggers 通道 flush：按收集序执行维护触发（flush 时才跑维护者
/// LLM 调用——收集阶段零副作用）。ConverseDone 的 reply_summary 由 step 收集
/// 进触发器（建图分支 = `format_plan_reply(dagspec)` 计划摘要，Reply 分支 =
/// 规划器答复原文——key_conclusions 语义），flush 不再回读 run 状态。
/// 失败照常 `Err` 穿出（治理降级统一出口；其 audit 由降级新一轮
/// `commit_intent` 追加进 audits 通道保交织序）。
fn flush_maintain_triggers(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    triggers: &[MaintainTrigger],
) -> Result<()> {
    for trigger in triggers {
        match trigger {
            MaintainTrigger::ConverseDone(m) => {
                maintain_after_converse(
                    run,
                    ctx,
                    m.read_paths.clone(),
                    &m.owner_message,
                    &m.reply_summary,
                )?;
            }
            MaintainTrigger::PlanReviewed(reason) => {
                // PlanReviewed 维护（审查结论落定后，apply 前）：拒绝理由经
                // disguise 投影（属主口吻中性转写——维护者零 reviewer 痕迹）。
                maintain_after_plan_review(run, ctx, reason)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_events::{ChatEvent, ChatEventBus};
    use alfred_core::governance::{GovernanceOptions, GovernanceState};
    use alfred_core::request::OwnerRequest;
    use std::path::PathBuf;

    fn test_run() -> GovernanceRun {
        GovernanceRun::new(
            "run-test",
            OwnerRequest {
                id: "req-test".into(),
                title: "t".into(),
                description: "d".into(),
                acceptance_criteria: "a".into(),
                created_at: "2026-09-07T00:00:00Z".into(),
            },
            GovernanceOptions::default(),
        )
    }

    fn test_model() -> alfred_executor::config::ExecutorModel {
        alfred_executor::config::ExecutorModel {
            provider: "p".into(),
            model: "m".into(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 8192,
            context_window: None,
            raw_id: true,
        }
    }

    fn temp_ctx(tag: &str) -> (GovernanceContext, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "alfred-commit-intent-{tag}-{}",
            alfred_core::util::short_id("t")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (
            GovernanceContext {
                run_dir: dir.clone(),
                planner_model: test_model(),
                executor_model: test_model(),
                reviewer_model: test_model(),
                append_system_prompt: String::new(),
                // S2b：driver/REPL 面缺省无通道（通知打终端）；事件化用例按需注入。
                events: None,
            },
            dir,
        )
    }

    fn audit_events(dir: &Path) -> Vec<(String, Value)> {
        let text = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (v["event"].as_str().unwrap().to_string(), v["data"].clone())
            })
            .collect()
    }

    #[test]
    fn proceed_applies_side_effects_before_transition_and_persists_once() {
        let (ctx, dir) = temp_ctx("proceed");
        let mut run = test_run();
        run.apply(GovernanceEvent::PlanProduced).unwrap(); // → PlanReviewing
                                                           // PlanReviewPassed：verdict 归档 + plan_review_passed 审计 → apply → persist。
        let mut effects = Effects::default();
        effects.audit("plan_review_passed", json!({ "reason": "faithful" }));
        let intent = StepIntent::Proceed {
            event: GovernanceEvent::PlanReviewPassed,
            effects,
        };
        let out = commit_intent(&mut run, &ctx, intent).unwrap();
        assert!(out.is_none());
        // 转移生效：PlanReviewing → Executing。
        assert_eq!(run.state(), GovernanceState::Executing);
        // audit 事件序：plan_review_passed 单条（无 state_entered——那是 loop 的事）。
        let events = audit_events(&dir);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["plan_review_passed"]);
        assert_eq!(events[0].1["reason"], "faithful");
        // persist 单点：state.json + plan-verdicts.json 投影同步落盘。
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(state["state_machine"]["state"], "executing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verdict_channel_archives_before_transition_and_persists() {
        let (ctx, dir) = temp_ctx("verdict");
        let mut run = test_run();
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        let mut effects = Effects::default();
        effects
            .verdicts
            .push(VerdictKind::Plan(PlanVerdict::new(true, "faithful")));
        effects.audit("plan_review_passed", json!({ "reason": "faithful" }));
        let intent = StepIntent::Proceed {
            event: GovernanceEvent::PlanReviewPassed,
            effects,
        };
        commit_intent(&mut run, &ctx, intent).unwrap();
        // verdicts 通道已入 run 历史（apply 前副作用）。
        assert_eq!(run.plan_verdicts.len(), 1);
        // persist 投影同步落盘。
        let pv: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("plan-verdicts.json")).unwrap())
                .unwrap();
        assert_eq!(pv.as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn audits_channel_preserves_collection_order_across_pre_and_post() {
        // 审计通道（有序 Writer）不变式：emit 序 = flush 序；post_apply 段在
        // apply 之后落（HEAD mechanical_retry* 少数派时点），persist 在其后。
        let (ctx, dir) = temp_ctx("audit-order");
        let mut run = test_run();
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        let mut effects = Effects::default();
        effects.audit("pre_first", json!({ "i": 1 }));
        effects.audit("pre_second", json!({ "i": 2 }));
        effects.post_apply_audit("post_only", json!({ "i": 3 }));
        let intent = StepIntent::Proceed {
            event: GovernanceEvent::PlanReviewPassed,
            effects,
        };
        commit_intent(&mut run, &ctx, intent).unwrap();
        let names: Vec<String> = audit_events(&dir).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["pre_first", "pre_second", "post_only"]);
        // post_apply 审计在状态转移后落：state 已 executing 而 audit 序不变。
        assert_eq!(run.state(), GovernanceState::Executing);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn escalate_audits_named_event_then_applies_then_persists() {
        let (ctx, dir) = temp_ctx("escalate");
        let mut run = test_run();
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        let intent = StepIntent::Escalate {
            event: GovernanceEvent::PlanReviewError,
            audit_name: "plan_review_error_escalated".into(),
            audit_data: None,
            reason: "offline: reviewer skipped".into(),
        };
        let out = commit_intent(&mut run, &ctx, intent).unwrap();
        assert!(out.is_none());
        assert_eq!(run.state(), GovernanceState::Escalated);
        assert_eq!(
            run.escalation_source,
            Some(alfred_core::governance::EscalationSource::PlanReview)
        );
        let events = audit_events(&dir);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["plan_review_error_escalated"]);
        assert_eq!(events[0].1["error"], "offline: reviewer skipped");
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(state["state_machine"]["state"], "escalated");
        assert_eq!(state["escalation_source"], "plan_review");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reply_parks_without_transition_and_flushes_named_side_effects() {
        // 维护走离线恒等直通（单测不起宿主 pi；模型为 raw builtin 会 bail）。
        std::env::set_var("ALFRED_MAINTAIN_OFFLINE", "1");
        let (ctx, dir) = temp_ctx("reply");
        let mut run = test_run();
        let mut effects = Effects::default();
        effects.audit("converse_reply", json!({ "record": "llm-calls/0000.json" }));
        effects.conversation_turn(
            ConversationRole::Planner,
            "需要先确认目录结构。".into(),
            ConversationSource::ConverseReply,
        );
        effects.converse_maintain(ConverseMaintain {
            read_paths: vec![],
            owner_message: "属主原话".into(),
            reply_summary: "需要先确认目录结构。".into(),
        });
        let intent = StepIntent::Reply {
            text: "需要先确认目录结构。".into(),
            effects,
        };
        let out = commit_intent(&mut run, &ctx, intent).unwrap();
        assert_eq!(out.as_deref(), Some("需要先确认目录结构。"));
        // 无转移：state 停留 Planning。
        assert_eq!(run.state(), GovernanceState::Planning);
        // 具名审计已 flush；conversation 轮已落；无 persist（无转移）。
        let events = audit_events(&dir);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["converse_reply", "maintain_done"]);
        let conv: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("conversation.json")).unwrap())
                .unwrap();
        let contents: Vec<&str> = conv["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["content"].as_str().unwrap())
            .collect();
        assert!(contents.contains(&"需要先确认目录结构。"));
        assert!(!dir.join("state.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn side_effect_failure_propagates_err_before_any_transition() {
        let (ctx, dir) = temp_ctx("side-effect-err");
        let mut run = test_run();
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        // 副作用写盘失败注入：run_dir 指向一个普通文件（audit open 必败）。
        let file_path = dir.join("not-a-dir");
        std::fs::write(&file_path, "x").unwrap();
        let mut bad_ctx = ctx.clone();
        bad_ctx.run_dir = file_path.clone();
        let mut effects = Effects::default();
        effects.audit("plan_review_passed", json!({}));
        let intent = StepIntent::Proceed {
            event: GovernanceEvent::PlanReviewPassed,
            effects,
        };
        assert!(commit_intent(&mut run, &bad_ctx, intent).is_err());
        // 副作用失败 → Err 穿出（治理降级由 loop 接），状态机不动、无 persist。
        assert_eq!(run.state(), GovernanceState::PlanReviewing);
        std::fs::remove_file(&file_path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// S2b 通知事件化：通道在场 → 通知变 [`ChatEvent::OrchestratorNotice`] 事件
    /// 且载荷=去前缀纯正文（契约见 chat_events.rs）。两个出口同一语义：
    /// commit_intent 的 post_apply_notices 落点（机械重跑提示通道）与治理环
    /// 直接出口 `orchestrator_notice`（状态行/升级块/contract_fault 共用）。
    /// REPL 面（通道缺席）打终端逐字节不变——e2e chat.sh 黑盒覆盖，此处不重证。
    #[test]
    fn orchestrator_notices_emit_deprefixed_events_when_channel_present() {
        let (mut ctx, dir) = temp_ctx("s2b-notice");
        let (tx, rx) = ChatEventBus::new();
        ctx.events = Some(tx);
        let mut run = test_run();
        run.apply(GovernanceEvent::PlanProduced).unwrap();

        // 落点①：commit_intent flush post_apply_notices（Effects 通道正文）。
        let mut effects = Effects::default();
        effects.post_apply_notice("节点 task-1 执行机械失败，按同一契约重跑（1/2）：boom");
        commit_intent(
            &mut run,
            &ctx,
            StepIntent::Proceed {
                event: GovernanceEvent::PlanReviewPassed,
                effects,
            },
        )
        .unwrap();

        // 落点②：治理环直接出口（状态行/升级块同函数）。
        orchestrator_notice(&ctx, "进入计划审查（state=plan_reviewing）");

        let got: Vec<ChatEvent> = {
            let mut v = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                v.push(ev);
            }
            v
        };
        assert_eq!(
            got,
            vec![
                ChatEvent::OrchestratorNotice(
                    "节点 task-1 执行机械失败，按同一契约重跑（1/2）：boom".into()
                ),
                ChatEvent::OrchestratorNotice(
                    "进入计划审查（state=plan_reviewing）".into()
                ),
            ],
            "两条通知均事件化且载荷无 [orchestrator] 前缀"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
