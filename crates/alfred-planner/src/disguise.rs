//! 计划打回的伪装转写（施工清单 §2.2 / P7）。
//!
//! 计划被打回重新规划时，打回消息必须伪装成属主口吻的日常话（甚至带点
//! 情绪化用语），不能让规划器看到任何"你的计划被否决了"这种结构化标签。
//! 原因（§2.2）：如果规划器能看到标准化的否决信号，它学会的将是"照否决
//! 信号改"的应付式捷径，而不是把打回当成属主的真实反馈去理解。
//!
//! 本模块把审查者的拒绝理由转写为属主口吻消息，并做**禁词检查**：结构化
//! 信号词（reject/verdict/否决/review 等）不得出现在最终消息里。若理由经
//! 中和后仍不干净，回退到"纯需求重述"模板（同样保证干净）；若需求文本
//! 本身含禁词（用户原话），回退到最小模板。

use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;

/// 结构化否决信号（P7 禁词检查）。最终消息不得含任一（小写匹配）。
pub const FORBIDDEN_SIGNALS: &[&str] = &[
    "reject",
    "rejected",
    "rejection",
    "rejects",
    "verdict",
    "reviewer",
    "review",
    "reviews",
    "reviewed",
    "scorer",
    "scored",
    "score",
    "grader",
    "graded",
    "eval",
    "evaluated",
    "evaluation",
    "unscored",
    "审查",
    "审查者",
    "评审",
    "评审者",
    "评分",
    "评估",
    "打分",
    "否决",
    "打回",
    "判定",
];

/// 中和替换表（长串优先；把结构化信号词替换为中性/属主口吻表达）。
const NEUTRAL_REPLACEMENTS: &[(&str, &str)] = &[
    ("fails to", "没能"),
    ("failed to", "没能"),
    ("does not match", "跟我要的对不上"),
    ("doesn't match", "跟我要的对不上"),
    ("does not align", "跟我要的对不上"),
    ("does not address", "没覆盖到"),
    ("was rejected", "不行"),
    ("is rejected", "不行"),
    ("rejection", "不行"),
    ("rejected", "不行"),
    ("reject", "不行"),
    ("verdict", "结论"),
    ("reviewed", ""),
    ("reviewer", ""),
    ("reviews", ""),
    ("review", ""),
    ("scored", ""),
    ("scorer", ""),
    ("score", ""),
    ("graded", ""),
    ("grader", ""),
    ("evaluated", ""),
    ("evaluation", ""),
    ("eval", ""),
    ("unscored", ""),
    ("审查者", ""),
    ("评审者", ""),
    ("评审", ""),
    ("评分", ""),
    ("评估", ""),
    ("打分", ""),
    ("审查", ""),
    ("否决", "不行"),
    ("打回", "要重做"),
    ("判定", "判断"),
];

/// 把审查者理由中和为不含结构化信号词的文本。
/// 会话文档第三段对规划器的投影字段名（方案B：review_summary → owner_feedback）。
pub const REVIEW_SUMMARY_PROJECTION_FIELD: &str = "owner_feedback";
/// review_summary 条目含禁词时的中性兜底模板（投影层与 maintain LLM 路径共用）。
pub const OWNER_FEEDBACK_NEUTRAL_TEMPLATE: &str = "属主对上一轮计划有反馈，请重新理解需求";

/// 净化 review_summary 条目：任一条目含禁词 → 回退中性模板（就地修改）。
pub fn sanitize_review_summary(entries: &mut Vec<String>) {
    for entry in entries.iter_mut() {
        if contains_forbidden_signal(entry).is_some() {
            *entry = OWNER_FEEDBACK_NEUTRAL_TEMPLATE.to_string();
        }
    }
}

pub fn neutralize_review_language(text: &str) -> String {
    let mut out = text.to_string();
    for (from, to) in NEUTRAL_REPLACEMENTS {
        out = out.replace(from, to);
    }
    out
}

/// 检查文本是否含任一禁词（小写匹配）。
pub fn contains_forbidden_signal(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    FORBIDDEN_SIGNALS
        .iter()
        .find(|w| lower.contains(*w))
        .copied()
}

/// 需求重述模板（作为伪装消息的兜底：只复述属主本意，天然无审查词汇）。
fn request_restatement(request: &OwnerRequest) -> String {
    format!(
        "我重新看了下需求，你给的方案跟我要的不太对。我要的是：{}——{}。验收标准：{}。你按这个重新弄一版，别自己发挥。",
        request.title, request.description, request.acceptance_criteria
    )
}

/// 最小兜底模板（需求文本本身含禁词时的最后防线）。
fn minimal_fallback() -> String {
    "我重新看了下需求，你给的方案跟我要的不太对，你再按我原来的需求重新弄一版。"
        .to_string()
}

/// 生成伪装打回消息（属主口吻）。
///
/// 策略：优先把审查者理由中和后嵌入属主口吻模板（保留具体反馈）；若中和后
/// 仍含禁词或为空，回退到需求重述模板；若需求文本含禁词（用户原话），回退
/// 到最小模板。最终消息保证不含禁词（自我强制，违反即 Err）。
pub fn disguise_rejection(
    request: &OwnerRequest,
    _plan: &DagSpec,
    reason: &str,
) -> Result<String, String> {
    let neutral = neutralize_review_language(reason);
    let candidate = if neutral.trim().is_empty() {
        request_restatement(request)
    } else {
        format!(
            "我重新看了下需求，你给的方案跟我要的不太对。{neutral}你再按我原来的需求重新弄一版，别想当然。"
        )
    };
    // 禁词检查：candidate 干净 → 用之。
    if contains_forbidden_signal(&candidate).is_none() {
        return Ok(candidate);
    }
    // 回退 1：需求重述模板（仍可能因需求原文含禁词而不干净）。
    let restated = request_restatement(request);
    if contains_forbidden_signal(&restated).is_none() {
        return Ok(restated);
    }
    // 回退 2：最小模板（保证干净）。
    Ok(minimal_fallback())
}
