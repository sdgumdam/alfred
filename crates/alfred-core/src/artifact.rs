//! 执行产物（施工清单 §3.1 / 限界上下文 §3.1 Artifact）。
//!
//! 产物 = 工作目录文件变化（workspace diff / 文件比对）。执行驱动在执行
//! 前后对工作区卷做快照比对，差异即执行者的产出。

use serde::{Deserialize, Serialize};

/// 变化种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

/// 工作区文件快照条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// 相对工作区根目录的路径（`/` 分隔，不以 `./` 开头）。
    pub path: String,
    /// 文件字节数。
    pub size: u64,
    /// SHA-256 十六进制摘要。
    pub sha256: String,
}

/// 单个文件的变化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<FileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<FileEntry>,
}

/// 执行产物：执行前后工作区比对的差异 + 终态文件清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// 所属任务 id。
    pub task_id: String,
    /// 差异列表（created / modified / deleted）。
    pub changes: Vec<FileChange>,
    /// 终态文件清单（相对路径 → 条目）。
    pub files: Vec<FileEntry>,
}
