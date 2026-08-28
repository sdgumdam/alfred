//! 对话 agent（converse）：会话文档 + 属主消息 → 建图指令序列 → DagSpec。
//!
//! 施工清单 §2.4：规划器不是单个长对话 agent，而是"会话文档 + 短会话"。
//! 对话 agent 每轮都是短会话、无状态，喂给它两样东西：会话文档 + 属主这轮
//! 说的话；它吐出建图指令序列（§五 S0 迭代"builder API 模式"），alfred 逐条
//! 驱动 GraphBuilder 拼装出 DagSpec。
//!
//! 两种模式：
//! - 真 LLM（默认）：宿主 Rust 直调 OpenAI 兼容接口（§3.1），解析指令序列
//!    → GraphBuilder → DagSpec；每次调用落盘 llm-calls/（P9 证据）。
//! - 离线（`ALFRED_OFFLINE=1` + `ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json>`）：
//!   确定性直通，读注入的 DagSpec 返回；仍把 would-be 请求落盘 llm-calls/
//!   （e2e 从记录断言会话文档/伪装消息）。

use std::path::PathBuf;

use alfred_core::builder::{BuildInstruction, GraphBuilder};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_executor::config::ExecutorModel;
use anyhow::{bail, Context, Result};

use crate::llm::{log_llm_call, ChatMessage, LlmCallRecord, LlmClient};
use crate::disguise::sanitize_review_summary;

/// 会话文档对规划器的投影（方案B：第三段 review_summary → owner_feedback，内容中性化）。
/// 磁盘上 state.json 的会话文档保持原名 review_summary（审计真源不变），只改喂给
/// 规划器的投影。
#[derive(serde::Serialize)]
pub(crate) struct SessionDocProjection {
    key_file_paths: Vec<String>,
    key_conclusions: Vec<String>,
    #[serde(rename = "owner_feedback")]
    review_summary: Vec<String>,
}

/// 把会话文档投影为规划器可见形态：第三段改名 owner_feedback，且任一条目含禁词
/// 时回退中性模板。
pub(crate) fn project_session_doc(doc: &SessionDoc) -> SessionDocProjection {
    let mut review_summary = doc.review_summary.clone();
    sanitize_review_summary(&mut review_summary);
    SessionDocProjection {
        key_file_paths: doc.key_file_paths.clone(),
        key_conclusions: doc.key_conclusions.clone(),
        review_summary,
    }
}

/// 规划器建图 schema 提示词（唯一真源）：converse 的 system prompt 与容器侧
/// planner 任务（R6b）共用同一份。容器内 pi 按这份规则产建图指令序列。
pub(crate) const CONVERSE_SYSTEM_PROMPT: &str = r#"你是治理系统的规划器。把属主需求拆成一个任务 DAG（每个节点 = 契约 + 沙箱档案）。你只与属主对话。

你的输入：会话文档（记忆）+ 属主本轮消息。
输出：建图指令序列（JSON 数组）。每条指令是：
- {"op":"begin","request_id":"<需求id>"}
- {"op":"add_node","id":"task-1","summary":"<一句话摘要>","contract":{"prompt":"<任务描述>","acceptance_criteria":"<验收标准>"},"sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false}}
- {"op":"add_edge","from":"...","to":"..."}
- {"op":"set_routes","start":["task-1"]}
- {"op":"commit"}

规则：
- begin 必须最先，commit 必须最后，且至少一个节点。
- 每个节点的 contract.prompt 与 acceptance_criteria 必须非空。
- 默认用缺省沙箱（volumes 空、runtime null、packages 空、network false）；除非任务确实需要，才声明额外权限。
- 计划必须忠实反映属主需求，不要做属主没要求的事。
- 只输出 JSON 数组，不要任何多余文字。"#;

/// converse 选项。
#[derive(Debug, Clone)]
pub struct ConverseOptions {
    pub run_dir: PathBuf,
    pub model: ExecutorModel,
}

/// converse 结果。
#[derive(Debug, Clone)]
pub struct ConverseOutcome {
    pub dagspec: DagSpec,
    /// llm-calls/ 记录文件路径（P9 证据；e2e 从记录断言）。
    pub record_path: PathBuf,
}

/// 对话 agent：会话文档 + 属主消息 → DagSpec。
pub fn converse(
    opts: &ConverseOptions,
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
) -> Result<ConverseOutcome> {
    let messages = build_messages(request, doc, owner_message);
    let (dagspec, response, offline) = if std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1") {
        let plan = read_offline_plan()?;
        validate_dagspec(&plan, request)?;
        let resp = serde_json::to_string_pretty(&plan).context("serialize offline plan")?;
        (plan, resp, true)
    } else {
        let client = LlmClient::new(opts.model.clone());
        // 建图指令序列可能很长：给足 max_tokens，避免中途截断。
        let response = client.chat_with_max_tokens(&messages, 8192)?;
        let dagspec = instructions_to_dagspec(&response, request)?;
        (dagspec, response, false)
    };

    let record = LlmCallRecord {
        ts: alfred_core::util::now_rfc3339(),
        role: "converse".into(),
        model: opts.model.inspect_model_id(),
        offline,
        messages,
        response,
        ok: true,
        error: None,
    };
    let record_path = log_llm_call(&opts.run_dir, &record)?;
    Ok(ConverseOutcome {
        dagspec,
        record_path,
    })
}

/// 构建提示词（会话文档 + 属主本轮消息）。
pub fn build_messages(
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
) -> Vec<ChatMessage> {
    // 方案B：喂给规划器的是投影（第三段 owner_feedback + 内容中性化），磁盘真源不变。
    let session = serde_json::to_string_pretty(&project_session_doc(doc)).unwrap_or_default();
    let system = CONVERSE_SYSTEM_PROMPT.to_string();
    let user = format!(
        "需求 id：{}\n\n会话文档（记忆）：\n{session}\n\n属主本轮消息：\n{owner_message}",
        request.id
    );
    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// 解析 LLM 输出 → 指令序列 → GraphBuilder → DagSpec。
pub fn instructions_to_dagspec(text: &str, request: &OwnerRequest) -> Result<DagSpec> {
    let cleaned = strip_fences(text);
    let v: serde_json::Value = serde_json::from_str(&cleaned)
        .with_context(|| format!("build instructions not JSON: {cleaned}"))?;
    let insts: Vec<BuildInstruction> =
        serde_json::from_value(v).context("build instruction schema mismatch")?;
    if insts.is_empty() {
        bail!("build instruction sequence is empty");
    }
    let mut builder = GraphBuilder::new();
    for inst in insts {
        builder
            .apply(inst)
            .map_err(|e| anyhow::anyhow!("builder error: {e}"))?;
    }
    let dagspec = builder
        .build()
        .map_err(|e| anyhow::anyhow!("builder error: {e}"))?;
    validate_dagspec(&dagspec, request)?;
    Ok(dagspec)
}

/// 校验 DagSpec 与请求对齐（request_id 匹配、节点非空、单节点骨架范围）。
fn validate_dagspec(dagspec: &DagSpec, request: &OwnerRequest) -> Result<()> {
    if dagspec.request_id != request.id {
        bail!(
            "dagspec request_id '{}' != request.id '{}'",
            dagspec.request_id,
            request.id
        );
    }
    if dagspec.nodes.is_empty() {
        bail!("dagspec has no nodes");
    }
    // P2 修复：单节点骨架显式拒绝多节点 DAG（清单骨架范围：单节点验证；静默
    // 截断违反"无静默出口"）。在计划提交即报结构错误，执行侧不再截断。
    if dagspec.nodes.len() > 1 {
        bail!(
            "dagspec has {} nodes; 多节点 DAG 本骨架不支持（单节点验证范围）",
            dagspec.nodes.len()
        );
    }
    Ok(())
}

/// 离线模式读注入的计划文件（DagSpec JSON）。
fn read_offline_plan() -> Result<DagSpec> {
    let path = std::env::var("ALFRED_OFFLINE_PLAN_FILE")
        .context("ALFRED_OFFLINE=1 requires ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json>")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read offline plan {}", path))?;
    serde_json::from_str(&text).with_context(|| format!("parse offline plan {}", path))
}

/// 剥 markdown 代码围栏 / 只取首个平衡 JSON 数组。
pub fn strip_fences(text: &str) -> String {
    let trimmed = text.trim();
    // 先找 ```json ... ``` 围栏块
    if let Some(start) = trimmed.find("```") {
        if let Some(rel) = trimmed[start..].find('[') {
            let abs = start + rel;
            if let Some(end) = trimmed.rfind(']') {
                if end > abs {
                    return trimmed[abs..=end].to_string();
                }
            }
        }
    }
    // 直接取首个 [ ... ] 平衡块
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            if end > start {
                return trimmed[start..=end].to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::util::now_rfc3339;

    fn request() -> OwnerRequest {
        OwnerRequest::new(
            "req-1",
            "create hello.txt",
            "Create a file named hello.txt with content Hello",
            "hello.txt exists and its content is exactly 'Hello'",
        )
    }

    #[test]
    fn strips_fenced_json_array() {
        let text = "好的，这是指令序列：\n```json\n[{\"op\":\"begin\",\"request_id\":\"req-1\"}]\n```\n完";
        let cleaned = strip_fences(text);
        assert!(cleaned.starts_with('[') && cleaned.ends_with(']'), "{cleaned}");
    }

    #[test]
    fn instructions_to_dagspec_builds_plan() {
        let text = r#"[
            {"op":"begin","request_id":"req-1"},
            {"op":"add_node","id":"task-1","summary":"create hello.txt",
             "contract":{"prompt":"create hello.txt with Hello","acceptance_criteria":"hello.txt exists with Hello"},
             "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false}},
            {"op":"commit"}
        ]"#;
        let dag = instructions_to_dagspec(text, &request()).unwrap();
        assert_eq!(dag.nodes.len(), 1);
        assert_eq!(dag.nodes[0].id, "task-1");
        assert_eq!(dag.nodes[0].contract.prompt, "create hello.txt with Hello");
        assert!(!dag.nodes[0].sandbox.network);
    }

    #[test]
    fn instructions_reject_wrong_request_id() {
        let text = r#"[
            {"op":"begin","request_id":"req-OTHER"},
            {"op":"add_node","id":"t","summary":"s",
             "contract":{"prompt":"p","acceptance_criteria":"a"}},
            {"op":"commit"}
        ]"#;
        let err = instructions_to_dagspec(text, &request()).unwrap_err();
        assert!(format!("{err:#}").contains("request_id"), "{err:#}");
    }

    #[test]
    fn instructions_reject_empty_contract() {
        let text = r#"[
            {"op":"begin","request_id":"req-1"},
            {"op":"add_node","id":"t","summary":"s","contract":{"prompt":"","acceptance_criteria":"a"}},
            {"op":"commit"}
        ]"#;
        assert!(instructions_to_dagspec(text, &request()).is_err());
    }

    #[test]
    fn instructions_reject_multi_node_dag() {
        // P2 修复：单节点骨架显式拒绝多节点 DAG（不静默截断）。
        let text = r#"[
            {"op":"begin","request_id":"req-1"},
            {"op":"add_node","id":"task-1","summary":"s1",
             "contract":{"prompt":"p1","acceptance_criteria":"a1"},
             "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false}},
            {"op":"add_node","id":"task-2","summary":"s2",
             "contract":{"prompt":"p2","acceptance_criteria":"a2"},
             "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false}},
            {"op":"add_edge","from":"task-1","to":"task-2"},
            {"op":"set_routes","start":["task-1"]},
            {"op":"commit"}
        ]"#;
        let err = instructions_to_dagspec(text, &request()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("多节点"), "expected 多节点 rejection: {msg}");
        assert!(msg.contains("不支持"), "expected 不支持 in rejection: {msg}");
        assert!(msg.contains("2 nodes"), "expected node count in rejection: {msg}");
    }
    
    #[test]
    fn build_messages_include_session_and_owner_message() {
        let mut doc = SessionDoc::new();
        doc.key_conclusions.push("用 Rust".into());
        let msgs = build_messages(&request(), &doc, "属主：技术选型用 Rust");
        let user = &msgs[1];
        assert!(user.content.contains("req-1"));
        assert!(user.content.contains("用 Rust"));
        assert!(user.content.contains("属主：技术选型用 Rust"));
    }
    #[test]
    fn build_messages_projects_owner_feedback_without_forbidden_signal() {
        // 方案B：第三段改名 owner_feedback，且无 review_summary 字段名（防回归）。
        let mut doc = SessionDoc::new();
        doc.review_summary.push("属主反馈：方案符合需求，按此推进。".into());
        let msgs = build_messages(&request(), &doc, "属主：继续");
        let user = &msgs[1];
        assert!(
            !user.content.contains("review_summary"),
            "projection leaked field name: {}",
            user.content
        );
        assert!(
            user.content.contains("owner_feedback"),
            "projection missing owner_feedback: {}",
            user.content
        );
        // 投影内容不得含结构化否决信号（P7 禁词）
        assert!(
            crate::disguise::contains_forbidden_signal(&user.content).is_none(),
            "projected session leaked forbidden signal: {}",
            user.content
        );
        // 磁盘真源不变（调用方持有原 doc，其字段名仍是 review_summary）
        assert_eq!(doc.review_summary.len(), 1);
    }

    #[test]
    fn build_messages_neutralizes_contaminated_review_summary_entry() {
        let mut doc = SessionDoc::new();
        doc.review_summary
            .push("The plan was rejected by the reviewer: fails to match.".into());
        let msgs = build_messages(&request(), &doc, "属主：继续");
        let user = &msgs[1];
        assert!(
            user.content.contains("属主对上一轮计划有反馈，请重新理解需求"),
            "contaminated entry not neutralized: {}",
            user.content
        );
        assert!(
            !user.content.contains("rejected by the reviewer"),
            "raw review language leaked: {}",
            user.content
        );
    }

    #[test]
    fn system_prompt_mentions_only_owner_as_counterparty() {
        // P1：规划器提示词不得出现审查者/执行者角色引用
        let msgs = build_messages(&request(), &SessionDoc::new(), "属主：继续");
        let system = &msgs[0].content;
        for leak in ["审查者", "执行者", "reviewer", "executor"] {
            assert!(
                !system.contains(leak),
                "system prompt leaked counterparty role '{leak}': {system}"
            );
        }
        assert!(system.contains("属主"), "system prompt must mention 属主");
    }

    #[test]
    fn dagspec_round_trip_with_sandbox_default() {
        // 无 sandbox 字段的旧 dagspec 应解析为默认沙箱
        let json = r#"{"request_id":"req-1","nodes":[{"id":"t","summary":"s","contract":{"prompt":"p","acceptance_criteria":"a","reviewer_models":[]}}]}"#;
        let dag: DagSpec = serde_json::from_str(json).unwrap();
        assert!(!dag.nodes[0].sandbox.network);
        assert_eq!(dag.nodes[0].sandbox, alfred_core::SandboxProfile::default());
    }

    #[test]
    fn converse_offline_requires_plan_file() {
        let _guard = crate::llm::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // ALFRED_OFFLINE=1 但未设 PLAN_FILE → 报错（不静默）
        std::env::set_var("ALFRED_OFFLINE", "1");
        std::env::remove_var("ALFRED_OFFLINE_PLAN_FILE");
        let model = ExecutorModel {
            provider: "z".into(),
            model: "m".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let opts = ConverseOptions {
            run_dir: std::env::temp_dir().join("alfred-converse-offline-test"),
            model,
        };
        let err = converse(&opts, &request(), &SessionDoc::new(), "msg").unwrap_err();
        assert!(
            format!("{err:#}").contains("ALFRED_OFFLINE_PLAN_FILE"),
            "{err:#}"
        );
        std::env::remove_var("ALFRED_OFFLINE");
    }

    #[test]
    fn timestamp_helper() {
        assert!(now_rfc3339().contains('T'));
    }

    #[test]
    fn plan_node_sandbox_is_deny_unknown() {
        // sandbox 带未知字段 → 解析失败（deny_unknown_fields）
        let json = r#"{"request_id":"req-1","nodes":[{"id":"t","summary":"s","contract":{"prompt":"p","acceptance_criteria":"a"},"sandbox":{"volumes":[],"network":false,"extra":1}}]}"#;
        assert!(serde_json::from_str::<DagSpec>(json).is_err());
    }
}
