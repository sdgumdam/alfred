//! owner↔planner 对话记录（对齐方案 v2 §二.8；reviewer 挂载输入的数据源）。
//!
//! 三容器改造中 reviewer 容器"比规划者多看到规划者和 owner 的对话记录"（属主原话
//! 08-25~28）——本实体是这条对话记录的**单一真源**：`{run_dir}/conversation.json`。
//! 格式见 §二.8：`run_id` + `turns[]`（seq/role/content/ts/source）。只挂给 reviewer
//! 容器（矩阵 §1.1 第 6 行）；planner/executor 挂载面上不存在此文件（非声明性 +
//! 隔离矩阵，验收 §四.4 断言）。
//!
//! 写入时机（§二.8）：每次 converse 调用落定后、每次属主消息（含打回伪装）落定后
//! 增量 append。数据源收敛自 `llm-calls/`、`panel-owner-message.txt`、
//! `panel-decision.json`、`state.json#session_doc`——统一经 [`append_to_disk`] 落盘。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util::now_rfc3339;

/// conversation.json 文件名（run_dir 下，reviewer 挂载输入）。
pub const CONVERSATION_FILE: &str = "conversation.json";

/// 对话角色（§二.8：owner | planner）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    Owner,
    Planner,
}

/// 轮次来源（§二.8：request.submit | owner.message | converse.reply | panel.decision）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSource {
    /// 初始需求提交（治理环起点读 request.json 时）。
    #[serde(rename = "request.submit")]
    RequestSubmit,
    /// 属主补充消息 / 计划打回伪装消息（revise / retry-伪装）。
    #[serde(rename = "owner.message")]
    OwnerMessage,
    /// converse 产出/答复（planning_step 落定后）。
    #[serde(rename = "converse.reply")]
    ConverseReply,
    /// 升级拍板时的属主决策（panel 决策）。
    #[serde(rename = "panel.decision")]
    PanelDecision,
}

/// 单轮对话（§二.8 turns[] 元素）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationTurn {
    /// 轮次序号（0 起；[`ConversationLog::append_turn`] 自动递增）。
    pub seq: u32,
    pub role: ConversationRole,
    /// 文本内容（原始文字；伪装消息落盘的就是伪装后文本，审查者据此判忠实度）。
    pub content: String,
    /// RFC3339 时间戳（append 自动取当前 UTC）。
    pub ts: String,
    pub source: ConversationSource,
}

/// owner↔planner 对话记录（§二.8 格式定义）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLog {
    pub run_id: String,
    #[serde(default)]
    pub turns: Vec<ConversationTurn>,
}

impl ConversationLog {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            turns: Vec::new(),
        }
    }

    /// 追加一轮：seq 自动 = 当前轮数，ts 自动 = 当前 UTC；返回该轮。
    pub fn append_turn(
        &mut self,
        role: ConversationRole,
        content: impl Into<String>,
        source: ConversationSource,
    ) -> ConversationTurn {
        let turn = ConversationTurn {
            seq: self.turns.len() as u32,
            role,
            content: content.into(),
            ts: now_rfc3339(),
            source,
        };
        self.turns.push(turn.clone());
        turn
    }
}

/// 读 `run_dir/conversation.json`；不存在 → `Ok(None)`（调用方用
/// [`ConversationLog::new`] 起步）。解析/读取错误带路径上下文返回。
pub fn load_conversation(run_dir: &Path) -> Result<Option<ConversationLog>, String> {
    let path = run_dir.join(CONVERSATION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// 落盘整个对话记录（`run_dir/conversation.json`，pretty JSON）。
pub fn save_conversation(run_dir: &Path, log: &ConversationLog) -> Result<(), String> {
    let path = run_dir.join(CONVERSATION_FILE);
    let text = serde_json::to_string_pretty(log)
        .map_err(|e| format!("serialize {}: {e}", path.display()))?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// 追加落盘（增量，统一入口）：读（缺省 new）→ append → 写。
///
/// 返回追加的轮次。文件不存在则创建（首轮通常是 `request.submit`）；
/// 已存在则读回再追加（seq 续接，不覆盖历史）。
pub fn append_to_disk(
    run_dir: &Path,
    run_id: &str,
    role: ConversationRole,
    content: impl Into<String>,
    source: ConversationSource,
) -> Result<ConversationTurn, String> {
    let mut log = load_conversation(run_dir)?.unwrap_or_else(|| ConversationLog::new(run_id));
    if log.run_id.is_empty() {
        log.run_id = run_id.to_string();
    }
    let turn = log.append_turn(role, content, source);
    save_conversation(run_dir, &log)?;
    Ok(turn)
}
