//! 规划器 LLM 直调（施工清单 §3.1：规划器是宿主进程里的 Rust 代码，直接读
//! 这份配置，用里面的接口地址和密钥发 HTTP 请求）。
//!
//! - 每次调用落盘 `llm-calls/` 作验收证据（P9 / §五 S0"每次调用落盘
//!   llm-calls/ 作验收证据"）。
//! - 离线用 `ALFRED_OFFLINE=1` 切回确定性直通（e2e 可控；仍落盘 llm-calls/
//!   记录 would-be 请求 + 离线响应，供"从 llm 调用记录断言"）。
//!
//! 模型接入配置复用 `~/.config/alfred/config.yml` 的 roles.planner →
//! models → providers（唯一真源，见 alfred-executor::config）。

use std::path::{Path, PathBuf};

use alfred_executor::config::ExecutorModel;
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
    pub messages: Vec<ChatMessage>,
    pub response: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// 规划器 LLM 客户端（宿主 Rust 直调 OpenAI 兼容 /chat/completions）。
#[derive(Debug, Clone)]
pub struct LlmClient {
    pub model: ExecutorModel,
}

impl LlmClient {
    pub fn new(model: ExecutorModel) -> Self {
        Self { model }
    }

    /// 是否离线模式（`ALFRED_OFFLINE=1`）。
    pub fn offline(&self) -> bool {
        std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1")
    }

    /// 调用一次对话补全，返回响应文本。
    pub fn chat(&self, messages: &[ChatMessage]) -> Result<String> {
        self.chat_with_max_tokens(messages, self.model.max_tokens)
    }

    /// 调用一次对话补全，指定 max_tokens（规划器要吐大段建图指令序列，
    /// 默认 1024 会截断——见 converse）。
    pub fn chat_with_max_tokens(
        &self,
        messages: &[ChatMessage],
        max_tokens: u32,
    ) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.model.base_url.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "model": self.model.model,
            "messages": messages,
            "max_tokens": max_tokens.max(self.model.max_tokens),
            "temperature": 0,
        });
        let resp = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.model.api_key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(300))
            .send_string(&body.to_string())
            .map_err(|e| anyhow::anyhow!("LLM request to {url} failed: {e}"))?;
        let text = resp
            .into_string()
            .context("LLM response body read failed")?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("LLM response not JSON: {text}"))?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(String::from)
            .with_context(|| format!("LLM response missing choices[0].message.content: {text}"))
    }
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
            messages: vec![ChatMessage::user("hi")],
            response: "hello".into(),
            ok: true,
            error: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: LlmCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.role, "planner");
        assert!(!back.offline);
    }

    #[test]
    fn offline_flag_detected() {
        let _guard = crate::llm::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let m = ExecutorModel {
            provider: "z".into(),
            model: "m".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let client = LlmClient::new(m);
        std::env::remove_var("ALFRED_OFFLINE");
        assert!(!client.offline());
        std::env::set_var("ALFRED_OFFLINE", "1");
        assert!(client.offline());
        std::env::remove_var("ALFRED_OFFLINE");
    }
}
