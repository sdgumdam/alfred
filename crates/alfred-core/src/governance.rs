//! 治理环状态机 + §3.3 分级路由（施工清单 §3.3 / §五 S1；实施计划 P12）。
//!
//! 编排器是"一段按固定规则运行的程序，不是大模型"（§2.1）——本模块是它
//! 的确定性核心：
//!
//! 1. **状态机**：治理环状态 + 事件转移。事件转移确定性、非法转移显式
//!    报错（返回 `Err` 说明哪个状态收到哪个事件不合法）、无静默出口——
//!    每条路径都经过 `apply()`，打回/升级/重跑/放弃全部显式落 audit。
//! 2. **分级路由（§3.3 表）**：`route()` 纯函数，六行全落码。执行审查
//!    结论（ExecVerdict）→ 路由决策。不存在表外第四种出口。
//! 3. **持久化**：`GovernanceRun` 落 state.json（状态机 + attempts +
//!    verdict 历史 + 会话文档 + 运行选项），供 `alfred decide` 续跑。

use serde::{Deserialize, Serialize};

use crate::request::OwnerRequest;
use crate::session::SessionDoc;
use crate::util::now_rfc3339;
use crate::verdict::{ExecVerdict, PlanVerdict};
use crate::dagspec::DagSpec;

/// 治理环状态（施工清单 §3.2 六环节 + 续跑挂起态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GovernanceState {
    /// 规划中（converse 在建图）。
    Planning,
    /// 计划审查中。
    PlanReviewing,
    /// 计划被打回（挂起，等属主 decide）。
    PlanRejected,
    /// 执行中（含 mechanical 同契约重跑）。
    Executing,
    /// 执行审查中。
    ExecReviewing,
    /// 全流程完成（验收 C 推进到终点）。
    Completed,
    /// 升级属主（挂起，等属主 decide）。
    Escalated,
    /// 属主放弃（终态）。
    Abandoned,
}

impl GovernanceState {
    /// 是否为挂起态（等属主 decide）。
    pub fn is_suspended(&self) -> bool {
        matches!(self, GovernanceState::PlanRejected | GovernanceState::Escalated)
    }

    /// 是否为终态。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            GovernanceState::Completed | GovernanceState::Abandoned
        )
    }
}

/// 状态机事件（语义：发生了什么 → 状态机判合法转移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceEvent {
    /// 规划器产出 DagSpec。
    PlanProduced,
    /// 规划侧失败（converse 出错）→ 升级属主（不悄悄放行）。
    PlanningError,
    /// 计划审查通过（PlanVerdict.pass=true）。
    PlanReviewPassed,
    /// 计划审查打回（PlanVerdict.pass=false）。
    PlanReviewRejected,
    /// 计划审查本身出错（unscored / eval error）→ 必须升级（§六继承项）。
    PlanReviewError,
    /// 执行成功（eval status success，进入执行审查）。
    ExecutionSucceeded,
    /// 执行机械失败（eval error/timeout）+ 重跑预算未耗尽 → 留在 Executing。
    ExecutionFailedRetry,
    /// 执行机械失败 + 预算耗尽 → 升级。
    ExecutionFailedEscalate,
    /// 执行审查 C（推进 → Completed）。
    ExecReviewPassed,
    /// 执行审查 I/P + mechanical + 预算未耗尽 → 回 Executing 重跑。
    ExecReviewMechanicalRetry,
    /// 执行审查 I/P + mechanical + 预算耗尽 → 升级。
    ExecReviewMechanicalEscalate,
    /// 执行审查 I/P + 语义性失败（ambiguity/dispute/fault/disagreement）→ 升级。
    ExecReviewSemanticEscalate,
    /// 执行审查本身出错（unscored）→ 升级（§六继承项）。
    ExecReviewError,
    /// 属主 decide retry。
    OwnerRetry,
    /// 属主 decide revise（改需求/改契约重新规划）。
    OwnerRevise,
    /// 属主 decide abandon。
    OwnerAbandon,
}

/// 非法转移错误（含来源状态与事件，显式报错）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: GovernanceState,
    pub event: GovernanceEvent,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal transition: event {:?} not allowed from state {:?}",
            self.event, self.from
        )
    }
}

impl std::error::Error for TransitionError {}

/// 治理环状态机（确定性事件转移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceStateMachine {
    state: GovernanceState,
}

impl Default for GovernanceStateMachine {
    fn default() -> Self {
        Self {
            state: GovernanceState::Planning,
        }
    }
}

impl GovernanceStateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> GovernanceState {
        self.state
    }

    /// 应用事件；非法转移返回 `TransitionError`（不改变状态）。
    pub fn apply(&mut self, event: GovernanceEvent) -> Result<(), TransitionError> {
        let next = transition(self.state, event)?;
        self.state = next;
        Ok(())
    }
}

/// 转移表（确定性、无静默出口）。非法组合返回 Err。
fn transition(
    from: GovernanceState,
    event: GovernanceEvent,
) -> Result<GovernanceState, TransitionError> {
    use GovernanceEvent::*;
    use GovernanceState::*;
    let next = match (from, event) {
        (Planning, PlanProduced) => PlanReviewing,
        (Planning, PlanningError) => Escalated,
        (PlanReviewing, PlanReviewPassed) => Executing,
        (PlanReviewing, PlanReviewRejected) => PlanRejected,
        (PlanReviewing, PlanReviewError) => Escalated,
        (PlanRejected, OwnerRetry) => Planning,
        (PlanRejected, OwnerRevise) => Planning,
        (PlanRejected, OwnerAbandon) => Abandoned,
        (Executing, ExecutionSucceeded) => ExecReviewing,
        (Executing, ExecutionFailedRetry) => Executing,
        (Executing, ExecutionFailedEscalate) => Escalated,
        (ExecReviewing, ExecReviewPassed) => Completed,
        (ExecReviewing, ExecReviewMechanicalRetry) => Executing,
        (ExecReviewing, ExecReviewMechanicalEscalate) => Escalated,
        (ExecReviewing, ExecReviewSemanticEscalate) => Escalated,
        (ExecReviewing, ExecReviewError) => Escalated,
        (Escalated, OwnerRetry) => Executing,
        (Escalated, OwnerRevise) => Planning,
        (Escalated, OwnerAbandon) => Abandoned,
        // 终态/挂起态对非法事件显式报错（无静默出口）。
        (Completed | Abandoned, _) => {
            return Err(TransitionError { from, event });
        }
        (_, _) => {
            return Err(TransitionError { from, event });
        }
    };
    Ok(next)
}

/// §3.3 路由决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingDecision {
    /// C → 推进（下一节点；单节点骨架即全流程完成）。
    Advance,
    /// I/P + mechanical → 按同一契约重跑执行（预算 N=2，耗尽升级）。
    MechanicalRetry,
    /// I/P + 语义性失败 → 直接升级属主。`suggest_contract_change` 为
    /// contract_fault 的预标注"建议改契约"。
    Escalate { suggest_contract_change: bool },
}

/// §3.3 分级路由表（纯函数，六行全落码，不存在表外第四种出口）。
///
/// 入参为已验证的 ExecVerdict（`value==C` 时 `failure_class==None`；
/// `value==I/P` 时 `failure_class==Some`）。违反不变量返回 Err。
pub fn route(verdict: &ExecVerdict) -> Result<RoutingDecision, String> {
    verdict.validate()?;
    use crate::verdict::FailureClass::*;
    use crate::verdict::VerdictGrade::*;
    match (verdict.value, verdict.failure_class) {
        // C（通过）— 推进
        (C, None) => Ok(RoutingDecision::Advance),
        // I/P + mechanical — 同契约重跑，预算耗尽升级
        (I | P, Some(Mechanical)) => Ok(RoutingDecision::MechanicalRetry),
        // I/P + contract_ambiguity — 直接升级（程序改不了文字歧义）
        (I | P, Some(ContractAmbiguity)) => Ok(RoutingDecision::Escalate {
            suggest_contract_change: false,
        }),
        // I/P + fidelity_dispute — 直接升级（语义判断，程序判不了）
        (I | P, Some(FidelityDispute)) => Ok(RoutingDecision::Escalate {
            suggest_contract_change: false,
        }),
        // I/P + contract_fault — 直接升级 + 预标注"建议改契约"
        (I | P, Some(ContractFault)) => Ok(RoutingDecision::Escalate {
            suggest_contract_change: true,
        }),
        // I/P + disagreement — 直接升级（审查层自己都没达成一致）
        (I | P, Some(Disagreement)) => Ok(RoutingDecision::Escalate {
            suggest_contract_change: false,
        }),
        (grade, fc) => Err(format!(
            "route: invariant violation grade={grade:?} failure_class={fc:?}"
        )),
    }
}

/// 属主决策（§3.2 环节 3/6：重跑 / 改契约重新规划 / 放弃）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerDecision {
    Retry,
    Revise,
    Abandon,
}

/// 治理环运行选项（续跑所需，落 state.json）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceOptions {
    /// 沙箱镜像。
    pub image: String,
    /// 执行 eval 单样本时间上限（秒）。
    pub exec_time_limit_secs: u32,
    /// 计划审查 eval 单样本时间上限（秒）。
    pub review_time_limit_secs: u32,
    /// 桥代理端口基数。
    pub port_base: u32,
    /// settled 后宽限（秒）。
    pub settle_grace_seconds: f64,
    /// 是否轮询 `inspect ctl` 观测面。
    pub ctl_enabled: bool,
}

impl Default for GovernanceOptions {
    fn default() -> Self {
        Self {
            image: "alfred-executor:latest".to_string(),
            exec_time_limit_secs: 600,
            review_time_limit_secs: 300,
            port_base: 13100,
            settle_grace_seconds: 20.0,
            ctl_enabled: true,
        }
    }
}

/// 治理环运行态（落 run_dir/state.json，供 decide 续跑）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceRun {
    pub run_id: String,
    pub request: OwnerRequest,
    pub state_machine: GovernanceStateMachine,
    /// mechanical 重跑已用预算。
    pub attempts_used: u32,
    /// mechanical 重跑预算（§3.3：N=2）。
    pub mechanical_budget: u32,
    /// 执行尝试绝对计数（含属主重跑；exec 子目录按此编号，避免跨周期碰撞）。
    #[serde(default)]
    pub execution_count: u32,
    /// 当前计划（converse 产出；重规划时被替换）。
    pub dagspec: Option<DagSpec>,
    /// 会话文档（maintain 维护，converse 每轮喂最新）。
    pub session_doc: SessionDoc,
    /// 计划审查结论历史。
    #[serde(default)]
    pub plan_verdicts: Vec<PlanVerdict>,
    /// 执行审查结论历史。
    #[serde(default)]
    pub exec_verdicts: Vec<ExecVerdict>,
    /// 当前属主消息（初始为 None → converse 用 request；重规划/改需求时为 Some）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_message: Option<String>,
    /// 运行选项（decide 续跑用）。
    pub options: GovernanceOptions,
    pub updated_at: String,
}

impl GovernanceRun {
    pub fn new(run_id: impl Into<String>, request: OwnerRequest, options: GovernanceOptions) -> Self {
        Self {
            run_id: run_id.into(),
            request,
            state_machine: GovernanceStateMachine::new(),
            attempts_used: 0,
            mechanical_budget: 2,
            execution_count: 0,
            dagspec: None,
            session_doc: SessionDoc::new(),
            plan_verdicts: Vec::new(),
            exec_verdicts: Vec::new(),
            owner_message: None,
            options,
            updated_at: now_rfc3339(),
        }
    }

    pub fn state(&self) -> GovernanceState {
        self.state_machine.state()
    }

    /// mechanical 预算是否耗尽。
    pub fn mechanical_exhausted(&self) -> bool {
        self.attempts_used >= self.mechanical_budget
    }

    /// 应用状态机事件并更新时间戳。
    pub fn apply(&mut self, event: GovernanceEvent) -> Result<(), TransitionError> {
        self.state_machine.apply(event)?;
        // P2 修复：进入 Planning（重规划周期开始）→ 重置 mechanical 重跑预算。
        // 覆盖 PlanRejected+retry / PlanRejected+revise / Escalated+revise 三条
        // 重规划路径；Escalated+retry（重入执行）在 decide 侧重置。
        if self.state() == GovernanceState::Planning {
            self.attempts_used = 0;
        }
        self.updated_at = now_rfc3339();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::{Confidence, FailureClass, VerdictGrade};

    fn exec_verdict(value: VerdictGrade, fc: Option<FailureClass>) -> ExecVerdict {
        ExecVerdict::new(value, fc, Confidence::High, vec![], "x").unwrap()
    }

    #[test]
    fn initial_state_is_planning() {
        let sm = GovernanceStateMachine::new();
        assert_eq!(sm.state(), GovernanceState::Planning);
    }

    #[test]
    fn happy_path_full_loop() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        assert_eq!(sm.state(), GovernanceState::PlanReviewing);
        sm.apply(GovernanceEvent::PlanReviewPassed).unwrap();
        assert_eq!(sm.state(), GovernanceState::Executing);
        sm.apply(GovernanceEvent::ExecutionSucceeded).unwrap();
        assert_eq!(sm.state(), GovernanceState::ExecReviewing);
        sm.apply(GovernanceEvent::ExecReviewPassed).unwrap();
        assert_eq!(sm.state(), GovernanceState::Completed);
        assert!(sm.state().is_terminal());
    }

    #[test]
    fn plan_rejection_then_owner_retry_replans() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewRejected).unwrap();
        assert_eq!(sm.state(), GovernanceState::PlanRejected);
        assert!(sm.state().is_suspended());
        sm.apply(GovernanceEvent::OwnerRetry).unwrap();
        assert_eq!(sm.state(), GovernanceState::Planning);
    }

    #[test]
    fn plan_rejection_then_owner_abandon_terminates() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewRejected).unwrap();
        sm.apply(GovernanceEvent::OwnerAbandon).unwrap();
        assert_eq!(sm.state(), GovernanceState::Abandoned);
        assert!(sm.state().is_terminal());
    }

    #[test]
    fn review_error_escalates() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewError).unwrap();
        assert_eq!(sm.state(), GovernanceState::Escalated);
        assert!(sm.state().is_suspended());
    }

    #[test]
    fn mechanical_retry_keeps_executing_then_escalates() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewPassed).unwrap();
        sm.apply(GovernanceEvent::ExecutionFailedRetry).unwrap();
        assert_eq!(sm.state(), GovernanceState::Executing);
        sm.apply(GovernanceEvent::ExecutionFailedEscalate).unwrap();
        assert_eq!(sm.state(), GovernanceState::Escalated);
    }

    #[test]
    fn escalted_owner_retry_reenters_execution() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewError).unwrap();
        assert_eq!(sm.state(), GovernanceState::Escalated);
        sm.apply(GovernanceEvent::OwnerRetry).unwrap();
        assert_eq!(sm.state(), GovernanceState::Executing);
    }

    #[test]
    fn escalted_owner_revise_replans() {
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewError).unwrap();
        assert_eq!(sm.state(), GovernanceState::Escalated);
        sm.apply(GovernanceEvent::OwnerRevise).unwrap();
        assert_eq!(sm.state(), GovernanceState::Planning);
    }

    #[test]
    fn illegal_transition_errors_explicitly() {
        // ExecReviewing 收到 PlanProduced → 非法
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanProduced).unwrap();
        sm.apply(GovernanceEvent::PlanReviewPassed).unwrap();
        sm.apply(GovernanceEvent::ExecutionSucceeded).unwrap();
        let err = sm.apply(GovernanceEvent::PlanProduced).unwrap_err();
        assert_eq!(err.from, GovernanceState::ExecReviewing);
        assert_eq!(err.event, GovernanceEvent::PlanProduced);
        assert!(format!("{err}").contains("illegal transition"));

        // 终态 Completed 不接受任何事件
        sm.apply(GovernanceEvent::ExecReviewPassed).unwrap();
        assert!(sm.apply(GovernanceEvent::OwnerRetry).is_err());
        assert!(sm.apply(GovernanceEvent::PlanProduced).is_err());
    }

    #[test]
    fn route_table_all_rows() {
        use RoutingDecision::*;
        // C → 推进
        assert_eq!(route(&exec_verdict(VerdictGrade::C, None)).unwrap(), Advance);
        // I/P + mechanical → 重跑
        assert_eq!(
            route(&exec_verdict(VerdictGrade::I, Some(FailureClass::Mechanical))).unwrap(),
            MechanicalRetry
        );
        assert_eq!(
            route(&exec_verdict(VerdictGrade::P, Some(FailureClass::Mechanical))).unwrap(),
            MechanicalRetry
        );
        // I/P + contract_ambiguity → 升级（无改契约标注）
        assert_eq!(
            route(&exec_verdict(VerdictGrade::I, Some(FailureClass::ContractAmbiguity))).unwrap(),
            Escalate {
                suggest_contract_change: false
            }
        );
        // I/P + fidelity_dispute → 升级（无改契约标注）
        assert_eq!(
            route(&exec_verdict(VerdictGrade::P, Some(FailureClass::FidelityDispute))).unwrap(),
            Escalate {
                suggest_contract_change: false
            }
        );
        // I/P + contract_fault → 升级 + 预标注"建议改契约"
        assert_eq!(
            route(&exec_verdict(VerdictGrade::I, Some(FailureClass::ContractFault))).unwrap(),
            Escalate {
                suggest_contract_change: true
            }
        );
        // I/P + disagreement → 升级（无改契约标注）
        assert_eq!(
            route(&exec_verdict(VerdictGrade::P, Some(FailureClass::Disagreement))).unwrap(),
            Escalate {
                suggest_contract_change: false
            }
        );
    }

    #[test]
    fn route_rejects_invariant_violation() {
        // C 带 failure_class → route 拒绝（不变量）
        let bad = ExecVerdict::new(
            VerdictGrade::C,
            Some(FailureClass::Mechanical),
            Confidence::High,
            vec![],
            "x",
        );
        assert!(bad.is_err());
    }

    #[test]
    fn governance_run_budget_tracking() {
        let mut run = GovernanceRun::new(
            "run-1",
            OwnerRequest::new("req-1", "t", "d", "a"),
            GovernanceOptions::default(),
        );
        assert!(!run.mechanical_exhausted());
        run.attempts_used = 1;
        assert!(!run.mechanical_exhausted());
        run.attempts_used = 2;
        assert!(run.mechanical_exhausted());
    }
    #[test]
    fn replanning_resets_mechanical_budget() {
        // P2 修复：PlanRejected + OwnerRetry 重规划 → 进入 Planning → attempts 重置为 0
        let mut run = GovernanceRun::new(
            "run-1",
            OwnerRequest::new("req-1", "t", "d", "a"),
            GovernanceOptions::default(),
        );
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        run.apply(GovernanceEvent::PlanReviewRejected).unwrap();
        run.attempts_used = 2; // 模拟已耗尽的预算
        run.apply(GovernanceEvent::OwnerRetry).unwrap();
        assert_eq!(run.state(), GovernanceState::Planning);
        assert_eq!(run.attempts_used, 0, "重规划周期应重置 mechanical 预算");
        assert!(!run.mechanical_exhausted());
    }

    #[test]
    fn owner_revise_replans_and_resets_budget() {
        // P2 修复：PlanRejected + OwnerRevise 与 Escalated + OwnerRevise → 重置 attempts
        let mut run = GovernanceRun::new(
            "run-2",
            OwnerRequest::new("req-1", "t", "d", "a"),
            GovernanceOptions::default(),
        );
        run.apply(GovernanceEvent::PlanProduced).unwrap();
        run.apply(GovernanceEvent::PlanReviewRejected).unwrap();
        run.attempts_used = 1;
        run.apply(GovernanceEvent::OwnerRevise).unwrap();
        assert_eq!(run.state(), GovernanceState::Planning);
        assert_eq!(run.attempts_used, 0);

        let mut run2 = GovernanceRun::new(
            "run-3",
            OwnerRequest::new("req-1", "t", "d", "a"),
            GovernanceOptions::default(),
        );
        run2.apply(GovernanceEvent::PlanProduced).unwrap();
        run2.apply(GovernanceEvent::PlanReviewError).unwrap();
        run2.attempts_used = 1;
        run2.apply(GovernanceEvent::OwnerRevise).unwrap();
        assert_eq!(run2.state(), GovernanceState::Planning);
        assert_eq!(run2.attempts_used, 0);
    }

    #[test]
    fn planning_error_escalates() {
        // P3 修复：规划侧失败（converse 出错）→ 升级属主，不悄悄放行
        let mut sm = GovernanceStateMachine::new();
        sm.apply(GovernanceEvent::PlanningError).unwrap();
        assert_eq!(sm.state(), GovernanceState::Escalated);
        assert!(sm.state().is_suspended());
    }
}
