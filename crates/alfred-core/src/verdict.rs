use crate::entities::{Confidence, FailureClass, VerdictValue};
use serde::{Deserialize, Serialize};

// 治理环裁决实体的唯一真源。S0 曾把 ExecVerdict/PlanVerdict 放在 entities.rs，
// S1 起收敛到本文件：VerdictValue/FailureClass/Confidence 枚举继续复用 entities.rs，
// 但裁决结构体只此一份，禁止在其他 crate 重定义。

/// 计划审查裁决：审查者只回答"过/不过 + 理由"。
/// 通过则进入执行；不通过则挂起等待属主拍板。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVerdict {
    pub pass: bool,
    pub reason: String,
}

/// 执行审查裁决：reviewer 对节点产出的判定。
/// value == C 时无失败分类；失败时 failure_class 必填，供 orchestrator 确定性路由。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecVerdict {
    pub value: VerdictValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    pub confidence: Confidence,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub explanation: String,
}

impl ExecVerdict {
    /// 构造一个 C（correct）裁决：无失败分类。
    pub fn correct(confidence: Confidence, evidence: Vec<String>, explanation: String) -> Self {
        Self { value: VerdictValue::C, failure_class: None, confidence, evidence, explanation }
    }

    /// 构造一个失败裁决：failure_class 必填。
    pub fn failed(
        value: VerdictValue,
        failure_class: FailureClass,
        confidence: Confidence,
        evidence: Vec<String>,
        explanation: String,
    ) -> Self {
        debug_assert!(value != VerdictValue::C, "correct verdict must not carry failure_class");
        Self {
            value,
            failure_class: Some(failure_class),
            confidence,
            evidence,
            explanation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_verdict_roundtrips_through_json() {
        let verdict = PlanVerdict { pass: false, reason: "dag 缺少 entrypoint".into() };
        let json = serde_json::to_string(&verdict).expect("serializes");
        let restored: PlanVerdict = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(restored, verdict);
    }

    #[test]
    fn plan_verdict_rejects_unknown_fields() {
        let json = r#"{"pass":true,"reason":"ok","extra":"field"}"#;
        let err =
            serde_json::from_str::<PlanVerdict>(json).expect_err("strict schema rejects unknown field");
        assert!(err.to_string().contains("extra"));
    }

    #[test]
    fn exec_verdict_correct_carries_no_failure_class() {
        let verdict =
            ExecVerdict::correct(Confidence::High, vec!["all tests pass".into()], "done".into());
        assert_eq!(verdict.value, VerdictValue::C);
        assert_eq!(verdict.failure_class, None);
        let json = serde_json::to_string(&verdict).expect("serializes");
        assert!(!json.contains("failure_class"), "C verdict omits failure_class");
    }

    #[test]
    fn exec_verdict_failed_requires_failure_class() {
        let verdict = ExecVerdict::failed(
            VerdictValue::I,
            FailureClass::Mechanical,
            Confidence::Medium,
            vec!["file missing".into()],
            "output file not produced".into(),
        );
        let json = serde_json::to_string(&verdict).expect("serializes");
        let restored: ExecVerdict = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(restored, verdict);
    }
}
