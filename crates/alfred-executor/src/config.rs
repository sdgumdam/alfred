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

/// 角色模型（provider 映射为 Inspect `openai-api/<provider>/<model>`）。
#[derive(Debug, Clone)]
pub struct ExecutorModel {
    /// provider 名（如 "zhipucoding"；mockllm 等内建模型为原始名）。
    pub provider: String,
    /// 模型 id（如 "glm-5.2" / "glm-4.7" / "mockllm/model"）。
    pub model: String,
    /// OpenAI 兼容 base_url（mockllm 等内建模型为空串）。
    pub base_url: String,
    /// provider api_key（只经 env 注入 eval 进程，不进 argv、不进容器）。
    pub api_key: String,
    /// max_tokens（reasoning 模型思考耗 token：1024 实测会被思考吃光致正文空——
/// 默认 4096，见 R3 验方实测与 R0 报告）。
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
    #[serde(default)]
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

/// 加载规划器模型配置（roles.planner）。R3 起规划器由宿主 Rust 直调 LLM。
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
///   （如 glm-5.2 → zhipucoding），max_tokens 取默认下限 1024——让 e2e
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
        Some(e) => (e.provider.clone(), e.max_tokens.unwrap_or(4096).max(4096)),
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
            (base.provider.clone(), 4096)
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
    serde_yaml::from_str(&text).with_context(|| format!("parse config {}", path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // env 变量（ALFRED_CONFIG / *_MODEL 覆盖）是进程级全局；并行测试会互相
    // 踩踏。三个碰 env 的测试用同一把锁串行化。
    static CONFIG_TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        CONFIG_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sample_config() -> String {
        r#"
providers:
  zhipucoding:
    base_url: "https://open.bigmodel.cn/api/coding/paas/v4"
    api_key: "sk-test"
models:
  - id: glm-5.2
    provider: zhipucoding
    max_tokens: 16384
  - id: glm-4.7
    provider: zhipucoding
roles:
  executor: glm-5.2
  reviewer: glm-5.2
  planner: glm-4.7
"#
        .to_string()
    }

    fn write_config(dir: &std::path::Path, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("config.yml");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn model_id_is_openai_api_prefixed() {
        let m = ExecutorModel {
            provider: "zhipucoding".into(),
            model: "glm-5.2".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        assert_eq!(m.inspect_model_id(), "openai-api/zhipucoding/glm-5.2");
    }

    #[test]
    fn raw_id_returns_model_unchanged() {
        let m = ExecutorModel {
            provider: "mockllm".into(),
            model: "mockllm/model".into(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 1024,
            raw_id: true,
        };
        assert_eq!(m.inspect_model_id(), "mockllm/model");
    }

    #[test]
    fn loads_reviewer_and_executor_from_config() {
        let _guard = env_guard();
        // 清除进程级覆盖 env（e2e 可能设置 ALFRED_REVIEWER_MODEL 等）
        for k in ["LLM_REVIEWER_MODEL", "ALFRED_REVIEWER_MODEL", "LLM_EXECUTOR_MODEL", "ALFRED_EXECUTOR_MODEL"] {
            std::env::remove_var(k);
        }
        let dir = std::env::temp_dir().join("alfred-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_config(&dir, &sample_config());
        std::env::set_var("ALFRED_CONFIG", &p);

        let exec = load_executor_model().unwrap();
        assert_eq!(exec.model, "glm-5.2");
        assert_eq!(exec.provider, "zhipucoding");
        // max_tokens 下限 1024，但 config 显式 16384 生效
        assert_eq!(exec.max_tokens, 16384);
        assert!(!exec.raw_id);

        let rev = load_reviewer_model().unwrap();
        assert_eq!(rev.model, "glm-5.2");
        assert_eq!(rev.provider, "zhipucoding");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_override_model_not_in_list_falls_back_to_base_provider() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join("alfred-config-test-env");
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_config(&dir, &sample_config());
        std::env::set_var("ALFRED_CONFIG", &p);
        std::env::set_var("ALFRED_REVIEWER_MODEL", "glm-4.7");

        let rev = load_reviewer_model().unwrap();
        // glm-4.7 在 models 列表里（sample_config 有），正常解析
        assert_eq!(rev.model, "glm-4.7");
        assert_eq!(rev.provider, "zhipucoding");
        assert_eq!(rev.max_tokens, 4096); // 列表项未配 max_tokens → 默认下限（reasoning 模型思考耗 token，4096 实测安全）

        // 不在列表的模型 → 沿用基础角色 provider
        std::env::set_var("ALFRED_REVIEWER_MODEL", "glm-999");
        let rev2 = load_reviewer_model().unwrap();
        assert_eq!(rev2.model, "glm-999");
        assert_eq!(rev2.provider, "zhipucoding");

        std::env::remove_var("ALFRED_REVIEWER_MODEL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mockllm_is_raw_builtin() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join("alfred-config-test-mock");
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_config(&dir, &sample_config());
        std::env::set_var("ALFRED_CONFIG", &p);
        // 用 LLM_ 前缀覆盖名，避免与 ALFRED_ 覆盖名测试并行竞争
        std::env::set_var("LLM_REVIEWER_MODEL", "mockllm/model");

        let rev = load_reviewer_model().unwrap();
        assert!(rev.raw_id);
        assert_eq!(rev.inspect_model_id(), "mockllm/model");
        assert_eq!(rev.base_url, "");

        std::env::remove_var("LLM_REVIEWER_MODEL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_planner_model_from_config() {
        let _guard = env_guard();
        for k in ["LLM_PLANNER_MODEL", "ALFRED_PLANNER_MODEL"] {
            std::env::remove_var(k);
        }
        let dir = std::env::temp_dir().join("alfred-config-test-planner");
        let _ = std::fs::remove_dir_all(&dir);
        let p = write_config(&dir, &sample_config());
        std::env::set_var("ALFRED_CONFIG", &p);

        let plan = load_planner_model().unwrap();
        assert_eq!(plan.model, "glm-4.7");
        assert_eq!(plan.provider, "zhipucoding");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planner_env_override_without_roles_planner_falls_back_to_executor_provider() {
        let _guard = env_guard();
        let dir = std::env::temp_dir().join("alfred-config-test-planner-env");
        let _ = std::fs::remove_dir_all(&dir);
        // 无 roles.planner 的 config（只有 executor/reviewer）
        let cfg = r#"
providers:
  zhipucoding:
    base_url: "https://open.bigmodel.cn/api/coding/paas/v4"
    api_key: "sk-test"
models:
  - id: glm-5.2
    provider: zhipucoding
  - id: glm-4.7
    provider: zhipucoding
roles:
  executor: glm-5.2
  reviewer: glm-5.2
"#;
        let p = write_config(&dir, cfg);
        std::env::set_var("ALFRED_CONFIG", &p);
        std::env::set_var("ALFRED_PLANNER_MODEL", "glm-4.7");

        let plan = load_planner_model().unwrap();
        assert_eq!(plan.model, "glm-4.7");
        assert_eq!(plan.provider, "zhipucoding"); // 继承 executor 的 provider

        std::env::remove_var("ALFRED_PLANNER_MODEL");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
