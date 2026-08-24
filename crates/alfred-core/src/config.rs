use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

// 系统路径配置文件（XDG Base Directory 规范）：
//   ${XDG_CONFIG_HOME:-~/.config}/alfred/config.yml
// 三块平级：providers（endpoint+凭证 map）/ models（数组，id 即请求体模型名）/
// roles（角色 → 模型 id 绑定）。模型经 provider 名引用 endpoint；
// 异构审查比较的是显式 provider 名（自部署网关转发时 id 前缀推断会猜错）。
// 优先级：环境变量 > 配置文件（LLM_BASE_URL/LLM_API_KEY 覆盖所有 provider
// 的 base_url/api_key——单 provider 调试场景；
// LLM_PLANNER_MODEL/LLM_EXECUTOR_MODEL/LLM_REVIEWER_MODEL 覆盖 roles 指向；
// contextWindow / maxTokens / thinking 只从文件读）。
// 文件不存在且 env 也没有才报 MissingEnv。
// api_key 是敏感信息：Unix 上文件权限宽于 0600 时警告到 stderr（不阻断读取）。

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub providers: HashMap<String, ProviderDef>,
    pub models: Vec<ModelDef>,
    pub roles: RoleBindings,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDef {
    pub base_url: String,
    pub api_key: String,
    pub protocol: Protocol,
}
// 请求协议：决定请求体拼装 / 认证头 / 响应解析路径。
// 未知值（如 "openai"、"azure"）加载期报错，不静默 fallback。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenaiCompatible,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    // 模型唯一标识：API 请求体的 model 字段原样发它，同时是 roles 引用的键。
    pub id: String,
    // 引用 providers map 的 key：endpoint 与凭证随 provider 走。
    pub provider: String,
    // alfred 侧元数据，不发给 API；消费者是 S1 的 prompt 预算，当前仅声明。
    #[serde(rename = "contextWindow")]
    pub context_window: Option<u64>,
    // 拼进请求体 max_tokens；缺省不带该字段，由服务端默认值决定。
    #[serde(rename = "maxTokens")]
    pub max_tokens: Option<u64>,
    // off 或缺省 → 请求体不带 reasoning_effort；low/medium/high → 原样带上。
    pub thinking: Option<Thinking>,
}

// 角色绑定：值是 models 里的 id，加载时校验指向必须存在。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBindings {
    pub planner: String,
    pub executor: String,
    pub reviewer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    Off,
    Low,
    Medium,
    High,
}

impl Thinking {
    // off 不产出 reasoning_effort；其余档位映射为同名 API 字段值。
    pub fn reasoning_effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    // 读文件本身的 IO 错误（权限拒绝等）。文件不存在不算错，是 Ok(None)。
    Io(std::io::Error),
    // 文件存在但内容不是合法 YAML 或不符合 schema（deny_unknown_fields 锁死）。
    Parse(serde_yaml::Error),
    // 语义校验失败：id 重复 / 模型 provider 不存在 / 角色引用了不存在的模型 id。
    Validation(String),
    // HOME 未设置且没有 XDG_CONFIG_HOME，连默认路径都推导不出来。
    NoHomeDir,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "cannot read config file: {err}"),
            Self::Parse(err) => write!(f, "config file is not valid llm config YAML: {err}"),
            Self::Validation(message) => write!(f, "config file violates llm config semantics: {message}"),
            Self::NoHomeDir => {
                write!(f, "cannot resolve config path: neither XDG_CONFIG_HOME nor HOME is set")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Parse(err) => Some(err),
            Self::Validation(_) => None,
            Self::NoHomeDir => None,
        }
    }
}

impl From<serde_yaml::Error> for ConfigError {
    fn from(err: serde_yaml::Error) -> Self {
        Self::Parse(err)
    }
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::NoHomeDir)?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("alfred").join("config.yml"))
}

pub fn load_llm_config() -> Result<Option<AppConfig>, ConfigError> {
    load_llm_config_from(&config_path()?)
}

fn load_llm_config_from(path: &std::path::Path) -> Result<Option<AppConfig>, ConfigError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::Io(err)),
    };
    // 两类警告都只提示不阻断：配置文件是用户的，alfred 无权拒绝，只负责提醒。
    if let Some(warning) = permission_warning(path) {
        eprintln!("{warning}");
    }
    let parsed: AppConfig = serde_yaml::from_str(&content)?;
    validate_config(&parsed).map_err(ConfigError::Validation)?;
    // 引用均已校验，find/索引不会缺键。
    let provider_of = |id: &str| -> &str {
        let def = parsed.models.iter().find(|def| def.id == id).expect("role ids validated");
        def.provider.as_str()
    };
    if let Some(warning) =
        heterogeneity_warning(provider_of(&parsed.roles.executor), provider_of(&parsed.roles.reviewer))
    {
        eprintln!("{warning}");
    }
    Ok(Some(parsed))
}

// 加载期校验：模型 id 无重复、模型 provider 必须存在于 providers map、
// 角色引用的 id 全部存在。
fn validate_config(config: &AppConfig) -> Result<(), String> {
    for (index, def) in config.models.iter().enumerate() {
        if config.models[..index].iter().any(|earlier| earlier.id == def.id) {
            return Err(format!(
                "duplicate model id '{}' in models; ids must be unique",
                def.id
            ));
        }
        if !config.providers.contains_key(&def.provider) {
            let defined: Vec<&str> = config.providers.keys().map(String::as_str).collect();
            return Err(format!(
                "model '{}' references provider '{}', \
                 which is not defined under providers (defined providers: {})",
                def.id,
                def.provider,
                defined.join(", ")
            ));
        }
    }
    for (role, id) in [
        ("planner", config.roles.planner.as_str()),
        ("executor", config.roles.executor.as_str()),
        ("reviewer", config.roles.reviewer.as_str()),
    ] {
        if !config.models.iter().any(|def| def.id == id) {
            let defined: Vec<&str> = config.models.iter().map(|def| def.id.as_str()).collect();
            return Err(format!(
                "role '{role}' references model id '{id}', \
                 which is not defined under models (defined ids: {})",
                defined.join(", ")
            ));
        }
    }
    Ok(())
}

// 敏感文件权限检查：宽于 0600 给出警告文本；0600 或更严返回 None。
#[cfg(unix)]
fn permission_warning(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = path.metadata().ok()?;
    let mode = metadata.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then(|| {
        format!(
            "warning: config file {} has permissions {mode:04o}, which is wider than 0600; \
             it contains an api_key, consider: chmod 600 {}",
            path.display(),
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn permission_warning(_path: &std::path::Path) -> Option<String> {
    None
}

// 异构审查校验：reviewer 与 executor 必须不同 provider（显式 provider 名）；
// 退化成同 provider 审查时给出警告文本，不阻断。
pub fn heterogeneity_warning(executor_provider: &str, reviewer_provider: &str) -> Option<String> {
    (executor_provider == reviewer_provider).then(|| {
        format!(
            "warning: reviewer and executor both use provider {reviewer_provider:?}; \
             review should use a different model provider than execution, \
             same-provider review is degraded"
        )
    })
}

#[cfg(test)]
static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    // 测试全部走 set_var/remove_var 改进程环境，cargo test 默认多线程跑会互相踩，
    // 用一个全局互斥锁串行化。每个测试结束把环境恢复原状。
    use super::*;
    use std::sync::MutexGuard;

    pub(super) const LLM_ENV_NAMES: [&str; 6] = [
        "LLM_BASE_URL",
        "LLM_API_KEY",
        "LLM_PLANNER_MODEL",
        "LLM_EXECUTOR_MODEL",
        "LLM_REVIEWER_MODEL",
        // 已删除的 legacy 变量；守卫照样保存/恢复，防止它从外部环境渗进断言。
        "LLM_MODEL",
    ];

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = super::TEST_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = ["XDG_CONFIG_HOME", "HOME", "LLM_BASE_URL", "LLM_API_KEY",
                "LLM_PLANNER_MODEL", "LLM_EXECUTOR_MODEL", "LLM_REVIEWER_MODEL", "LLM_MODEL"]
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
            Self { _lock: lock, saved }
        }

        fn set(&self, name: &str, value: &std::path::Path) {
            std::env::set_var(name, value);
        }

        fn unset_all(&self, names: &[&str]) {
            for name in names {
                std::env::remove_var(name);
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    pub(super) fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let alfred_dir = dir.join("alfred");
        fs::create_dir_all(&alfred_dir).expect("create alfred config dir");
        let path = alfred_dir.join("config.yml");
        fs::write(&path, body).expect("write config.yml");
        path
    }

    pub(super) const FULL_CONFIG: &str = r#"
providers:
  kuaizi:
    base_url: "https://kuaizi.example.com/v1"
    api_key: "kuaizi-key"
    protocol: openai-compatible
  volc:
    base_url: "https://volc.example.com/v1"
    api_key: "volc-key"
    protocol: anthropic

models:
  - id: qwen3.8-max
    provider: kuaizi
    contextWindow: 131072
    maxTokens: 8192
    thinking: high
  - id: doubao-seed-evolving
    provider: kuaizi
    contextWindow: 262144
  - id: glm-5.2
    provider: volc
    thinking: medium

roles:
  planner: qwen3.8-max
  executor: doubao-seed-evolving
  reviewer: glm-5.2
"#;

    #[test]
    fn config_path_prefers_xdg_config_home_over_home_default() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let xdg = temp.path().join("xdg");
        let home = temp.path().join("home");
        guard.set("XDG_CONFIG_HOME", &xdg);
        guard.set("HOME", &home);
        assert_eq!(config_path().expect("path"), xdg.join("alfred/config.yml"));

        std::env::remove_var("XDG_CONFIG_HOME");
        assert_eq!(config_path().expect("path"), home.join(".config/alfred/config.yml"));
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        guard.set("XDG_CONFIG_HOME", temp.path());
        assert_eq!(load_llm_config().expect("load"), None);
    }

    #[test]
    fn load_reads_providers_models_and_roles_from_file() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = write_config(temp.path(), FULL_CONFIG);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 600");
        }
        guard.set("XDG_CONFIG_HOME", temp.path());
        let config = load_llm_config().expect("load").expect("config present");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(
            config.providers["kuaizi"],
            ProviderDef {
                base_url: "https://kuaizi.example.com/v1".into(),
                api_key: "kuaizi-key".into(),
                protocol: Protocol::OpenaiCompatible,
            }
        );
        assert_eq!(
            config.providers["volc"],
            ProviderDef {
                base_url: "https://volc.example.com/v1".into(),
                api_key: "volc-key".into(),
                protocol: Protocol::Anthropic,
            }
        );
        assert_eq!(config.roles.planner, "qwen3.8-max");
        assert_eq!(config.roles.executor, "doubao-seed-evolving");
        assert_eq!(config.roles.reviewer, "glm-5.2");
        assert_eq!(config.models.len(), 3);
        assert_eq!(
            config.models[0],
            ModelDef {
                id: "qwen3.8-max".into(),
                provider: "kuaizi".into(),
                context_window: Some(131_072),
                max_tokens: Some(8192),
                thinking: Some(Thinking::High),
            }
        );
        assert_eq!(
            config.models[1],
            ModelDef {
                id: "doubao-seed-evolving".into(),
                provider: "kuaizi".into(),
                context_window: Some(262_144),
                max_tokens: None,
                thinking: None,
            }
        );
        assert_eq!(
            config.models[2],
            ModelDef {
                id: "glm-5.2".into(),
                provider: "volc".into(),
                context_window: None,
                max_tokens: None,
                thinking: Some(Thinking::Medium),
            }
        );
    }

    #[test]
    fn load_rejects_duplicate_model_id() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG.replace("  - id: doubao-seed-evolving", "  - id: qwen3.8-max").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("duplicate id must fail");
        let rendered = err.to_string();
        assert!(matches!(err, ConfigError::Validation(_)), "expected validation error, got {rendered}");
        assert!(rendered.contains("duplicate"), "error should say duplicate: {rendered}");
        assert!(rendered.contains("qwen3.8-max"), "error should name the id: {rendered}");
    }

    #[test]
    fn load_rejects_model_with_unknown_provider() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG.replace("    provider: volc", "    provider: ghost-provider").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("unknown provider must fail");
        let rendered = err.to_string();
        assert!(matches!(err, ConfigError::Validation(_)), "expected validation error, got {rendered}");
        assert!(rendered.contains("glm-5.2"), "error should name the model: {rendered}");
        assert!(rendered.contains("ghost-provider"), "error should name the provider: {rendered}");
    }

    #[test]
    fn load_rejects_role_id_not_defined_in_models() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG.replace("  reviewer: glm-5.2", "  reviewer: kimi-k3").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("dangling role id must fail");
        let rendered = err.to_string();
        assert!(matches!(err, ConfigError::Validation(_)), "expected validation error, got {rendered}");
        assert!(rendered.contains("reviewer"), "error should name the role: {rendered}");
        assert!(rendered.contains("kimi-k3"), "error should name the missing id: {rendered}");
    }

    #[test]
    fn load_rejects_unknown_field_via_deny_unknown_fields() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        // 模型定义里的未知字段（temperature 不在 schema 里）。
        write_config(
            temp.path(),
            FULL_CONFIG.replace("    thinking: high", "    thinking: high\n    temperature: 0.7").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("unknown model field must fail");
        assert!(matches!(err, ConfigError::Parse(_)), "expected parse error, got {err}");
    }

    #[test]
    fn load_rejects_unknown_protocol_value() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG.replace("    protocol: anthropic", "    protocol: azure").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("unknown protocol must fail");
        let rendered = err.to_string();
        assert!(matches!(err, ConfigError::Parse(_)), "expected parse error, got {rendered}");
        assert!(rendered.contains("azure"), "error should name the value: {rendered}");
    }

    #[test]
    fn load_rejects_unknown_thinking_literal() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG.replace("    thinking: high", "    thinking: galaxy").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("unknown thinking literal must fail");
        assert!(matches!(err, ConfigError::Parse(_)), "expected parse error, got {err}");
    }

    #[test]
    fn load_rejects_malformed_yaml() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(temp.path(), "providers:\n  kuaizi:\n    base_url: [\n");
        guard.set("XDG_CONFIG_HOME", temp.path());
        let err = load_llm_config().expect_err("garbage yaml must fail");
        assert!(matches!(err, ConfigError::Parse(_)), "expected parse error, got {err}");
    }

    // 权限 0644：生成 stderr 警告文本，读取仍成功（不阻断）。
    #[cfg(unix)]
    #[test]
    fn permissive_0644_file_warns_but_still_loads() {
        use std::os::unix::fs::PermissionsExt;
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = write_config(temp.path(), FULL_CONFIG);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod 644");
        guard.set("XDG_CONFIG_HOME", temp.path());

        let warning = permission_warning(&path);
        assert!(warning.is_some(), "0644 must produce a stderr warning");
        assert!(warning.expect("warning").contains("0644"));

        let config =
            load_llm_config().expect("permissive file must still load").expect("config present");
        assert_eq!(config.providers["kuaizi"].api_key, "kuaizi-key");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 600");
        assert!(permission_warning(&path).is_none(), "0600 must stay silent");
    }

    // 异构审查校验：同 provider 给出警告文本，异 provider 静默。
    #[test]
    fn same_provider_produces_warning() {
        let warning = heterogeneity_warning("kuaizi", "kuaizi");
        assert!(warning.is_some(), "same provider must warn about degraded review");
        assert!(warning.expect("warning").contains("kuaizi"));
        assert!(
            heterogeneity_warning("kuaizi", "volc").is_none(),
            "heterogeneous providers must stay silent"
        );
    }

    // reviewer 与 executor 同 provider 的配置文件仍要加载成功（警告不阻断）。
    #[test]
    fn same_provider_config_file_still_loads() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG.replace("  reviewer: glm-5.2", "  reviewer: qwen3.8-max").as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let config = load_llm_config().expect("same-provider config must load").expect("config present");
        let provider_of = |id: &str| {
            config.models.iter().find(|def| def.id == id).map(|def| def.provider.as_str())
        };
        assert_eq!(provider_of(&config.roles.reviewer), provider_of(&config.roles.executor));
    }

    // 不同角色可以引用同一个模型 id。
    #[test]
    fn different_roles_can_share_the_same_model_id() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(
            temp.path(),
            FULL_CONFIG
                .replace("  planner: qwen3.8-max", "  planner: doubao-seed-evolving")
                .as_str(),
        );
        guard.set("XDG_CONFIG_HOME", temp.path());
        let config = load_llm_config().expect("shared id must load").expect("config present");
        assert_eq!(config.roles.planner, config.roles.executor);
    }

    #[test]
    fn load_errors_when_no_xdg_and_no_home() {
        let guard = EnvGuard::new();
        guard.unset_all(&["XDG_CONFIG_HOME", "HOME"]);
        let err = load_llm_config().expect_err("no home must fail");
        assert!(matches!(err, ConfigError::NoHomeDir), "expected NoHomeDir, got {err}");
    }
}

#[cfg(test)]
mod precedence_tests {
    // 验证 LlmConfig::from_env_or_file 的优先级：env > 配置文件。
    // LLM_BASE_URL / LLM_API_KEY 覆盖所有 provider 的 endpoint/凭证；
    // LLM_*_MODEL 覆盖 roles 的 id 指向；ModelDef 字段只从文件读。
    // 同样用互斥锁串行化进程环境改动。
    use super::tests::{write_config, FULL_CONFIG, LLM_ENV_NAMES};
    use crate::llm::{LlmConfig, LlmRole};
    use std::sync::MutexGuard;

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = super::TEST_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = ["XDG_CONFIG_HOME", "HOME", "LLM_BASE_URL", "LLM_API_KEY",
                "LLM_PLANNER_MODEL", "LLM_EXECUTOR_MODEL", "LLM_REVIEWER_MODEL", "LLM_MODEL"]
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
            Self { _lock: lock, saved }
        }

        fn use_xdg(&self, dir: &std::path::Path) {
            std::env::set_var("XDG_CONFIG_HOME", dir);
        }

        fn clear_llm_env(&self) {
            for name in LLM_ENV_NAMES {
                std::env::remove_var(name);
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn env_wins_for_all_providers_and_role_id_pointers() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(temp.path(), FULL_CONFIG);
        guard.use_xdg(temp.path());
        guard.clear_llm_env();
        std::env::set_var("LLM_BASE_URL", "https://env.example.com/v1");
        std::env::set_var("LLM_API_KEY", "env-key");
        // env 把 planner 的 id 指向改到 glm-5.2：
        // 生效的是指向变化，planner 请求体应拿到 glm-5.2 的 ModelDef 与 volc 的 endpoint。
        std::env::set_var("LLM_PLANNER_MODEL", "glm-5.2");

        let config = LlmConfig::from_env_or_file().expect("env satisfies config");
        // env 覆盖所有 provider 的 base_url/api_key（单 provider 调试语义）。
        assert_eq!(config.providers["kuaizi"].base_url, "https://env.example.com/v1");
        assert_eq!(config.providers["kuaizi"].api_key, "env-key");
        assert_eq!(config.providers["volc"].base_url, "https://env.example.com/v1");
        assert_eq!(config.providers["volc"].api_key, "env-key");
        assert_eq!(config.roles.planner, "glm-5.2");
        assert_eq!(config.roles.executor, "doubao-seed-evolving");
        assert_eq!(config.roles.reviewer, "glm-5.2");
        let (planner_def, planner_provider) = config.role_model(LlmRole::Planner).expect("planner resolves");
        assert_eq!(planner_def.id, "glm-5.2");
        // ModelDef 字段不随 env 变化：glm-5.2 的 thinking=medium 原样带上。
        assert_eq!(planner_def.thinking, Some(crate::config::Thinking::Medium));
        // provider endpoint 已被 env 覆盖。
        assert_eq!(planner_provider.base_url, "https://env.example.com/v1");
    }

    #[test]
    fn env_id_pointer_to_undefined_id_fails() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(temp.path(), FULL_CONFIG);
        guard.use_xdg(temp.path());
        guard.clear_llm_env();
        std::env::set_var("LLM_PLANNER_MODEL", "kimi-ghost");

        let err = LlmConfig::from_env_or_file().expect_err("env pointer to ghost id must fail");
        assert!(
            matches!(err, crate::llm::LlmError::UnknownModelAlias { role: LlmRole::Planner, .. }),
            "expected UnknownModelAlias for planner, got {err}"
        );
    }

    #[test]
    fn file_fills_fields_missing_from_env() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        write_config(temp.path(), FULL_CONFIG);
        guard.use_xdg(temp.path());
        guard.clear_llm_env();
        // 已删除的 legacy 变量必须毫无作用：roles 仍取文件里的 id 指向。
        std::env::set_var("LLM_MODEL", "deleted-legacy-model");

        let config = LlmConfig::from_env_or_file().expect("file backstops env");
        assert_eq!(config.providers["kuaizi"].base_url, "https://kuaizi.example.com/v1");
        assert_eq!(config.providers["volc"].api_key, "volc-key");
        assert_eq!(config.roles.planner, "qwen3.8-max");
        assert_eq!(config.roles.executor, "doubao-seed-evolving");
        assert_eq!(config.roles.reviewer, "glm-5.2");
    }

    #[test]
    fn missing_file_env_only_has_no_models_to_point_at() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        guard.use_xdg(temp.path()); // 目录存在但没有 config.yml
        guard.clear_llm_env();
        std::env::set_var("LLM_PLANNER_MODEL", "qwen-env-planner");
        std::env::set_var("LLM_EXECUTOR_MODEL", "doubao-env-executor");
        std::env::set_var("LLM_REVIEWER_MODEL", "glm-env-reviewer");

        // v5 语义：ModelDef 与 provider 都只存在于文件；没有文件，env 指向无处可指。
        let err = LlmConfig::from_env_or_file().expect_err("env-only ids dangle without a file");
        assert!(
            matches!(err, crate::llm::LlmError::UnknownModelAlias { .. }),
            "expected UnknownModelAlias, got {err}"
        );
    }

    #[test]
    fn missing_everything_reports_missing_env() {
        let guard = EnvGuard::new();
        let temp = tempfile::tempdir().expect("tempdir");
        guard.use_xdg(temp.path());
        guard.clear_llm_env();

        let err = LlmConfig::from_env_or_file().expect_err("nothing configured must fail");
        assert!(
            matches!(err, crate::llm::LlmError::MissingEnv("LLM_PLANNER_MODEL")),
            "first missing piece should be the planner role binding, got {err}"
        );
    }
}
