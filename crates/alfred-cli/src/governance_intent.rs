//! StepIntent — 治理环 step 函数的意图描述 + 单点提交序列 `commit_intent`。
//!
//! 重构方案 v2（.plans/重构方案-驱动层.md）步骤②/③/④ 的核心抽象：step 函数
//! （planning/plan_review/execution/exec_review）不再各自内联"副作用 → apply →
//! persist"，而是**产出意图**，由本模块 [`commit_intent`] 按固定提交序列单点执行：
//!
//! ```text
//! commit_intent(run, ctx, intent):
//!   1. 副作用（apply 前，HEAD 语义）：审计落盘 / conversation 轮 / 维护者触发 /
//!      verdict 归档 / dagspec+contract 落盘 / 计数器调整
//!   2. run.apply(event)（Reply 停驻无事件）
//!   3. persist（state.json + verdict 投影，单点）
//! ```
//!
//! 副作用失败（维护者停摆等）→ `Err` 穿出 → `run_governance_loop` 治理降级
//! （9265791 统一出口），不静默跳过。行为等价由 tests/e2e/equiv.sh 判定
//! （5 场景 canonical 比对 + HEAD 事件-副作用断言表）。
//!
//! **审计名的单一真源**：同一状态机事件可对应多个审计名（如
//! `ExecutionFailedEscalate` → `mechanical_budget_exhausted_escalated` 或
//! `execution_hard_error_escalated`，按机械/硬错误分支二选一）——所以
//! [`StepIntent::Proceed`] 显式携带 `audit_name` + `audit_data`，不按事件名推导。
use crate::governance::{
    audit, format_plan_reply, maintain_after_converse, maintain_after_plan_review,
    persist_governance_run, write_dagspec, write_run_contract, GovernanceContext,
};
use alfred_core::conversation::{append_to_disk, ConversationRole, ConversationSource};
use alfred_core::governance::{GovernanceEvent, GovernanceRun};
use alfred_core::{DagSpec, PlanVerdict};
use anyhow::{Context as _, Result};
use serde_json::{json, Value};

/// step 函数产出的意图（重构方案 v2 §1）。
#[derive(Debug, Clone)]
pub enum StepIntent {
    /// 正常转移：副作用 → apply(event) → persist。
    Proceed {
        event: GovernanceEvent,
        payload: StepPayload,
    },
    /// Reply 停驻（§2.4 答复分支）：conversation 轮 + ConverseDone 维护后**无
    /// 事件**——state 停留 Planning，治理环返回调用方（reply 呈现给属主）。
    Reply { text: String },
    /// 治理降级（§六继承项）：step 宿主驱动失败，fail_* outcome 已由驱动落盘。
    /// `audit_name` 承载降级审计名（planning_error_escalated /
    /// plan_review_error_escalated / review_host_failure_escalated / …）。
    /// 注意：PlanningError 走 loop 内联 return 路径（HEAD 语义），由调用方区分。
    Escalate {
        event: GovernanceEvent,
        audit_name: String,
        reason: String,
    },
}

/// Proceed 的载荷通道（重构方案 v2 §1：多审计名 + 各事件审计 data + 附加产物）。
#[derive(Debug, Clone, Default)]
pub struct StepPayload {
    /// 审查结论归档（plan/exec review 命中 verdict 分支时 push 进 run 历史）。
    pub verdict: Option<VerdictKind>,
    /// 建图产物（PlanProduced 路径：contract/dagspec 落盘 + run.dagspec 替换）。
    pub dagspec: Option<DagSpec>,
    /// 各事件审计 data（planning_done 的 nodes、exec_review_passed 的 explanation 等）。
    pub audit_data: Value,
    /// 审计事件名（ Proceed 必填——同一事件的多个可能审计名由 step 函数决定）。
    pub audit_name: String,
    /// ConverseDone 维护输入（PlanProduced 路径必触发；reply_summary=计划摘要）。
    pub converse_maintain: Option<ConverseMaintain>,
    /// apply 后审计通道（HEAD `mechanical_retry_from_verdict` 在 apply/attempts
    /// 调整之后落——少数派顺序，显式通道保序；其余事件一律 None）。
    pub post_apply_audit: Option<(String, Value)>,
    /// PlanReviewed 维护输入（plan_review_rejected 路径：拒绝理由原文；
    /// disguise 投影在 maintain_after_plan_review 内做——HEAD 语义不变）。
    pub plan_reviewed_maintain: Option<String>,
}

/// 审查结论归档通道（区分 push 进哪条 verdict 历史）。
#[derive(Debug, Clone)]
pub enum VerdictKind {
    Plan(PlanVerdict),
    Exec(alfred_core::ExecVerdict),
}

/// ConverseDone 滚动维护的输入（HEAD `maintain_after_converse` 三参）。
#[derive(Debug, Clone)]
pub struct ConverseMaintain {
    pub read_paths: Vec<String>,
    pub owner_message: String,
}

/// 单点提交序列：副作用（apply 前）→ apply → persist。
///
/// 返回 `Some(reply)` = Reply 停驻（治理环应把答复 surface 给调用方并停驻）。
/// 返回 `None` = 已完成转移+persist（或 Escalate 降级），治理环继续。
pub fn commit_intent(
    run: &mut GovernanceRun,
    ctx: &GovernanceContext,
    intent: StepIntent,
) -> Result<Option<String>> {
    match intent {
        StepIntent::Reply { text } => {
            // conversation 轮 + ConverseDone 维护已在 step 侧的 Proceed/Reply
            // 拆分中处理（Reply 的维护输入由 step 侧经 StepIntent::Reply 前置
            // 调用 maintain 通道……不——Reply 的副作用也必须单点。见
            // `commit_reply`：conversation + maintain + audit，无 apply/persist。
            Ok(Some(text))
        }
        StepIntent::Escalate {
            event,
            audit_name,
            reason,
        } => {
            audit(&ctx.run_dir, &audit_name, &json!({ "reason": reason }))?;
            run.apply(event)?;
            persist_governance_run(&ctx.run_dir, run)?;
            Ok(None)
        }
        StepIntent::Proceed {
            event,
            payload,
        } => {
            // ---- 1. 副作用（apply 前，HEAD 语义） ----
            // 1a. 审查结论归档（verdict 历史 push，先于审计——HEAD 顺序）。
            match &payload.verdict {
                Some(VerdictKind::Plan(v)) => run.plan_verdicts.push(v.clone()),
                Some(VerdictKind::Exec(v)) => run.exec_verdicts.push(v.clone()),
                None => {}
            }
            // 1b. 事件审计。
            audit(&ctx.run_dir, &payload.audit_name, &payload.audit_data)?;
            // 1c. PlanProduced 附加落盘：conversation 轮 → contract → dagspec
            //     （reviewer_models 注入后）→ ConverseDone 维护（成功才推进基线）。
            if let Some(dagspec) = &payload.dagspec {
                append_to_disk(
                    &ctx.run_dir,
                    &run.run_id,
                    ConversationRole::Planner,
                    format_plan_reply(dagspec),
                    ConversationSource::ConverseReply,
                )
                .map_err(anyhow::Error::msg)
                .context("append converse.reply to conversation.json")?;
                write_run_contract(&ctx.run_dir, dagspec)?;
                write_dagspec(&ctx.run_dir, dagspec)?;
                if let Some(m) = &payload.converse_maintain {
                    maintain_after_converse(
                        run,
                        ctx,
                        m.read_paths.clone(),
                        &m.owner_message,
                        &format_plan_reply(dagspec),
                    )?;
                }
            }
            if let Some(reason) = &payload.plan_reviewed_maintain {
                // PlanReviewed 维护（审查结论落定后，apply 前）：拒绝理由经
                // disguise 投影（属主口吻中性转写——维护者零 reviewer 痕迹）。
                maintain_after_plan_review(run, ctx, reason)?;
            }
            // ---- 2. 状态机转移 ----
            run.apply(event)?;
            // ---- 2b. apply 后审计（HEAD 顺序：mechanical_retry_from_verdict 在
            // attempts 调整/apply 之后落——经 post_apply_audit 通道保序）。
            if let Some((name, data)) = payload.post_apply_audit {
                audit(&ctx.run_dir, &name, &data)?;
            }
            // ---- 3. persist 单点 ----
            persist_governance_run(&ctx.run_dir, run)?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            },
            dir,
        )
    }

    fn audit_events(dir: &PathBuf) -> Vec<(String, Value)> {
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
        let intent = StepIntent::Proceed {
            event: GovernanceEvent::PlanReviewPassed,
            payload: StepPayload {
                verdict: Some(VerdictKind::Plan(PlanVerdict::new(
                    true,
                    "faithful",
                ))),
                audit_name: "plan_review_passed".into(),
                audit_data: json!({ "reason": "faithful" }),
                ..Default::default()
            },
        };
        let out = commit_intent(&mut run, &ctx, intent).unwrap();
        assert!(out.is_none());
        // 转移生效：PlanReviewing → Executing。
        assert_eq!(run.state(), GovernanceState::Executing);
        // verdict 归档（apply 前副作用）。
        assert_eq!(run.plan_verdicts.len(), 1);
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
        let pv: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("plan-verdicts.json")).unwrap())
                .unwrap();
        assert_eq!(pv.as_array().unwrap().len(), 1);
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
            reason: "offline: reviewer skipped".into(),
        };
        let out = commit_intent(&mut run, &ctx, intent).unwrap();
        assert!(out.is_none());
        assert_eq!(run.state(), GovernanceState::Escalated);
        assert_eq!(run.escalation_source, Some(alfred_core::governance::EscalationSource::PlanReview));
        let events = audit_events(&dir);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["plan_review_error_escalated"]);
        assert_eq!(events[0].1["reason"], "offline: reviewer skipped");
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(state["state_machine"]["state"], "escalated");
        assert_eq!(state["escalation_source"], "plan_review");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reply_parks_without_transition_or_persist_side_effects() {
        let (ctx, dir) = temp_ctx("reply");
        let mut run = test_run();
        let intent = StepIntent::Reply {
            text: "需要先确认目录结构。".into(),
        };
        let out = commit_intent(&mut run, &ctx, intent).unwrap();
        assert_eq!(out.as_deref(), Some("需要先确认目录结构。"));
        // 无转移：state 停留 Planning。
        assert_eq!(run.state(), GovernanceState::Planning);
        // 无审计/无 persist（Reply 的 conversation 轮由 step 侧落，见拆分阶段）。
        assert!(!dir.join("audit.jsonl").exists());
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
        let intent = StepIntent::Proceed {
            event: GovernanceEvent::PlanReviewPassed,
            payload: StepPayload {
                audit_name: "plan_review_passed".into(),
                audit_data: json!({}),
                ..Default::default()
            },
        };
        assert!(commit_intent(&mut run, &bad_ctx, intent).is_err());
        // 副作用失败 → Err 穿出（治理降级由 loop 接），状态机不动、无 persist。
        assert_eq!(run.state(), GovernanceState::PlanReviewing);
        std::fs::remove_file(&file_path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
