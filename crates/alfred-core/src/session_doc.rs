use serde::{Deserialize, Serialize};

// 会话文档：planner 对话 agent 与维护 agent 共享的会话状态载体。
// 固定三段，格式锁死——禁止加段、改名或换成自由文本。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDoc {
    // 关键文件路径
    pub key_file_paths: Vec<String>,
    // 关键结论
    pub key_conclusions: Vec<String>,
    // 审查结论摘要
    pub review_summary: String,
}
