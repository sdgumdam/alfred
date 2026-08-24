use crate::entities::{FailureClass, VerdictValue};
use crate::verdict::ExecVerdict;

/// 分级路由的唯一出口集合：任何 (value, failure_class) 组合只落三选一，无第四出口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAction {
    /// 推进下一节点；若是末节点则由 orchestrator 落成 Completed。
    Advance,
    /// 同契约重跑本节点。attempt 为即将执行的第几次（1-based）。
    Retry { attempt: u32 },
    /// 升级属主拍板。contract_fault 预标注建议改契约。
    Escalate { suggest_contract_change: bool },
}

/// 确定性路由查表（施工清单 3.3）：
/// - C → Advance
/// - I/P + mechanical，预算未耗尽 → Retry
/// - I/P + mechanical，预算耗尽 → Escalate（不建议改契约）
/// - I/P + contract_ambiguity / fidelity_dispute / disagreement → Escalate
/// - I/P + contract_fault → Escalate 且预标注"建议改契约"
/// - 非 C 但无 failure_class → Escalate（无分类不给重跑，避免盲目重试）
///
/// 纯函数：无 LLM、无随机、无系统时间。
pub fn route(verdict: &ExecVerdict, retry_budget: u32, attempts_so_far: u32) -> RouteAction {
    if verdict.value == VerdictValue::C {
        return RouteAction::Advance;
    }
    match verdict.failure_class {
        Some(FailureClass::Mechanical) => {
            if attempts_so_far < retry_budget {
                RouteAction::Retry { attempt: attempts_so_far + 1 }
            } else {
                RouteAction::Escalate { suggest_contract_change: false }
            }
        }
        Some(FailureClass::ContractFault) => RouteAction::Escalate { suggest_contract_change: true },
        Some(FailureClass::ContractAmbiguity)
        | Some(FailureClass::FidelityDispute)
        | Some(FailureClass::Disagreement)
        | None => RouteAction::Escalate { suggest_contract_change: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entities::Confidence;

    fn verdict(value: VerdictValue, failure_class: Option<FailureClass>) -> ExecVerdict {
        ExecVerdict {
            value,
            failure_class,
            confidence: Confidence::High,
            evidence: vec![],
            explanation: String::new(),
        }
    }

    #[test]
    fn correct_verdict_advances() {
        assert_eq!(route(&verdict(VerdictValue::C, None), 2, 0), RouteAction::Advance);
        // C 即使误带 failure_class 也一律 Advance（通过优先，不翻旧账）。
        assert_eq!(
            route(&verdict(VerdictValue::C, Some(FailureClass::Mechanical)), 2, 0),
            RouteAction::Advance
        );
    }

    #[test]
    fn mechanical_within_budget_retries_with_incremented_attempt() {
        assert_eq!(
            route(&verdict(VerdictValue::I, Some(FailureClass::Mechanical)), 2, 0),
            RouteAction::Retry { attempt: 1 }
        );
        assert_eq!(
            route(&verdict(VerdictValue::P, Some(FailureClass::Mechanical)), 2, 1),
            RouteAction::Retry { attempt: 2 }
        );
    }

    #[test]
    fn mechanical_budget_exhausted_escalates_without_contract_change() {
        assert_eq!(
            route(&verdict(VerdictValue::I, Some(FailureClass::Mechanical)), 2, 2),
            RouteAction::Escalate { suggest_contract_change: false }
        );
    }

    #[test]
    fn contract_fault_escalates_with_contract_change_suggestion() {
        assert_eq!(
            route(&verdict(VerdictValue::I, Some(FailureClass::ContractFault)), 2, 0),
            RouteAction::Escalate { suggest_contract_change: true }
        );
    }

    #[test]
    fn ambiguity_dispute_disagreement_escalate_without_contract_change() {
        for class in [
            FailureClass::ContractAmbiguity,
            FailureClass::FidelityDispute,
            FailureClass::Disagreement,
        ] {
            assert_eq!(
                route(&verdict(VerdictValue::P, Some(class)), 2, 0),
                RouteAction::Escalate { suggest_contract_change: false },
                "{class:?} must escalate without retry"
            );
        }
    }

    #[test]
    fn unclassified_failure_escalates_without_retry() {
        assert_eq!(
            route(&verdict(VerdictValue::I, None), 2, 0),
            RouteAction::Escalate { suggest_contract_change: false }
        );
    }
}
