use crate::entities::{Artifact, DagSpec};
use crate::routing::{route, RouteAction};
use crate::verdict::{ExecVerdict, PlanVerdict};
use serde::{Deserialize, Serialize};
use std::fmt;

// 治理环唯一非 LLM 组件：纯函数确定性状态机。
// 给定 (当前状态, 事件) → 下一状态；无 LLM 调用、无随机、无系统时间依赖。
// 路由查表委托给 routing::route()，本文件不重复实现；裁决实体复用 verdict.rs。

/// 治理环状态：PlanRejected / Escalated 是等待属主拍板的挂起态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum State {
    Planning,
    PlanReviewing,
    PlanRejected,
    Executing,
    ExecReviewing,
    Completed,
    Escalated,
}

/// 属主对挂起态的裁决：重试 / 改契约 / 放弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerDecision {
    Retry,
    ReviseContract,
    Abandon,
}

/// 状态机输入事件。Artifact 复用 entities.rs，裁决复用 verdict.rs，不重定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    PlanSubmitted(DagSpec),
    PlanReviewed(PlanVerdict),
    ExecutionDone(Artifact),
    ExecReviewed(ExecVerdict),
    OwnerDecided(OwnerDecision),
}

impl Event {
    fn kind(&self) -> &'static str {
        match self {
            Self::PlanSubmitted(_) => "plan_submitted",
            Self::PlanReviewed(_) => "plan_reviewed",
            Self::ExecutionDone(_) => "execution_done",
            Self::ExecReviewed(_) => "exec_reviewed",
            Self::OwnerDecided(_) => "owner_decided",
        }
    }
}

/// 非法 状态+事件 组合：明确报错，不静默忽略。
#[derive(Debug, Clone, PartialEq)]
pub enum OrchError {
    IllegalTransition { state: State, event_kind: String },
}

impl fmt::Display for OrchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IllegalTransition { state, event_kind } => {
                write!(f, "illegal transition: state `{state:?}` cannot accept event `{event_kind}`")
            }
        }
    }
}

impl std::error::Error for OrchError {}

/// 一次合法转移的结果：新状态 + 路由动作（仅 ExecReviewed 经 route() 产生）。
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionOutcome {
    pub new_state: State,
    pub action: Option<RouteAction>,
}

/// 确定性编排器。带 run 状态持久化支持：serde 序列化后 CLI 落盘 state.json。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Orchestrator {
    state: State,
    attempts: u32,
    retry_budget: u32,
    dag_spec: Option<DagSpec>,
    plan_verdict: Option<PlanVerdict>,
    exec_verdict: Option<ExecVerdict>,
    artifact: Option<Artifact>,
}

impl Orchestrator {
    /// 新建编排器，初始状态 Planning。retry_budget 为 mechanical 重试预算上限。
    pub fn new(retry_budget: u32) -> Self {
        Self {
            state: State::Planning,
            attempts: 0,
            retry_budget,
            dag_spec: None,
            plan_verdict: None,
            exec_verdict: None,
            artifact: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn retry_budget(&self) -> u32 {
        self.retry_budget
    }

    /// 确定性转移：给定事件推进状态机。非法组合返回 OrchError::IllegalTransition。
    /// 纯函数语义：除自身状态外不触碰任何外部世界。
    pub fn transition(&mut self, event: Event) -> Result<TransitionOutcome, OrchError> {
        match (self.state, event) {
            (State::Planning, Event::PlanSubmitted(dag)) => {
                self.dag_spec = Some(dag);
                self.plan_verdict = None;
                self.go(State::PlanReviewing)
            }
            (State::PlanReviewing, Event::PlanReviewed(verdict)) => {
                let next = if verdict.pass { State::Executing } else { State::PlanRejected };
                self.plan_verdict = Some(verdict);
                self.go(next)
            }
            (State::PlanRejected, Event::OwnerDecided(decision)) => match decision {
                OwnerDecision::Retry => {
                    self.reset_exec_state();
                    self.attempts = 0;
                    self.go(State::Planning)
                }
                OwnerDecision::ReviseContract => {
                    self.reset_exec_state();
                    self.go(State::Planning)
                }
                OwnerDecision::Abandon => self.go(State::Completed),
            },
            (State::Executing, Event::ExecutionDone(artifact)) => {
                self.artifact = Some(artifact);
                self.exec_verdict = None;
                self.go(State::ExecReviewing)
            }
            (State::ExecReviewing, Event::ExecReviewed(verdict)) => {
                let action = route(&verdict, self.retry_budget, self.attempts);
                self.exec_verdict = Some(verdict);
                match action {
                    // 单节点骨架：Advance 直接落成 Completed；多节点推进由后续切片扩展。
                    RouteAction::Advance => self.go_with(State::Completed, action),
                    RouteAction::Retry { attempt } => {
                        self.attempts = attempt;
                        self.go_with(State::Executing, action)
                    }
                    RouteAction::Escalate { .. } => self.go_with(State::Escalated, action),
                }
            }
            (State::Escalated, Event::OwnerDecided(decision)) => match decision {
                OwnerDecision::Retry => {
                    self.reset_exec_state();
                    self.attempts = 0;
                    self.go(State::Executing)
                }
                OwnerDecision::ReviseContract => {
                    self.reset_exec_state();
                    self.go(State::Planning)
                }
                OwnerDecision::Abandon => self.go(State::Completed),
            },
            (state, event) => Err(OrchError::IllegalTransition {
                state,
                event_kind: event.kind().to_string(),
            }),
        }
    }

    fn go(&mut self, next: State) -> Result<TransitionOutcome, OrchError> {
        self.state = next;
        Ok(TransitionOutcome { new_state: next, action: None })
    }

    fn go_with(&mut self, next: State, action: RouteAction) -> Result<TransitionOutcome, OrchError> {
        self.state = next;
        Ok(TransitionOutcome { new_state: next, action: Some(action) })
    }

    /// 进入新一轮计划/执行前清零执行期痕迹，防旧 verdict/artifact 漂移进新 run。
    fn reset_exec_state(&mut self) {
        self.dag_spec = None;
        self.plan_verdict = None;
        self.exec_verdict = None;
        self.artifact = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::{
        Confidence, Contract, FailureClass, NodeSpec, NodeType, VerdictValue,
    };
    use serde_json::{Map, Value};
    use std::collections::BTreeMap;

    fn dag_spec() -> DagSpec {
        DagSpec {
            name: "smoke".into(),
            version: 1,
            entrypoint: "n1".into(),
            nodes: vec![NodeSpec {
                node_id: "n1".into(),
                node_type: NodeType::Step,
                contract: Contract {
                    prompt: "do it".into(),
                    acceptance_criteria: "done".into(),
                    reviewer_models: vec![],
                },
                params: Map::<String, Value>::new(),
                input_schema: BTreeMap::new(),
                routes: None,
            }],
            edges: vec![],
        }
    }

    fn artifact() -> Artifact {
        Artifact {
            node_id: "n1".into(),
            workspace_diff: "diff".into(),
            produced_at: "2026-08-24T00:00:00Z".into(),
        }
    }

    fn plan_verdict(pass: bool) -> PlanVerdict {
        PlanVerdict { pass, reason: "review".into() }
    }

    fn exec_correct() -> ExecVerdict {
        ExecVerdict::correct(Confidence::High, vec!["ok".into()], "correct".into())
    }

    fn exec_failed(failure_class: FailureClass) -> ExecVerdict {
        ExecVerdict::failed(
            VerdictValue::I,
            failure_class,
            Confidence::High,
            vec!["bad".into()],
            "failed".into(),
        )
    }

    /// 推进到指定状态的测试驱动：只走合法转移。
    fn drive_to(retry_budget: u32, target: State) -> Orchestrator {
        let mut orch = Orchestrator::new(retry_budget);
        match target {
            State::Planning => {}
            State::PlanReviewing => {
                orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
            }
            State::PlanRejected => {
                orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
                orch.transition(Event::PlanReviewed(plan_verdict(false))).unwrap();
            }
            State::Executing | State::ExecReviewing => {
                orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
                orch.transition(Event::PlanReviewed(plan_verdict(true))).unwrap();
                if target == State::ExecReviewing {
                    orch.transition(Event::ExecutionDone(artifact())).unwrap();
                }
            }
            State::Escalated => {
                orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
                orch.transition(Event::PlanReviewed(plan_verdict(true))).unwrap();
                orch.transition(Event::ExecutionDone(artifact())).unwrap();
                orch.transition(Event::ExecReviewed(exec_failed(FailureClass::ContractFault)))
                    .unwrap();
            }
            State::Completed => unreachable!("tests build Completed via specific transitions"),
        }
        assert_eq!(orch.state(), target);
        orch
    }

    #[test]
    fn planning_accepts_plan_submitted_and_moves_to_plan_reviewing() {
        let mut orch = Orchestrator::new(2);
        let outcome = orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
        assert_eq!(outcome, TransitionOutcome { new_state: State::PlanReviewing, action: None });
        assert_eq!(orch.state(), State::PlanReviewing);
    }

    #[test]
    fn plan_reviewing_accepts_pass_and_moves_to_executing() {
        let mut orch = drive_to(2, State::PlanReviewing);
        let outcome = orch.transition(Event::PlanReviewed(plan_verdict(true))).unwrap();
        assert_eq!(outcome.new_state, State::Executing);
        assert_eq!(orch.state(), State::Executing);
    }

    #[test]
    fn plan_reviewing_accepts_reject_and_moves_to_plan_rejected() {
        let mut orch = drive_to(2, State::PlanReviewing);
        let outcome = orch.transition(Event::PlanReviewed(plan_verdict(false))).unwrap();
        assert_eq!(outcome.new_state, State::PlanRejected);
        assert_eq!(orch.state(), State::PlanRejected);
    }

    #[test]
    fn plan_rejected_retry_returns_to_planning_and_closes_the_loop() {
        // 回路：PlanRejected → OwnerDecided(Retry) → Planning → 可再次提交新计划。
        let mut orch = drive_to(2, State::PlanRejected);
        let outcome = orch.transition(Event::OwnerDecided(OwnerDecision::Retry)).unwrap();
        assert_eq!(outcome.new_state, State::Planning);
        assert_eq!(orch.state(), State::Planning);
        assert_eq!(orch.attempts(), 0);

        let outcome = orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
        assert_eq!(outcome.new_state, State::PlanReviewing);
        assert_eq!(orch.state(), State::PlanReviewing);
    }

    #[test]
    fn plan_rejected_revise_contract_returns_to_planning() {
        let mut orch = drive_to(2, State::PlanRejected);
        let outcome =
            orch.transition(Event::OwnerDecided(OwnerDecision::ReviseContract)).unwrap();
        assert_eq!(outcome.new_state, State::Planning);
        assert_eq!(orch.state(), State::Planning);
    }

    #[test]
    fn plan_rejected_abandon_completes_run() {
        let mut orch = drive_to(2, State::PlanRejected);
        let outcome = orch.transition(Event::OwnerDecided(OwnerDecision::Abandon)).unwrap();
        assert_eq!(outcome.new_state, State::Completed);
        assert_eq!(orch.state(), State::Completed);
    }

    #[test]
    fn executing_accepts_execution_done_and_moves_to_exec_reviewing() {
        let mut orch = drive_to(2, State::Executing);
        let outcome = orch.transition(Event::ExecutionDone(artifact())).unwrap();
        assert_eq!(outcome.new_state, State::ExecReviewing);
        assert_eq!(orch.state(), State::ExecReviewing);
    }

    #[test]
    fn correct_exec_verdict_advances_single_node_run_to_completed() {
        let mut orch = drive_to(2, State::ExecReviewing);
        let outcome = orch.transition(Event::ExecReviewed(exec_correct())).unwrap();
        assert_eq!(outcome, TransitionOutcome { new_state: State::Completed, action: Some(RouteAction::Advance) });
        assert_eq!(orch.state(), State::Completed);
    }

    #[test]
    fn mechanical_failure_within_budget_retries_and_increments_attempts() {
        let mut orch = drive_to(2, State::ExecReviewing);
        let outcome = orch
            .transition(Event::ExecReviewed(exec_failed(FailureClass::Mechanical)))
            .unwrap();
        assert_eq!(
            outcome,
            TransitionOutcome { new_state: State::Executing, action: Some(RouteAction::Retry { attempt: 1 }) }
        );
        assert_eq!(orch.state(), State::Executing);
        assert_eq!(orch.attempts(), 1);
    }

    #[test]
    fn mechanical_failure_with_exhausted_budget_escalates() {
        // budget=1：第一次失败后 attempts=1 回到 Executing，再走一轮到 ExecReviewing，
        // 第二次机械失败时 attempts(1) >= budget(1) → 预算耗尽 → Escalate。
        let mut orch = drive_to(1, State::ExecReviewing);
        let first = orch
            .transition(Event::ExecReviewed(exec_failed(FailureClass::Mechanical)))
            .unwrap();
        assert_eq!(first.action, Some(RouteAction::Retry { attempt: 1 }));

        orch.transition(Event::ExecutionDone(artifact())).unwrap();
        let second = orch
            .transition(Event::ExecReviewed(exec_failed(FailureClass::Mechanical)))
            .unwrap();
        assert_eq!(
            second,
            TransitionOutcome {
                new_state: State::Escalated,
                action: Some(RouteAction::Escalate { suggest_contract_change: false }),
            }
        );
        assert_eq!(orch.state(), State::Escalated);
    }

    #[test]
    fn contract_fault_escalates_with_pre_labeled_contract_change_suggestion() {
        let mut orch = drive_to(2, State::ExecReviewing);
        let outcome = orch
            .transition(Event::ExecReviewed(exec_failed(FailureClass::ContractFault)))
            .unwrap();
        assert_eq!(
            outcome,
            TransitionOutcome {
                new_state: State::Escalated,
                action: Some(RouteAction::Escalate { suggest_contract_change: true }),
            }
        );
        assert_eq!(orch.state(), State::Escalated);
    }

    #[test]
    fn escalated_retry_returns_to_executing_and_resets_attempts() {
        let mut orch = drive_to(2, State::Escalated);
        let outcome = orch.transition(Event::OwnerDecided(OwnerDecision::Retry)).unwrap();
        assert_eq!(outcome.new_state, State::Executing);
        assert_eq!(orch.state(), State::Executing);
        assert_eq!(orch.attempts(), 0);
    }

    #[test]
    fn escalated_revise_contract_returns_to_planning() {
        let mut orch = drive_to(2, State::Escalated);
        let outcome =
            orch.transition(Event::OwnerDecided(OwnerDecision::ReviseContract)).unwrap();
        assert_eq!(outcome.new_state, State::Planning);
        assert_eq!(orch.state(), State::Planning);
    }

    #[test]
    fn escalated_abandon_completes_run() {
        let mut orch = drive_to(2, State::Escalated);
        let outcome = orch.transition(Event::OwnerDecided(OwnerDecision::Abandon)).unwrap();
        assert_eq!(outcome.new_state, State::Completed);
        assert_eq!(orch.state(), State::Completed);
    }

    #[test]
    fn illegal_combinations_are_rejected_with_explicit_error() {
        // Planning 上只允许 PlanSubmitted。
        let mut planning = Orchestrator::new(2);
        for event in [
            Event::PlanReviewed(plan_verdict(true)),
            Event::ExecutionDone(artifact()),
            Event::ExecReviewed(exec_correct()),
            Event::OwnerDecided(OwnerDecision::Retry),
        ] {
            let err = planning.transition(event.clone()).unwrap_err();
            assert_eq!(
                err,
                OrchError::IllegalTransition { state: State::Planning, event_kind: event.kind().into() }
            );
            assert_eq!(planning.state(), State::Planning, "illegal event must not mutate state");
        }

        // Completed 是终态：任何事件都拒绝。
        let mut completed = drive_to(2, State::ExecReviewing);
        completed.transition(Event::ExecReviewed(exec_correct())).unwrap();
        assert_eq!(completed.state(), State::Completed);
        let err = completed.transition(Event::OwnerDecided(OwnerDecision::Retry)).unwrap_err();
        assert_eq!(
            err,
            OrchError::IllegalTransition { state: State::Completed, event_kind: "owner_decided".into() }
        );

        // PlanReviewing 不接受 OwnerDecided（属主无权越过审查）。
        let mut reviewing = drive_to(2, State::PlanReviewing);
        let err = reviewing.transition(Event::OwnerDecided(OwnerDecision::Abandon)).unwrap_err();
        assert_eq!(
            err,
            OrchError::IllegalTransition { state: State::PlanReviewing, event_kind: "owner_decided".into() }
        );

        // ExecReviewing 不接受 PlanSubmitted（计划事件不能打断执行审查）。
        let mut exec_reviewing = drive_to(2, State::ExecReviewing);
        let err = exec_reviewing.transition(Event::PlanSubmitted(dag_spec())).unwrap_err();
        assert_eq!(
            err,
            OrchError::IllegalTransition { state: State::ExecReviewing, event_kind: "plan_submitted".into() }
        );
    }

    #[test]
    fn orchestrator_survives_json_roundtrip_for_state_persistence() {
        // Event 同样需 serde 稳定（CLI 事件流落盘）：逐一往返。
        for event in [
            Event::PlanSubmitted(dag_spec()),
            Event::PlanReviewed(plan_verdict(true)),
            Event::ExecutionDone(artifact()),
            Event::ExecReviewed(exec_correct()),
            Event::OwnerDecided(OwnerDecision::ReviseContract),
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let restored: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, event);
        }

        // run 状态持久化：序列化当前状态成 JSON（CLI 落 state.json），读回逐字节一致。
        let mut orch = drive_to(2, State::ExecReviewing);
        orch.transition(Event::ExecReviewed(exec_failed(FailureClass::Mechanical))).unwrap();
        assert_eq!(orch.state(), State::Executing);
        assert_eq!(orch.attempts(), 1);

        let json = serde_json::to_string(&orch).unwrap();
        let restored: Orchestrator = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, orch);
        assert_eq!(restored.state(), State::Executing);
        assert_eq!(restored.attempts(), 1);
    }

    #[test]
    fn orchestrator_json_rejects_unknown_fields() {
        let json = r#"{
            "state": "planning",
            "attempts": 0,
            "retry_budget": 2,
            "dag_spec": null,
            "plan_verdict": null,
            "exec_verdict": null,
            "artifact": null,
            "surprise": true
        }"#;
        assert!(serde_json::from_str::<Orchestrator>(json).is_err());
    }
}
