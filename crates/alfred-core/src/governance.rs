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
//!    verdict 历史 + 会话文档 + 运行选项），供库调用方（codux driver）续跑。

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

/// 升级来源（Escalated + OwnerRetry 的路由依据，P1 修复）。
///
/// 升级属主时记录来源；属主 decide retry 时按来源路由：
/// - Execution  → 重入 Executing（R3 现行为）；
/// - PlanReview → 回 PlanReviewing（重新审同一计划，不绕计划审查闸门）；
/// - Planning   → 回 Planning（重新规划）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSource {
    /// 规划侧失败（converse 出错）升级。
    Planning,
    /// 计划审查本身出错（unscored / eval error）升级。
    PlanReview,
    /// 执行 / 执行审查失败升级。
    Execution,
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
    ///
    /// 裸状态机不知道升级来源：`(Escalated, OwnerRetry)` 走缺省路由
    /// （→ Executing，旧 run 兼容）。带来源的路由用 `apply_with_source`。
    pub fn apply(&mut self, event: GovernanceEvent) -> Result<(), TransitionError> {
        let next = transition(self.state, event, None)?;
        self.state = next;
        Ok(())
    }

    /// 应用事件（带升级来源）：`(Escalated, OwnerRetry)` 按来源路由。
    pub fn apply_with_source(
        &mut self,
        event: GovernanceEvent,
        escalation_source: Option<EscalationSource>,
    ) -> Result<(), TransitionError> {
        let next = transition(self.state, event, escalation_source)?;
        self.state = next;
        Ok(())
    }
}

/// 转移表（确定性、无静默出口）。非法组合返回 Err。
///
/// `escalation_source` 仅供 `(Escalated, OwnerRetry)` 一行按升级来源路由；
/// 其余行不读它（传 None 即可）。
fn transition(
    from: GovernanceState,
    event: GovernanceEvent,
    escalation_source: Option<EscalationSource>,
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
        // P1 修复：Escalated + OwnerRetry 按升级来源路由。缺省/旧 run（无来源
        // 字段）→ 重入执行（R3 现行为，向后兼容）。
        (Escalated, OwnerRetry) => match escalation_source {
            Some(EscalationSource::PlanReview) => PlanReviewing,
            Some(EscalationSource::Planning) => Planning,
            Some(EscalationSource::Execution) | None => Executing,
        },
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
    /// 规划（planner 容器）eval 单样本时间上限（秒）。R6b 起 planner 容器化。
    #[serde(default = "default_planner_time_limit")]
    pub planner_time_limit_secs: u32,
    /// 桥代理端口基数。
    pub port_base: u32,
    /// settled 后宽限（秒）。
    pub settle_grace_seconds: f64,
    /// 兼容保留（inspect ctl 已随去 eval 退役，当前无观测面轮询）。
    pub ctl_enabled: bool,
}

/// `planner_time_limit_secs` 缺省值（旧 state.json 无此字段时反序列化兜底）。
fn default_planner_time_limit() -> u32 {
    600
}

impl Default for GovernanceOptions {
    fn default() -> Self {
        Self {
            image: "alfred-executor:latest".to_string(),
            exec_time_limit_secs: 600,
            review_time_limit_secs: 300,
            planner_time_limit_secs: 600,
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
    /// 最近一次升级的来源（P1 修复：Escalated + OwnerRetry 按来源路由）。
    /// 旧 run 目录无此字段 → 反序列化缺省 None → 路由按 Execution（向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_source: Option<EscalationSource>,
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
            escalation_source: None,
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
    ///
    /// 升级事件（PlanningError / PlanReviewError / Execution*Escalate /
    /// ExecReviewError）先落升级来源，供 Escalated + OwnerRetry 按来源路由。
    pub fn apply(&mut self, event: GovernanceEvent) -> Result<(), TransitionError> {
        if let Some(source) = escalation_source_for(event) {
            self.escalation_source = Some(source);
        }
        self.state_machine
            .apply_with_source(event, self.escalation_source)?;
        // P2 修复：进入 Planning（重规划周期开始）→ 重置 mechanical 重跑预算。
        // 覆盖 PlanRejected+retry / PlanRejected+revise / Escalated+revise / Escalated+retry(→Planning)
        // 四条重规划路径；Escalated+retry（重入执行/重审计划）在 decide 侧重置 attempts。
        if self.state() == GovernanceState::Planning {
            self.attempts_used = 0;
        }
        self.updated_at = now_rfc3339();
        Ok(())
    }
}

/// 升级事件 → 升级来源（GovernanceRun::apply 落记录，供 Escalated+OwnerRetry 路由）。
fn escalation_source_for(event: GovernanceEvent) -> Option<EscalationSource> {
    use GovernanceEvent::*;
    match event {
        PlanningError => Some(EscalationSource::Planning),
        PlanReviewError => Some(EscalationSource::PlanReview),
        ExecutionFailedEscalate
        | ExecReviewMechanicalEscalate
        | ExecReviewSemanticEscalate
        | ExecReviewError => Some(EscalationSource::Execution),
        _ => None,
    }
}
