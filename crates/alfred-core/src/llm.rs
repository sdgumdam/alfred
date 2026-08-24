use crate::config::{ModelDef, Protocol, ProviderDef, RoleBindings, Thinking};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// OpenAI 兼容协议：POST {base_url}/chat/completions，
// Authorization: Bearer {api_key}，body 带 model + messages。
// endpoint/凭证随模型的 provider 走（见 config.yml 的 providers/models/roles 三块）。
const ENV_BASE_URL: &str = "LLM_BASE_URL";
const ENV_API_KEY: &str = "LLM_API_KEY";
const ENV_PLANNER_MODEL: &str = "LLM_PLANNER_MODEL";
const ENV_EXECUTOR_MODEL: &str = "LLM_EXECUTOR_MODEL";
const ENV_REVIEWER_MODEL: &str = "LLM_REVIEWER_MODEL";

static CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// LLM 使用角色：决定 chat_completion 经 roles 绑定取哪个模型、哪个 provider 拼请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRole {
    Planner,
    Executor,
    Reviewer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }
}

#[derive(Debug)]
pub enum LlmError {
    // 配置项既不在环境变量（LLM_BASE_URL / LLM_API_KEY /
    // LLM_PLANNER_MODEL / LLM_EXECUTOR_MODEL / LLM_REVIEWER_MODEL）
    // 也不在 XDG 配置文件里，才报 MissingEnv。
    MissingEnv(&'static str),
    // 配置文件存在但读不了 / 解析失败——坏文件要显式报错，不静默降级。
    Config(crate::config::ConfigError),
    // 角色（含 env 覆盖后的指向）在 models 里找不到对应定义。
    UnknownModelAlias { role: LlmRole, alias: String },
    // 模型的 provider 名在 providers 里没有定义（env 指向替换后可能出现）。
    UnknownProvider { model_id: String, provider: String },
    Http(reqwest::Error),
    Api { status: u16, body: String },
    // HTTP 成功但响应体里取不到正文（openai-compatible: choices[0].message.content；
    // anthropic: content[0].text）。
    MalformedResponse(String),
    // anthropic 协议请求体必须带 max_tokens，而模型没配 maxTokens。
    MissingMaxTokens(String),
    // 请求/响应落盘失败——llm-calls 是验收证据，写不进去必须显式报错。
    Io(std::io::Error),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(name) => write!(
                f,
                "llm config missing: {name} is set neither as an environment variable nor in the config file"
            ),
            Self::Config(err) => write!(f, "llm config file error: {err}"),
            Self::UnknownModelAlias { role, alias } => write!(
                f,
                "llm role {role:?} points at model id {alias:?}, \
                 which is not defined in models of the config file"
            ),
            Self::UnknownProvider { model_id, provider } => write!(
                f,
                "llm model {model_id:?} references provider {provider:?}, \
                 which is not defined in providers of the config file"
            ),
            Self::Http(err) => write!(f, "llm http call failed: {err}"),
            Self::Api { status, body } => write!(f, "llm api returned status {status}: {body}"),
            Self::MalformedResponse(body) => {
                write!(f, "llm response carries no extractable content: {body}")
            }
            Self::MissingMaxTokens(model_id) => write!(
                f,
                "model {model_id:?} is used over the anthropic protocol, \
                 which requires max_tokens; set maxTokens for this model in the config file"
            ),
            Self::Io(err) => write!(f, "cannot persist llm call record: {err}"),
        }
    }
}

impl std::error::Error for LlmError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Config(err) => Some(err),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for LlmError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err)
    }
}

impl From<std::io::Error> for LlmError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

// 同步 workspace，用 reqwest blocking；每次调用把请求/响应原文落盘到
// {run_dir}/llm-calls/<毫秒时间戳>-<进程内序号>.json 作验收证据。
pub fn chat_completion(role: LlmRole, messages: &[Message], run_dir: &Path) -> Result<String, LlmError> {
    // 配置缺失也算一次调用：落盘骨架请求 + 错误原因，验收证据不留洞。
    let config = match LlmConfig::from_env_or_file() {
        Ok(config) => config,
        Err(err) => {
            let request_body = json!({ "model": Value::Null, "messages": messages });
            record_call(run_dir, Protocol::OpenaiCompatible, &request_body, None, Some(&err.to_string()))?;
            return Err(err);
        }
    };

    let (model_def, provider_def) = match config.role_model(role) {
        Ok(resolved) => resolved,
        Err(err) => {
            let request_body = json!({ "model": Value::Null, "messages": messages });
            record_call(
                run_dir,
                Protocol::OpenaiCompatible,
                &request_body,
                None,
                Some(&err.to_string()),
            )?;
            return Err(err);
        }
    };
    let request_body = build_request(provider_def, model_def, messages)?;
    let url = request_url(provider_def);

    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build()?;
    let request = client.post(&url).json(&request_body);
    let request = match provider_def.protocol {
        Protocol::OpenaiCompatible => request.bearer_auth(&provider_def.api_key),
        // anthropic 用 x-api-key + 固定版本头，不用 Bearer。
        Protocol::Anthropic => request
            .header("x-api-key", &provider_def.api_key)
            .header("anthropic-version", "2023-06-01"),
    };
    let response = match request.send() {
        Ok(response) => response,
        Err(err) => {
            // 传输层失败没有响应体；原始 Http 错误优先，落盘尽力而为。
            let _ = record_call(run_dir, provider_def.protocol, &request_body, None, Some(&err.to_string()));
            return Err(LlmError::Http(err));
        }
    };

    let status = response.status();
    let body_text = response.text()?;
    let body_json: Value =
        serde_json::from_str(&body_text).unwrap_or_else(|_| Value::String(body_text.clone()));
    record_call(run_dir, provider_def.protocol, &request_body, Some(&body_json), None)?;

    if !status.is_success() {
        return Err(LlmError::Api { status: status.as_u16(), body: body_text });
    }
    body_json
        .pointer(content_pointer(provider_def.protocol))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(LlmError::MalformedResponse(body_text))
}

// 请求 URL / 请求体按 provider.protocol 分发；HTTP 发送共用 reqwest blocking。
fn request_url(provider_def: &ProviderDef) -> String {
    let base = provider_def.base_url.trim_end_matches('/');
    match provider_def.protocol {
        Protocol::OpenaiCompatible => format!("{base}/chat/completions"),
        Protocol::Anthropic => format!("{base}/v1/messages"),
    }
}

fn build_request(
    provider_def: &ProviderDef,
    model_def: &ModelDef,
    messages: &[Message],
) -> Result<Value, LlmError> {
    match provider_def.protocol {
        Protocol::OpenaiCompatible => Ok(build_openai_request(model_def, messages)),
        Protocol::Anthropic => build_anthropic_request(model_def, messages),
    }
}

// openai-compatible：model(id) + messages 必带；maxTokens、
// thinking(low/medium/high) → reasoning_effort 只在配置时带上；
// contextWindow 是 alfred 侧元数据、provider 是路由信息，绝不进请求体。
fn build_openai_request(model_def: &ModelDef, messages: &[Message]) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), model_def.id.clone().into());
    body.insert("messages".into(), json!(messages));
    if let Some(max_tokens) = model_def.max_tokens {
        body.insert("max_tokens".into(), max_tokens.into());
    }
    if let Some(effort) = model_def.thinking.and_then(Thinking::reasoning_effort) {
        body.insert("reasoning_effort".into(), effort.into());
    }
    Value::Object(body)
}

// anthropic：system 角色消息抽出到顶层 system 字段，其余消息进 messages；
// 请求体必须带 max_tokens（模型没配 maxTokens 是配置错误）。
fn build_anthropic_request(model_def: &ModelDef, messages: &[Message]) -> Result<Value, LlmError> {
    let max_tokens =
        model_def.max_tokens.ok_or(LlmError::MissingMaxTokens(model_def.id.clone()))?;
    let system_prompt: Vec<&str> = messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str())
        .collect();
    let conversation: Vec<&Message> =
        messages.iter().filter(|message| message.role != "system").collect();

    let mut body = serde_json::Map::new();
    body.insert("model".into(), model_def.id.clone().into());
    if !system_prompt.is_empty() {
        body.insert("system".into(), system_prompt.join("\n").into());
    }
    body.insert("messages".into(), json!(conversation));
    body.insert("max_tokens".into(), max_tokens.into());
    if let Some(effort) = model_def.thinking.and_then(Thinking::reasoning_effort) {
        body.insert("reasoning_effort".into(), effort.into());
    }
    Ok(Value::Object(body))
}

fn content_pointer(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiCompatible => "/choices/0/message/content",
        Protocol::Anthropic => "/content/0/text",
    }
}

fn protocol_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiCompatible => "openai-compatible",
        Protocol::Anthropic => "anthropic",
    }
}

/// 解析后的 LLM 配置（环境变量 > XDG 配置文件）。
/// 三块平级：providers（endpoint+凭证）/ models（数组）/ roles（角色 → 模型 id）。
/// executor 容器注入与 reviewer inspect --model 派生都从这里取。
#[derive(Debug)]
pub struct LlmConfig {
    pub providers: HashMap<String, ProviderDef>,
    pub models: Vec<ModelDef>,
    pub roles: RoleBindings,
}

impl LlmConfig {
    // 优先级：环境变量 > 配置文件。
    // LLM_BASE_URL / LLM_API_KEY 覆盖所有 provider 的 endpoint/凭证（单 provider 调试场景）；
    // LLM_*_MODEL 覆盖 roles 里的 id 指向（不是 ModelDef 字段）；
    // ModelDef（id/provider/三参数）只从文件读，因此文件总是会被读取。
    pub fn from_env_or_file() -> Result<Self, LlmError> {
        let file = crate::config::load_llm_config().map_err(LlmError::Config)?;
        let env_base_url = optional_env(ENV_BASE_URL);
        let env_api_key = optional_env(ENV_API_KEY);

        let providers: HashMap<String, ProviderDef> = file
            .as_ref()
            .map(|config| {
                config
                    .providers
                    .iter()
                    .map(|(name, def)| {
                        (
                            name.clone(),
                            ProviderDef {
                                base_url: env_base_url.clone().unwrap_or_else(|| def.base_url.clone()),
                                api_key: env_api_key.clone().unwrap_or_else(|| def.api_key.clone()),
                                // env 只覆盖 endpoint/凭证，协议永远跟文件走。
                                protocol: def.protocol,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let models = file.as_ref().map(|config| config.models.clone()).unwrap_or_default();

        let roles = RoleBindings {
            planner: resolve(
                ENV_PLANNER_MODEL,
                optional_env(ENV_PLANNER_MODEL),
                file.as_ref().map(|config| config.roles.planner.as_str()),
            )?,
            executor: resolve(
                ENV_EXECUTOR_MODEL,
                optional_env(ENV_EXECUTOR_MODEL),
                file.as_ref().map(|config| config.roles.executor.as_str()),
            )?,
            reviewer: resolve(
                ENV_REVIEWER_MODEL,
                optional_env(ENV_REVIEWER_MODEL),
                file.as_ref().map(|config| config.roles.reviewer.as_str()),
            )?,
        };

        let config = Self { providers, models, roles };
        // env 覆盖 id 指向后仍要校验指向存在——env 可以把 roles 指到文件里
        // 不存在的 id，那同样是配置错误，必须显式报错。
        for role in [LlmRole::Planner, LlmRole::Executor, LlmRole::Reviewer] {
            config.role_model(role)?;
        }
        Ok(config)
    }

    pub fn role_alias(&self, role: LlmRole) -> &str {
        match role {
            LlmRole::Planner => &self.roles.planner,
            LlmRole::Executor => &self.roles.executor,
            LlmRole::Reviewer => &self.roles.reviewer,
        }
    }

    // 角色 → 模型定义 + 其 provider 的 endpoint/凭证。
    pub fn role_model(&self, role: LlmRole) -> Result<(&ModelDef, &ProviderDef), LlmError> {
        let alias = self.role_alias(role);
        let model_def = self
            .models
            .iter()
            .find(|def| def.id == alias)
            .ok_or(LlmError::UnknownModelAlias { role, alias: alias.to_owned() })?;
        let provider_def = self.providers.get(&model_def.provider).ok_or(LlmError::UnknownProvider {
            model_id: model_def.id.clone(),
            provider: model_def.provider.clone(),
        })?;
        Ok((model_def, provider_def))
    }
}

fn optional_env(name: &'static str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn resolve(env_name: &'static str, env: Option<String>, file: Option<&str>) -> Result<String, LlmError> {
    env.or_else(|| file.map(str::to_owned)).ok_or(LlmError::MissingEnv(env_name))
}

fn record_call(
    run_dir: &Path,
    protocol: Protocol,
    request: &Value,
    response: Option<&Value>,
    error: Option<&str>,
) -> Result<(), LlmError> {
    let dir = run_dir.join("llm-calls");
    fs::create_dir_all(&dir)?;
    let millis =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or_default();
    let seq = CALL_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut record = serde_json::Map::new();
    // protocol 一并落盘：同一份 llm-calls 证据里能看出走的是哪条协议路径。
    record.insert("protocol".into(), protocol_label(protocol).into());
    record.insert("request".into(), request.clone());
    if let Some(response) = response {
        record.insert("response".into(), response.clone());
    }
    if let Some(error) = error {
        record.insert("error".into(), error.into());
    }
    let rendered =
        serde_json::to_string_pretty(&Value::Object(record)).expect("record serialization cannot fail");
    fs::write(dir.join(format!("{millis}-{seq}.json")), rendered)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    // 请求体拼装的纯函数测试：不碰 env、不发 HTTP，只验 build_openai_request / build_anthropic_request 的输出形状。
    use super::*;

    fn model_def(model: &str) -> ModelDef {
        ModelDef {
            id: model.into(),
            provider: "kuaizi".into(),
            context_window: None,
            max_tokens: None,
            thinking: None,
        }
    }

    fn messages() -> Vec<Message> {
        vec![Message::system("sys"), Message::user("hi")]
    }

    #[test]
    fn minimal_model_def_emits_only_model_and_messages() {
        let body = build_openai_request(&model_def("kimi-k3"), &messages());
        assert_eq!(body["model"], "kimi-k3");
        assert_eq!(body["messages"][0]["role"], "system");
        let object = body.as_object().expect("body is an object");
        assert_eq!(object.len(), 2, "optional fields must be absent: {object:?}");
    }

    #[test]
    fn thinking_off_omits_reasoning_effort() {
        let mut def = model_def("kimi-k3");
        def.thinking = Some(Thinking::Off);
        def.max_tokens = Some(1024);
        let body = build_openai_request(&def, &messages());
        assert!(body.get("reasoning_effort").is_none(), "off must not emit reasoning_effort");
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn thinking_high_maps_into_reasoning_effort() {
        let mut def = model_def("kimi-k3");
        def.thinking = Some(Thinking::High);
        let body = build_openai_request(&def, &messages());
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn provider_and_context_window_never_enter_request_body() {
        let mut def = model_def("kimi-k3");
        def.context_window = Some(1_048_576);
        def.max_tokens = Some(131_072);
        def.thinking = Some(Thinking::Low);
        let body = build_openai_request(&def, &messages());
        assert!(body.get("contextWindow").is_none(), "contextWindow is alfred-side metadata");
        assert!(body.get("provider").is_none(), "provider is routing info, never a field");
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["max_tokens"], 131_072);
    }
    // ---- anthropic 协议路径 ----

    #[test]
    fn anthropic_request_extracts_system_and_requires_max_tokens() {
        let mut def = model_def("claude-opus");
        def.max_tokens = Some(4096);
        let msgs = vec![
            Message::system("你是助手"),
            Message::user("hi"),
            Message::system("第二条 system"),
        ];
        let body = build_anthropic_request(&def, &msgs).expect("max_tokens present");
        assert_eq!(body["model"], "claude-opus");
        // system 消息合并进顶层 system 字段，不进 messages。
        assert_eq!(body["system"], "你是助手\n第二条 system");
        let conversation = body["messages"].as_array().expect("messages array");
        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation[0]["role"], "user");
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn anthropic_request_without_max_tokens_is_config_error() {
        let def = model_def("claude-opus"); // max_tokens: None
        let err = build_anthropic_request(&def, &messages()).expect_err("max_tokens required");
        assert!(
            matches!(&err, LlmError::MissingMaxTokens(id) if id == "claude-opus"),
            "expected MissingMaxTokens naming the model, got {err}"
        );
    }

    #[test]
    fn anthropic_omits_system_field_when_no_system_messages() {
        let mut def = model_def("claude-opus");
        def.max_tokens = Some(1024);
        let body = build_anthropic_request(&def, &[Message::user("hi")]).expect("ok");
        assert!(body.get("system").is_none(), "no system messages → no system field");
    }

    #[test]
    fn protocol_dispatch_selects_url_body_and_auth() {
        let openai = ProviderDef {
            base_url: "https://gw.example.com".into(),
            api_key: "k".into(),
            protocol: Protocol::OpenaiCompatible,
        };
        let anthropic = ProviderDef {
            base_url: "https://anthropic.example.com".into(),
            api_key: "k".into(),
            protocol: Protocol::Anthropic,
        };
        assert_eq!(request_url(&openai), "https://gw.example.com/chat/completions");
        assert_eq!(request_url(&anthropic), "https://anthropic.example.com/v1/messages");
        assert_eq!(content_pointer(Protocol::OpenaiCompatible), "/choices/0/message/content");
        assert_eq!(content_pointer(Protocol::Anthropic), "/content/0/text");
        assert_eq!(protocol_label(Protocol::OpenaiCompatible), "openai-compatible");
        assert_eq!(protocol_label(Protocol::Anthropic), "anthropic");

        let mut def = model_def("claude-opus");
        def.max_tokens = Some(1024);
        let body = build_request(&anthropic, &def, &messages()).expect("anthropic body");
        assert_eq!(body["system"], "sys");
        let body = build_request(&openai, &model_def("kimi-k3"), &messages()).expect("openai body");
        assert_eq!(body["messages"][0]["role"], "system");
    }
}
