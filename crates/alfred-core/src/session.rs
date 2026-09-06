//! 会话文档（施工清单 §2.4）：规划器的记忆载体。
//!
//! 规划器 = 会话文档 + 短会话。会话文档的维护者 agent 原定在两种时机更新
//! 会话文档——① 一次计划审查结论落定之后；② 属主补充新需求之后——**该维护者
//! 实现已回退，待新架构重做**；schema 三段结构保留（converse 输入契约不变，
//! 空文档无害）。会话文档固定三段结构：a) 计划要参考的关键文件路径；
//! b) 已经确立的关键结论（如技术选型结果）；c) 审查结论的摘要。
//!
//! 固定字段的结构化格式，不写自由散文——靠格式保证可查可纠，不靠叮嘱
//! "要仔细"。属主对会话文档只读，不直接修改。
//!
//! 隔离（§2.2/P7）：`review_summary` 必须是**中性转写**（属主口吻/去结构化
//! 信号），不能出现"你的计划被否决了"这类标准化否决标签（重做时保持）。

use serde::{Deserialize, Serialize};

/// 规划器会话文档（固定三段）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDoc {
    /// 计划要参考的关键文件路径。
    #[serde(default)]
    pub key_file_paths: Vec<String>,
    /// 已经确立的关键结论（如技术选型结果、属主补充的新需求）。
    #[serde(default)]
    pub key_conclusions: Vec<String>,
    /// 审查结论的摘要（中性转写，属主口吻，无结构化否决信号）。
    #[serde(default)]
    pub review_summary: Vec<String>,
}

impl SessionDoc {
    pub fn new() -> Self {
        Self::default()
    }
}
