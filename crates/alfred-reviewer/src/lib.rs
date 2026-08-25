//! alfred-reviewer：审查者（R2/R3 实现；R1 阶段为占位 crate）。
//!
//! 职责见施工清单 §3.1：Inspect AI 打分模块，异构多模型独立判分。
//! 计划审查（DagSpec vs OwnerRequest 忠实度）与执行审查（Contract 验收
//! 标准判 Artifact）都走 `inspect eval`；R2 起实现 ExecVerdict scorer。

/// R1 占位。R2 起此 crate 承载 ExecVerdict / PlanVerdict scorer。
pub fn _placeholder() -> &'static str {
    "alfred-reviewer: R2"
}
