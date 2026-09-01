//! 属主请求（施工清单 §2.1 属主职责 / §3.2 环节 1）。

use serde::{Deserialize, Serialize};

use crate::util::now_rfc3339;

/// 属主（人）提交的原始需求：需求文字 + 验收标准。
///
/// 治理环起点读取（request.json）；规划器从本实体拆解出任务节点与契约。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OwnerRequest {
    /// 需求唯一 id（如 `req-<ts>`）。
    pub id: String,
    /// 需求标题（一句话）。
    pub title: String,
    /// 需求正文（自然语言描述）。
    pub description: String,
    /// 验收标准（给审查者的判分依据）。
    pub acceptance_criteria: String,
    /// RFC3339 提交时间。
    pub created_at: String,
}

impl OwnerRequest {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
        acceptance_criteria: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            acceptance_criteria: acceptance_criteria.into(),
            created_at: now_rfc3339(),
        }
    }
}
