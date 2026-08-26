//! 审查结论实体（施工清单 §3.3 ExecVerdict；限界上下文 §6.9 ReviewScore）。
//!
//! R1 定义 ExecVerdict 实体（R2 的 Inspect scorer 才产出）；分流规则见
//! §3.3 表。R2 增补 PlanVerdict（计划审查结论：计划是否忠实于属主需求）。

use serde::{Deserialize, Serialize};

/// 审查等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VerdictGrade {
    /// 通过
    C,
    /// 不通过
    I,
    /// 部分通过
    P,
}

/// 失败原因分类（§3.3 分流表的唯一依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// 机械性失败：环境、工具故障，不是契约或逻辑问题。
    ///
    /// 单 grader 结构不可观测——不由 grader 判；由编排器按执行状态判定
    /// （eval error/timeout → mechanical，R3 接续）。grading prompt 不列。
    Mechanical,
    /// 契约本身写得有歧义。
    ContractAmbiguity,
    /// 产物与契约之间有争议，是否兑现说法不一。
    FidelityDispute,
    /// 产物忠实于契约，但契约偏离了属主本意。
    ContractFault,
    /// 多个审查者意见分歧、没有多数结论。
    ///
    /// 单 grader 结构不可观测——留多 grader 未来（多 grader 独立判分后
    /// 无多数结论才判）。grading prompt 不列。
    Disagreement,
}

/// 置信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// 执行审查结论（§3.3：value / failure_class / confidence / evidence / explanation）。
///
/// 不变量（§3.3 路由表）：`value == C` 时 `failure_class` 必须为 `None`；
/// `value == I/P` 时 `failure_class` 必须为 `Some`。`new()` 强制该约束，
/// 避免产生路由表之外的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecVerdict {
    pub value: VerdictGrade,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    pub confidence: Confidence,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub explanation: String,
}

impl ExecVerdict {
    /// 构造并校验不变量。`value == C` 时 `failure_class` 必须为 `None`，
    /// 否则返回错误说明。
    pub fn new(
        value: VerdictGrade,
        failure_class: Option<FailureClass>,
        confidence: Confidence,
        evidence: Vec<String>,
        explanation: impl Into<String>,
    ) -> Result<Self, String> {
        let verdict = Self {
            value,
            failure_class,
            confidence,
            evidence,
            explanation: explanation.into(),
        };
        verdict.validate()?;
        Ok(verdict)
    }

    /// 校验 §3.3 不变量。
    pub fn validate(&self) -> Result<(), String> {
        match (self.value, self.failure_class) {
            (VerdictGrade::C, Some(_)) => Err(
                "ExecVerdict invariant violated: grade C must have failure_class = None".into(),
            ),
            (VerdictGrade::I | VerdictGrade::P, None) => Err(
                "ExecVerdict invariant violated: grade I/P requires failure_class".into(),
            ),
            _ => Ok(()),
        }
    }
}

/// 计划审查结论（R2）：计划是否忠实于属主需求。
///
/// pass=true：计划忠实于 OwnerRequest；pass=false：打回重规划（P7 在 R3
/// 把 reason 转写为属主口吻喂回 planner）。由计划审查 scorer 产出，经
/// eval log 结构化读取落盘。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVerdict {
    pub pass: bool,
    pub reason: String,
}

impl PlanVerdict {
    pub fn new(pass: bool, reason: impl Into<String>) -> Self {
        Self {
            pass,
            reason: reason.into(),
        }
    }
}
