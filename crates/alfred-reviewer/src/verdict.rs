//! verdict.json 输出契约（R6c）：reviewer 容器产出 → 宿主 Pydantic 等价校验。
//!
//! 旧 eval 内嵌 scorer 的资产（Pydantic 校验 / [BEGIN DATA] 中和 / unscored 兜底）
//! 迁入 reviewer 容器路径后，宿主侧读 `/outputs/verdict.json`，用 Rust serde
//! （等价 Pydantic 校验）解析为 alfred-core 的 `PlanVerdict` / `ExecVerdict`。
//!
//! 输出契约（容器内 pi 写，宿主读）：
//! - 计划审查：`{"pass": bool, "reason": "简述"}`
//! - 执行审查：`{"grade": "C"|"I"|"P", "failure_class": "contract_ambiguity"|"fidelity_dispute"|"contract_fault"|null, "rationale": "简述"}`
//!
//! 校验语义（对齐旧 Pydantic scorer + R2Audit2 不变量）：
//! - `ExecVerdict::new` 强制 §3.3 不变量（C→failure_class None；I/P→failure_class Some）。
//! - 解析失败/非法 → `Err`（调用方落 unscored，升级属主，不悄悄放行）。

use alfred_core::verdict::{Confidence, ExecVerdict, FailureClass, PlanVerdict, VerdictGrade};
use serde::{Deserialize, Serialize};

/// 计划审查 verdict 文档（容器产出，宿主解析）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVerdictDoc {
    pub pass: bool,
    pub reason: String,
}

/// 执行审查 verdict 文档（容器产出，宿主解析）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecVerdictDoc {
    pub grade: String,
    #[serde(default)]
    pub failure_class: Option<String>,
    pub rationale: String,
}

/// 解析容器产出的计划审查 verdict JSON → `PlanVerdict`。
///
/// reason 为空 / 缺字段 / 未知字段（deny_unknown_fields）→ `Err`（unscored 兜底）。
pub fn parse_plan_verdict_json(text: &str) -> Result<PlanVerdict, String> {
    let doc: PlanVerdictDoc = serde_json::from_str(text)
        .map_err(|e| format!("parse plan verdict JSON: {e}"))?;
    if doc.reason.trim().is_empty() {
        return Err("plan verdict reason is empty".into());
    }
    Ok(PlanVerdict::new(doc.pass, doc.reason))
}

/// 解析容器产出的执行审查 verdict JSON → `ExecVerdict`。
///
/// 未知 grade / failure_class、或违反 §3.3 不变量（C 带 failure_class、I/P 缺
/// failure_class）→ `Err`（unscored 兜底 + 升级属主）。
pub fn parse_exec_verdict_json(text: &str) -> Result<ExecVerdict, String> {
    let doc: ExecVerdictDoc = serde_json::from_str(text)
        .map_err(|e| format!("parse exec verdict JSON: {e}"))?;
    if doc.rationale.trim().is_empty() {
        return Err("exec verdict rationale is empty".into());
    }
    let grade = match doc.grade.as_str() {
        "C" => VerdictGrade::C,
        "I" => VerdictGrade::I,
        "P" => VerdictGrade::P,
        other => return Err(format!("unknown exec verdict grade: {other}")),
    };
    let failure_class = match doc.failure_class.as_deref() {
        None => None,
        Some("contract_ambiguity") => Some(FailureClass::ContractAmbiguity),
        Some("fidelity_dispute") => Some(FailureClass::FidelityDispute),
        Some("contract_fault") => Some(FailureClass::ContractFault),
        Some(other) => return Err(format!("unknown exec verdict failure_class: {other}")),
    };
    ExecVerdict::new(grade, failure_class, Confidence::High, vec![], doc.rationale)
        .map_err(|e| format!("exec verdict invariant violation: {e}"))
}

/// `[BEGIN DATA]` / `[END DATA]` 中和（FinalAudit P3 资产迁入 reviewer 容器路径）。
///
/// 数据块边界只能由审查模板持有：内容中出现同名标记会伪造数据块边界（注入/
/// 中和绕过）。插入前把标记改写为无冲突形式。容器路径中 driver prompt 不内嵌
/// 内容（pi 直接读文件），此函数为任何需要把内容嵌进 prompt 的兜底路径保留。
pub fn neutralize_data_markers(text: &str) -> String {
    text.replace("[BEGIN DATA]", "[BEGIN_DATA]")
        .replace("[END DATA]", "[END_DATA]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plan_verdict_pass() {
        let v = parse_plan_verdict_json(r#"{"pass": true, "reason": "covers all"}"#).unwrap();
        assert!(v.pass);
        assert_eq!(v.reason, "covers all");
    }

    #[test]
    fn parse_plan_verdict_fail() {
        let v = parse_plan_verdict_json(r#"{"pass": false, "reason": "does task B"}"#).unwrap();
        assert!(!v.pass);
    }

    #[test]
    fn parse_plan_verdict_rejects_empty_reason() {
        let err = parse_plan_verdict_json(r#"{"pass": true, "reason": ""}"#).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn parse_plan_verdict_rejects_unknown_fields() {
        assert!(parse_plan_verdict_json(r#"{"pass": true, "reason": "x", "future": 1}"#).is_err());
    }

    #[test]
    fn parse_plan_verdict_rejects_bad_json() {
        assert!(parse_plan_verdict_json("not json").is_err());
    }

    #[test]
    fn parse_exec_verdict_grade_c_no_failure_class() {
        let v = parse_exec_verdict_json(
            r#"{"grade": "C", "failure_class": null, "rationale": "fully satisfied"}"#,
        )
        .unwrap();
        assert_eq!(v.value, VerdictGrade::C);
        assert_eq!(v.failure_class, None);
        assert_eq!(v.explanation, "fully satisfied");
        assert_eq!(v.confidence, Confidence::High);
    }

    #[test]
    fn parse_exec_verdict_grade_i_with_failure_class() {
        let v = parse_exec_verdict_json(
            r#"{"grade": "I", "failure_class": "fidelity_dispute", "rationale": "off target"}"#,
        )
        .unwrap();
        assert_eq!(v.value, VerdictGrade::I);
        assert_eq!(v.failure_class, Some(FailureClass::FidelityDispute));
    }

    #[test]
    fn parse_exec_verdict_grade_p_with_failure_class() {
        // R2Audit2：P 档必须有 failure_class
        let v = parse_exec_verdict_json(
            r#"{"grade": "P", "failure_class": "contract_fault", "rationale": "partial"}"#,
        )
        .unwrap();
        assert_eq!(v.value, VerdictGrade::P);
        assert_eq!(v.failure_class, Some(FailureClass::ContractFault));
    }

    #[test]
    fn parse_exec_verdict_rejects_c_with_failure_class() {
        // 不变量：C 必须 failure_class=None
        let err = parse_exec_verdict_json(
            r#"{"grade": "C", "failure_class": "fidelity_dispute", "rationale": "x"}"#,
        )
        .unwrap_err();
        assert!(err.contains("invariant"), "{err}");
    }

    #[test]
    fn parse_exec_verdict_rejects_i_without_failure_class() {
        // 不变量：I/P 必须 failure_class
        let err = parse_exec_verdict_json(r#"{"grade": "I", "failure_class": null, "rationale": "x"}"#)
            .unwrap_err();
        assert!(err.contains("invariant"), "{err}");
    }

    #[test]
    fn parse_exec_verdict_rejects_unknown_grade() {
        let err = parse_exec_verdict_json(
            r#"{"grade": "X", "failure_class": null, "rationale": "x"}"#,
        )
        .unwrap_err();
        assert!(err.contains("unknown exec verdict grade"), "{err}");
    }

    #[test]
    fn parse_exec_verdict_rejects_unknown_failure_class() {
        let err = parse_exec_verdict_json(
            r#"{"grade": "I", "failure_class": "mechanical", "rationale": "x"}"#,
        )
        .unwrap_err();
        assert!(err.contains("unknown exec verdict failure_class"), "{err}");
    }

    #[test]
    fn neutralize_data_markers_rewrites_boundaries() {
        assert_eq!(
            neutralize_data_markers("[BEGIN DATA] x [END DATA]"),
            "[BEGIN_DATA] x [END_DATA]"
        );
        assert_eq!(neutralize_data_markers("no markers"), "no markers");
    }
}
