//! R1 最小 executor 配置加载。
//!
//! 从 `~/.config/alfred/config.yml`（P11 唯一真源）解析
//! `roles.executor → models → providers`，得出执行者模型。
//! P11 会以完整三层 config 模块替换本文件；本文件只覆盖 R1 需要的最小面。

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// 执行者模型（provider 映射为 Inspect `openai-api/<provider>/<model>`）。
#[derive(Debug, Clone)]
pub struct ExecutorModel {
    /// provider 名（如 "zhipucoding"）。
    pub provider: String,
    /// 模型 id（如 "glm-5.2"）。
    pub model: String,
    /// OpenAI 兼容 base_url。
    pub base_url: String,
    /// provider api_key（只经 `-M api_key=` 传参进 eval 进程，不进容器）。
    pub api_key: String,
    /// max_tokens（reasoning 模型下限 1024，见 R0报告 审计修正 1）。
    pub max_tokens: u32,
}

impl ExecutorModel {
    /// Inspect 通用 provider 模型 id。
    pub fn inspect_model_id(&self) -> String {
        format!("openai-api/{}/{}", self.provider, self.model)
    }
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    providers: HashMap<String, Provider>,
    models: Vec<ModelEntry>,
    roles: Roles,
}

#[derive(Debug, Deserialize)]
struct Provider {
    base_url: String,
    api_key: String,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    provider: String,
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Roles {
    executor: String,
}

/// config.yml 路径：`ALFRED_CONFIG` 覆盖，否则 `~/.config/alfred/config.yml`。
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("ALFRED_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(&home).join(".config"));
    base.join("alfred").join("config.yml")
}

/// 加载执行者模型配置。
pub fn load_executor_model() -> Result<ExecutorModel> {
    let path = config_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    let cfg: ConfigFile = serde_yaml::from_str(&text)
        .with_context(|| format!("parse config {}", path.display()))?;

    let model_id = cfg.roles.executor.clone();
    let entry = cfg
        .models
        .iter()
        .find(|m| m.id == model_id)
        .with_context(|| format!("roles.executor model '{model_id}' not found in models"))?;
    let prov = cfg
        .providers
        .get(&entry.provider)
        .with_context(|| {
            format!(
                "model '{model_id}' references unknown provider '{}'",
                entry.provider
            )
        })?;

    // reasoning 模型（glm-5.2）：max_tokens 必须 ≥1024，否则 thinking 吃光预算正文为空
    let max_tokens = entry.max_tokens.unwrap_or(1024).max(1024);

    Ok(ExecutorModel {
        provider: entry.provider.clone(),
        model: model_id,
        base_url: prov.base_url.clone(),
        api_key: prov.api_key.clone(),
        max_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_is_openai_api_prefixed() {
        let m = ExecutorModel {
            provider: "zhipucoding".into(),
            model: "glm-5.2".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
        };
        assert_eq!(m.inspect_model_id(), "openai-api/zhipucoding/glm-5.2");
    }
}
