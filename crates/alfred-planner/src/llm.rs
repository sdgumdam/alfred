//! 规划器 LLM 调用记录（R6b：ureq 手搓直调退役）。
//!
//! R6b 起规划器的 LLM 调用在**容器内**执行（planner 容器驱动，`container.rs`），
//! 经 `sandbox_agent_bridge` 桥代发到宿主侧 Inspect 模型（与 executor 同机制）。
//! 宿主不再手搓 HTTP 直调——`LlmClient` / `ureq` 已删除。
//!
//! 本模块只保留调用记录的**落盘**：每次 converse/maintain 调用（桥代发或离线
//! 确定性直通）由调用方构造 [`LlmCallRecord`] 并经 [`log_llm_call`] 写到
//! `run_dir/llm-calls/<seq>.json`（P9 / §五 S0 验收证据）。宿主侧桥服务的调用
//! 日志（evals/ 下的 inspect 记录）即实际 LLM HTTP 调用的审计源；llm-calls/
//! 记录承载语义轮次（messages + response），供 e2e 从记录断言会话文档/伪装消息。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 聊天消息（OpenAI 兼容 roles）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

/// LLM 调用记录（落盘 llm-calls/；e2e 从记录断言会话文档/伪装消息）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmCallRecord {
    pub ts: String,
    pub role: String,
    pub model: String,
    pub offline: bool,
    /// 调用通道：`"container_bridge"`（容器内 pi 经桥代发）| `"offline"`（离线确定性直通）。
    /// 旧记录无此字段 → 反序列化缺省空串（向后兼容）。
    #[serde(default)]
    pub transport: String,
    pub messages: Vec<ChatMessage>,
    pub response: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// 追加一条 LLM 调用记录到 `run_dir/llm-calls/<seq>.json`。
pub fn log_llm_call(run_dir: &Path, record: &LlmCallRecord) -> Result<PathBuf> {
    let dir = run_dir.join("llm-calls");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let seq = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    let path = dir.join(format!("{:04}.json", seq));
    let text = serde_json::to_string_pretty(record).context("serialize llm call record")?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}


/// env 变量（ALFRED_OFFLINE 等）是进程级全局；并行测试会互相踩踏。
/// 碰 env 的测试（converse_offline_* 等）用这把锁串行化。
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_message_roles() {
        let s = ChatMessage::system("sys");
        assert_eq!(s.role, "system");
        let u = ChatMessage::user("usr");
        assert_eq!(u.role, "user");
    }

    #[test]
    fn llm_call_record_round_trip() {
        let rec = LlmCallRecord {
            ts: "2026-08-26T00:00:00Z".into(),
            role: "planner".into(),
            model: "openai-api/zhipucoding/glm-5.3".into(),
            offline: false,
            transport: "container_bridge".into(),
            messages: vec![ChatMessage::user("hi")],
            response: "hello".into(),
            ok: true,
            error: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: LlmCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, "planner");
        assert!(!back.offline);
        assert_eq!(back.transport, "container_bridge");
    }

    #[test]
    fn llm_call_record_defaults_transport_for_legacy_records() {
        // 旧记录（无 transport 字段）→ 反序列化缺省空串（向后兼容）
        let json = r#"{"ts":"2026-08-26T00:00:00Z","role":"converse","model":"m","offline":false,"messages":[],"response":"r","ok":true,"error":null}"#;
        let rec: LlmCallRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.transport, "");
        assert!(!rec.offline);
    }
}
