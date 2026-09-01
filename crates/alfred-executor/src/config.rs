//! executor / reviewer 模型配置加载。
//!
//! 从 `~/.config/alfred/config.yml`（P11 唯一真源）解析
//! `roles.{executor,reviewer} → models → providers`，得出执行者/审查者模型。
//! P11 会以完整三层 config 模块替换本文件；本文件覆盖 R1/R2 需要的最小面
//! （env 覆盖：`LLM_<ROLE>_MODEL` 指定角色模型 id，`LLM_BASE_URL` /
//! `LLM_API_KEY` 覆盖 provider endpoint/key）。

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// max_tokens 缺省/下限（reasoning 模型思考耗 token：4096 实测被思考吃光致
/// 正文空 / 无 tool_calls —— R6c tier3b planner converse pi 零工具调用根因）。
/// 治本在 ModelEntry `maxTokens` serde 生效（alias 已接）；此下限对冲 config
/// 未显式配置 maxTokens 的场景。
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// 角色模型（provider 映射为 Inspect `openai-api/<provider>/<model>`）。
#[derive(Debug, Clone)]
pub struct ExecutorModel {
    /// provider 名（如 "zhipucoding"；mockllm 等内建模型为原始名）。
    pub provider: String,
    /// 模型 id（如 "glm-5.2" / "glm-4.7" / "mockllm/model"）。
    pub model: String,
    /// OpenAI 兼容 base_url（mockllm 等内建模型为空串）。
    pub base_url: String,
    /// provider api_key（只经 env 注入驱动进程，不进 argv、不进容器）。
    pub api_key: String,
    /// max_tokens（reasoning 模型思考耗 token：4096 实测会被思考吃光致正文空/
/// 无 tool_calls——默认下限 8192，见 R6c tier3b 根因与 R3 验方实测）。
    pub max_tokens: u32,
    /// 原始 inspect 模型 id（不经 openai-api/ 前缀包装），如 mockllm/model。
    pub raw_id: bool,
}

impl ExecutorModel {
    /// Inspect 通用 provider 模型 id；raw_id 时原样返回。
    pub fn inspect_model_id(&self) -> String {
        if self.raw_id {
            self.model.clone()
        } else {
            format!("openai-api/{}/{}", self.provider, self.model)
        }
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
    /// config.yml 用 `maxTokens`（camelCase）；serde 默认按字段名 `max_tokens`
    /// 解析会静默吞掉该键（None → 恒落 4096 下限）。alias 同时接受两种拼写。
    #[serde(default, alias = "maxTokens")]
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Roles {
    executor: String,
    #[serde(default)]
    reviewer: Option<String>,
    #[serde(default)]
    planner: Option<String>,
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

/// 加载执行者模型配置（roles.executor）。
pub fn load_executor_model() -> Result<ExecutorModel> {
    load_role_model("executor")
}

/// 加载审查者模型配置（roles.reviewer）。
pub fn load_reviewer_model() -> Result<ExecutorModel> {
    load_role_model("reviewer")
}

/// 加载规划器模型配置（roles.planner）。R6b 起规划器为容器内 pi 对话 agent（桥代发 LLM）。
pub fn load_planner_model() -> Result<ExecutorModel> {
    load_role_model("planner")
}

/// 角色模型 id 的 env 覆盖：`LLM_<ROLE>_MODEL`（P11 约定）优先，
/// `ALFRED_<ROLE>_MODEL` 为规范别名。
fn env_override_model_id(role: &str) -> Option<String> {
    let upper = role.to_ascii_uppercase();
    std::env::var(format!("LLM_{upper}_MODEL"))
        .ok()
        .or_else(|| std::env::var(format!("ALFRED_{upper}_MODEL")).ok())
}

/// inspect 内建模型（不经 openai-api provider）：raw id，无 provider 依赖。
/// 当前支持 mockllm（e2e 解析失败 unscored 用例用）。
fn raw_builtin_model(model_id: &str) -> Option<ExecutorModel> {
    if model_id.starts_with("mockllm/") {
        Some(ExecutorModel {
            provider: "mockllm".into(),
            model: model_id.to_string(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 1024,
            raw_id: true,
        })
    } else {
        None
    }
}

/// 加载指定角色的模型配置。
///
/// 规则：
/// - 模型 id 先取 env 覆盖（`LLM_<ROLE>_MODEL` / `ALFRED_<ROLE>_MODEL`），
///   否则取 config roles.<role>。
/// - mockllm 等 inspect 内建模型走 raw id 分支（无 base_url/key）。
/// - env 覆盖的模型 id 不在 models 列表时，沿用基础角色模型的 provider
///   （如 glm-5.2 → zhipucoding），max_tokens 取默认下限 8192——让 e2e
///   能以 `ALFRED_REVIEWER_MODEL=glm-4.7` 指定便宜模型，无需改 config。
fn load_role_model(role: &str) -> Result<ExecutorModel> {
    let cfg = load_config()?;
    // env 覆盖优先；无 env 覆盖时才要求 config roles.<role> 存在。
    let base_id = role_model_id(&cfg, role).ok();
    let model_id = match env_override_model_id(role) {
        Some(m) => m,
        None => match &base_id {
            Some(b) => b.clone(),
            None => bail!(
                "config roles.{role} not set and no ALFRED_{}/LLM_{}_MODEL override",
                role.to_ascii_uppercase(),
                role.to_ascii_uppercase()
            ),
        },
    };

    if let Some(raw) = raw_builtin_model(&model_id) {
        return Ok(raw);
    }

    let entry = cfg.models.iter().find(|m| m.id == model_id);
    let (provider_name, max_tokens) = match entry {
        Some(e) => (
            e.provider.clone(),
            e.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(DEFAULT_MAX_TOKENS),
        ),
        None => {
            // env 覆盖的模型不在列表：沿用基础角色的 provider；基础角色缺失
            // 时退到 executor 的 provider（e2e 以 ALFRED_PLANNER_MODEL 指定
            // 便宜模型、config 无 roles.planner 时的兜底）。
            let base = base_id
                .as_ref()
                .and_then(|id| cfg.models.iter().find(|m| m.id == *id))
                .or_else(|| cfg.models.iter().find(|m| m.id == cfg.roles.executor))
                .with_context(|| {
                    format!(
                        "model '{model_id}' not in models list and no base role model to inherit provider"
                    )
                })?;
            (base.provider.clone(), DEFAULT_MAX_TOKENS)
        }
    };
    let prov = cfg.providers.get(&provider_name).with_context(|| {
        format!("model '{model_id}' references unknown provider '{provider_name}'")
    })?;

    let base_url = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| prov.base_url.clone());
    let api_key = std::env::var("LLM_API_KEY").unwrap_or_else(|_| prov.api_key.clone());

    Ok(ExecutorModel {
        provider: provider_name,
        model: model_id,
        base_url,
        api_key,
        max_tokens,
        raw_id: false,
    })
}

fn load_config() -> Result<ConfigFile> {
    let path = config_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    let cfg: ConfigFile =
        serde_yaml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
    validate_config(&cfg)?;
    Ok(cfg)
}

/// 结构校验（§3.1 原文三种错误；第三种=模型引用未知 provider 在 load_role_model 报）：
/// ① roles 引用的模型 id 不在 models 列表（且非 env 覆盖）→ 加载期报错点名；
/// ② models 列表重复 id → 报错点名。
fn validate_config(cfg: &ConfigFile) -> Result<()> {
    // ② models 列表重复 id → 报错点名
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for m in &cfg.models {
        if seen.insert(m.id.as_str(), ()).is_some() {
            bail!("config models list has duplicate id '{}'", m.id);
        }
    }
    // ① roles 引用的模型 id 不在 models 列表（且非 env 覆盖）→ 报错点名
    let role_models: [(&str, Option<&String>); 3] = [
        ("executor", Some(&cfg.roles.executor)),
        ("reviewer", cfg.roles.reviewer.as_ref()),
        ("planner", cfg.roles.planner.as_ref()),
    ];
    for (role, model_id) in role_models {
        let Some(model_id) = model_id else { continue };
        if env_override_model_id(role).is_some() {
            continue; // env 覆盖生效，config roles.<role> 引用不参与解析
        }
        if !cfg.models.iter().any(|m| m.id == *model_id) {
            bail!(
                "config roles.{role} references model '{model_id}' not in models list"
            );
        }
    }
    Ok(())
}

fn role_model_id(cfg: &ConfigFile, role: &str) -> Result<String> {
    match role {
        "executor" => Ok(cfg.roles.executor.clone()),
        "reviewer" => cfg
            .roles
            .reviewer
            .clone()
            .context("config roles.reviewer not set"),
        "planner" => cfg
            .roles
            .planner
            .clone()
            .context("config roles.planner not set"),
        other => bail!("unknown role '{other}'"),
    }
}
