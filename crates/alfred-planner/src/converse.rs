//! 对话 agent（converse）：会话文档 + 属主消息 → §2.4 两分支。
//!
//! 施工清单 §2.4：规划器不是单个长对话 agent，而是"会话文档 + 短会话"。
//! 对话 agent 每轮都是短会话、无状态，喂给它两样东西：会话文档 + 属主这轮
//! 说的话；它吐出两种东西之一：
//! - **建图指令序列**（§五 S0 迭代"builder API 模式"），alfred 逐条驱动
//!   GraphBuilder 拼装出 DagSpec，交给编排核心（→ 计划审查/执行）。
//! - **给属主的答复**（纯文本；对话性轮次，不强制产 DagSpec——多轮对话的
//!   答复侧，对话产出建图指令后交编排器接管）。
//!
//! 两种模式：
//! - 真 LLM（默认）：宿主 Rust 驱动 planner 容器（pi agent 在容器内读挂载输入、
//!   按两分支规则产 /outputs/instructions.json 或 /outputs/reply.txt），宿主按
//!   产出文件分派两分支 → GraphBuilder → DagSpec 或答复；每次调用落盘
//!   llm-calls/（P9 证据）。
//! - 离线（`ALFRED_OFFLINE=1`）：确定性直通，两分支由注入文件二选一——
//!   `ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json>` → 建图指令分支；
//!   `ALFRED_OFFLINE_REPLY_FILE=<reply.txt>` → 答复分支；仍把 would-be 请求
//!   落盘 llm-calls/（e2e 从记录断言会话文档/伪装消息）。

use std::path::PathBuf;

use alfred_core::builder::{BuildInstruction, GraphBuilder};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_executor::config::ExecutorModel;
use anyhow::{bail, Context, Result};

use crate::llm::{log_llm_call, ChatMessage, LlmCallRecord};
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
pub(crate) const CONVERSE_SYSTEM_PROMPT: &str = r#"你是治理系统的规划器，是对话 agent。把属主需求拆成一个任务 DAG（每个节点 = 契约 + 沙箱档案）。你只与属主对话。

你的输入：会话文档（记忆）+ 属主本轮消息。
输出二选一：
- 当属主消息是要制定/修改计划（或你判断需要产出建图指令）时：输出建图指令序列（JSON 数组）。每条指令是：
- {"op":"begin","request_id":"<需求id>"}
- {"op":"add_node","id":"task-1","summary":"<一句话摘要>","contract":{"prompt":"<任务描述>","acceptance_criteria":"<验收标准>"},"sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}}
- {"op":"add_edge","from":"...","to":"..."}
- {"op":"set_routes","start":["task-1"]}
- {"op":"commit"}
- 当属主消息是对话性提问/澄清/讨论、暂不需要改计划时：用自然语言直接答复属主，不输出建图指令 JSON。

规则：
- 建图指令 JSON 与自然语言答复只能二选一，不要混合；产建图指令时只输出 JSON 数组，不要任何多余文字。
- begin 必须最先，commit 必须最后，且至少一个节点。
- 每个节点的 contract.prompt 与 acceptance_criteria 必须非空。
- 每个节点必须声明非空 workspace_subdirs（sandbox.workspace_subdirs，工作区子目录列表，如 ["src"]）：声明的是该节点可见/可写的工作区范围（节点只能看到这些子目录），这是强制约束；空/缺省声明 = 计划不合格。挂载语义：首个子目录挂为该节点工作区根 /workspace，其余子目录挂为 /workspace/<子目录>。
- workspace_subdirs 必须声明具体子目录名：按任务产物位置声明（如任务写 src/ 下则声明 ["src"]）；禁止声明 "."（工作区根，挂载语义下根由系统接管，声明子目录必须是具体相对目录）；禁止声明与挂载根同名的目录名（如 "workspace"，避免嵌套歧义）；任务描述（contract.prompt）里"根目录"措辞应与声明的子目录一致（首个子目录即该节点工作区根 /workspace）。
- 默认用缺省沙箱（volumes 空、runtime null、packages 空、network false），workspace_subdirs 按上条必须非空；除非任务确实需要，才声明额外权限。
- 计划必须忠实反映属主需求，不要做属主没要求的事。
- 答复属主时用自然语言直接、清晰，不要夹带建图指令。"#;

/// converse 选项。
#[derive(Debug, Clone)]
pub struct ConverseOptions {
    pub run_dir: PathBuf,
    pub model: ExecutorModel,
    /// R6b：planner 容器驱动选项（起容器跑 converse；桥代发 LLM）。
    pub container: crate::container::PlannerContainerOptions,
}

impl ConverseOptions {
    pub fn new(run_dir: PathBuf, model: ExecutorModel) -> Self {
        Self {
            run_dir,
            model,
            container: crate::container::PlannerContainerOptions::default(),
        }
    }
}

/// converse 结果（§2.4 两分支：建图指令 或 给属主的答复）。
#[derive(Debug, Clone)]
pub enum ConverseOutcome {
    /// 建图指令序列 → DagSpec（交编排器接管：结构检查 → 计划审查 → 执行）。
    Instructions {
        dagspec: DagSpec,
        /// llm-calls/ 记录文件路径（P9 证据；e2e 从记录断言）。
        record_path: PathBuf,
    },
    /// 纯文本答复（给属主；不强制产 DagSpec——对话继续，多轮轮次可再喂入）。
    Reply {
        reply: String,
        /// llm-calls/ 记录文件路径（P9 证据；e2e 从记录断言）。
        record_path: PathBuf,
    },
}

/// 对话 agent：会话文档 + 属主消息 → §2.4 两分支。
///
/// 每轮都是短会话、无状态（喂会话文档 + 本轮属主消息）。多轮对话 = 属主消息
/// 轮次可多次喂入：对话产出建图指令（`Instructions`）后交编排器接管；否则返回
/// `Reply`（对话继续，下一轮再喂入）。
pub fn converse(
    opts: &ConverseOptions,
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
) -> Result<ConverseOutcome> {
    let messages = build_messages(request, doc, owner_message);
    let (outcome, response, offline, transport) =
        if std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1") {
            // 离线模式保留：不经容器（现状直通）；两分支由注入文件二选一。
            let (outcome, response) = converse_offline(request)?;
            (outcome, response, true, "offline")
        } else {
            // R6b：容器内 pi 读输入跑 converse（桥代发 LLM），宿主读 /outputs 产出
            // （/outputs/instructions.json 或 /outputs/reply.txt，task.py 已强制恰好一个）。
            let out = crate::container::run_converse_in_container(
                &opts.container, &opts.model, request, doc, owner_message,
            )?;
            let outcome = match out.produced_file.as_str() {
                crate::container::CONVERSE_OUTPUT_FILE => {
                    let dagspec = instructions_to_dagspec(&out.output_text, request)?;
                    ConverseOutcome::Instructions {
                        dagspec,
                        record_path: PathBuf::new(),
                    }
                }
                crate::container::CONVERSE_REPLY_FILE => ConverseOutcome::Reply {
                    reply: out.output_text.trim().to_string(),
                    record_path: PathBuf::new(),
                },
                other => bail!("converse container produced unexpected output file {other}"),
            };
            (outcome, out.output_text, false, "container_bridge")
        };

    let record = LlmCallRecord {
        ts: alfred_core::util::now_rfc3339(),
        role: "converse".into(),
        model: opts.model.inspect_model_id(),
        offline,
        transport: transport.to_string(),
        messages,
        response,
        ok: true,
        error: None,
    };
    let record_path = log_llm_call(&opts.run_dir, &record)?;
    Ok(match outcome {
        ConverseOutcome::Instructions { dagspec, .. } => ConverseOutcome::Instructions {
            dagspec,
            record_path,
        },
        ConverseOutcome::Reply { reply, .. } => ConverseOutcome::Reply {
            reply,
            record_path,
        },
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

/// 离线确定性直通（`ALFRED_OFFLINE=1`）：§2.4 两分支由注入文件二选一。
///
/// - `ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json>` → 建图指令分支（validate → DagSpec）。
/// - `ALFRED_OFFLINE_REPLY_FILE=<reply.txt>` → 答复分支（纯文本）。
/// 两者同时/都不设 → 显式报错（不静默）。返回 (outcome, response 文本)，
/// response 供 llm-calls 记录（与容器路径同构）。
fn converse_offline(request: &OwnerRequest) -> Result<(ConverseOutcome, String)> {
    let plan_file = std::env::var("ALFRED_OFFLINE_PLAN_FILE").ok();
    let reply_file = std::env::var("ALFRED_OFFLINE_REPLY_FILE").ok();
    match (plan_file, reply_file) {
        (Some(_), Some(_)) => bail!(
            "ALFRED_OFFLINE_PLAN_FILE 与 ALFRED_OFFLINE_REPLY_FILE 同时设置（二选一）"
        ),
        (Some(path), None) => {
            let plan = read_offline_plan(&path)?;
            validate_dagspec(&plan, request)?;
            let resp = serde_json::to_string_pretty(&plan).context("serialize offline plan")?;
            Ok((
                ConverseOutcome::Instructions {
                    dagspec: plan,
                    record_path: PathBuf::new(),
                },
                resp,
            ))
        }
        (None, Some(path)) => {
            let reply = std::fs::read_to_string(&path)
                .with_context(|| format!("read offline reply {}", path))?;
            let reply = reply.trim().to_string();
            if reply.is_empty() {
                bail!("ALFRED_OFFLINE_REPLY_FILE 为空");
            }
            Ok((
                ConverseOutcome::Reply {
                    reply: reply.clone(),
                    record_path: PathBuf::new(),
                },
                reply,
            ))
        }
        (None, None) => bail!(
            "ALFRED_OFFLINE=1 requires ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json> or ALFRED_OFFLINE_REPLY_FILE=<reply.txt>"
        ),
    }
}

/// 离线模式读注入的计划文件（DagSpec JSON）。
fn read_offline_plan(path: &str) -> Result<DagSpec> {
    let text = std::fs::read_to_string(path)
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
             "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}},
            {"op":"commit"}
        ]"#;
        let dag = instructions_to_dagspec(text, &request()).unwrap();
        assert_eq!(dag.nodes.len(), 1);
        assert_eq!(dag.nodes[0].id, "task-1");
        assert_eq!(dag.nodes[0].contract.prompt, "create hello.txt with Hello");
        assert!(!dag.nodes[0].sandbox.network);
        // R6e(补C)：add_node 指令带 workspace_subdirs → dagspec 节点声明保留
        assert_eq!(dag.nodes[0].sandbox.workspace_subdirs, vec!["src"]);
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
             "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}},
            {"op":"add_node","id":"task-2","summary":"s2",
             "contract":{"prompt":"p2","acceptance_criteria":"a2"},
             "sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["tests"]}},
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
    fn system_prompt_mandates_workspace_subdirs() {
        // R6e(补C)：规划器提示词必须强制每个节点声明非空 workspace_subdirs——
        // 真规划器按此产出声明，才能通过计划审查结构闸门（补A）。示例 JSON 也要带声明。
        let msgs = build_messages(&request(), &SessionDoc::new(), "属主：继续");
        let system = &msgs[0].content;
        assert!(
            system.contains("每个节点必须声明非空 workspace_subdirs"),
            "system prompt must mandate non-empty workspace_subdirs: {system}"
        );
        assert!(
            system.contains("强制约束"),
            "system prompt must state mandatory constraint: {system}"
        );
        assert!(
            system.contains("workspace_subdirs"),
            "system prompt must mention workspace_subdirs: {system}"
        );

        // add_node 示例本身带 workspace_subdirs 声明（执行者可见子集）
        assert!(
            system.contains("\"workspace_subdirs\":[\"src\"]"),
            "add_node example must declare workspace_subdirs: {system}"
        );
    }

    #[test]
    fn system_prompt_banishes_root_and_mount_root_subdir_names() {
        // R6f(补)：规划器提示词必须收敛 workspace_subdirs 命名——禁止声明 "."
        // （工作区根，挂载语义下根由系统接管）与挂载根同名目录（如 "workspace"，
        // 嵌套歧义），并提示按任务产物位置声明具体子目录。实测边界：["."] 被
        // executor validate_workspace_subdir 以 CurDir 拒绝；["workspace"] 产生
        // ws/workspace 嵌套；["src"] + "in the workspace" → 正确落 ws/src/hello.txt。
        let msgs = build_messages(&request(), &SessionDoc::new(), "属主：继续");
        let system = &msgs[0].content;
        // 禁止声明 "."（工作区根）
        assert!(
            system.contains("禁止声明 \".\""),
            "system prompt must ban '.' as workspace_subdirs: {system}"
        );
        assert!(
            system.contains("根由系统接管"),
            "system prompt must explain root is system-managed: {system}"
        );
        // 禁止声明与挂载根同名的目录名（嵌套歧义）
        assert!(
            system.contains("禁止声明与挂载根同名"),
            "system prompt must ban mount-root-identical subdir names: {system}"
        );
        assert!(
            system.contains("\"workspace\""),
            "system prompt must name the mount-root collision example: {system}"
        );
        // 提示按任务产物位置声明具体子目录名
        assert!(
            system.contains("必须声明具体子目录名"),
            "system prompt must require concrete subdir names: {system}"
        );
        assert!(
            system.contains("按任务产物位置声明"),
            "system prompt must advise declaring by artifact location: {system}"
        );
        // contract.prompt 里"根目录"措辞应与声明的子目录一致（首个子目录即工作区根）
        assert!(
            system.contains("\"根目录\"措辞应与声明的子目录一致"),
            "system prompt must align root-dir wording with declared subset: {system}"
        );
    }

    #[test]
    fn converse_offline_requires_plan_file() {
        let _guard = crate::llm::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // ALFRED_OFFLINE=1 但未设 PLAN_FILE → 报错（不静默）
        std::env::set_var("ALFRED_OFFLINE", "1");
        std::env::remove_var("ALFRED_OFFLINE_PLAN_FILE");
        std::env::remove_var("ALFRED_OFFLINE_REPLY_FILE");
        let opts = ConverseOptions::new(
            std::env::temp_dir().join("alfred-converse-offline-test"),
            offline_model(),
        );
        let err = converse(&opts, &request(), &SessionDoc::new(), "msg").unwrap_err();
        assert!(
            format!("{err:#}").contains("ALFRED_OFFLINE_PLAN_FILE"),
            "{err:#}"
        );
        std::env::remove_var("ALFRED_OFFLINE");
        std::env::remove_var("ALFRED_OFFLINE_PLAN_FILE");
        std::env::remove_var("ALFRED_OFFLINE_REPLY_FILE");
    }

    fn offline_model() -> ExecutorModel {
        ExecutorModel {
            provider: "z".into(),
            model: "m".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
            raw_id: false,
        }
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
