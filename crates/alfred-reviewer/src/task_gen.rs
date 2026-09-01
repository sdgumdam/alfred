//!
//! R6c：新增 reviewer 容器任务生成（`templates/reviewer_task.py.tmpl`）——
//! 计划审查/执行审查都走同一容器 pi 模板，差异只在 MODE + SYSTEM/DRIVER prompt。

use anyhow::{Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;

/// 内嵌的计划审查任务模板（见 templates/plan_review.py.tmpl）。
const PLAN_REVIEW_TEMPLATE: &str = include_str!("../templates/plan_review.py.tmpl");
/// 内嵌的 reviewer 容器任务模板（见 templates/reviewer_task.py.tmpl）。
const REVIEWER_TASK_TEMPLATE: &str = include_str!("../templates/reviewer_task.py.tmpl");

/// 生成 plan_review.py 内容。
///
/// 审查者全可见（§2.4）：除 OwnerRequest + DagSpec 外，还注入会话文档，供评分器
/// 对照"属主补充与审查摘要都骗不过审查"。会话文档可为空（如独立
/// `alfred plan-review` 子命令无治理上下文）。
pub fn generate_plan_review_py(
    request: &OwnerRequest,
    dagspec: &DagSpec,
    session_doc: Option<&SessionDoc>,
) -> Result<String> {
    let mut out = PLAN_REVIEW_TEMPLATE.to_string();

    // 对象经 serde_json::to_string 成 JSON 文本，再经 json() 编码成 Python
    // 字符串字面量；模板里 json.loads 还原成 dict（值语义一致）。
    let request_json = serde_json::to_string(request).context("serialize OwnerRequest")?;
    let dagspec_json = serde_json::to_string(dagspec).context("serialize DagSpec")?;
    let session_doc_json = match session_doc {
        Some(doc) => serde_json::to_string(doc).context("serialize SessionDoc")?,
        None => "null".to_string(),
    };
    let inject: &[(&str, String)] = &[
        ("__OWNER_REQUEST_JSON__", json(&request_json)?),
        ("__DAGSPEC_JSON__", json(&dagspec_json)?),
        ("__SESSION_DOC_JSON__", json(&session_doc_json)?),
        ("__RUN_ID_JSON__", json(&request.id)?),
    ];
    for (token, value) in inject {
        if !out.contains(token) {
            anyhow::bail!("plan_review template missing token {token}");
        }
        out = out.replace(token, value);
    }

    for token in [
        "__OWNER_REQUEST_JSON__",
        "__DAGSPEC_JSON__",
        "__SESSION_DOC_JSON__",
        "__RUN_ID_JSON__",
    ] {
        if out.contains(token) {
            anyhow::bail!("plan_review token replacement incomplete: {token}");
        }
    }

    Ok(out)
}

fn json(s: &str) -> Result<String> {
    serde_json::to_string(s).context("json-encode template value")
}
/// reviewer 容器任务生成参数（R6c）。
pub struct ReviewerTaskGenParams {
    /// 沙箱 compose 文件绝对路径。
    pub compose_file: String,
    /// "plan_review" | "exec_review"（容器内 pi 的任务模式）。
    pub mode: String,
    /// 容器侧审查 system prompt（计划忠实度 / 执行判分规则）。
    pub system_prompt: String,
    /// 容器侧 driver prompt（读 /inputs + /workspace → 按 SYSTEM_PROMPT → 写 /outputs/verdict.json）。
    pub driver_prompt: String,
    /// 容器内产出文件绝对路径（"/outputs/verdict.json"）。
    pub output_file: String,
    /// AGT 扩展路径（"/tmp/.agt/agt-policy.ts"）；空串 = 不加载。
    pub agt_ext: String,
    /// AGT 策略路径（"/tmp/.agt/policy.json"）；AGT 未启用时空串。
    pub agt_policy_path: String,
    /// AGT 审计路径（"/tmp/.agt/audit/audit.jsonl"）；AGT 未启用时空串。
    pub agt_audit_path: String,
    /// 桥代理端口基数（每样本自增）。
    pub port_base: u32,
    /// pi 模型（provider/model 形态，如 "inspect-bridge/inspect"）。
    pub pi_model: String,
    /// 容器内工作区路径（"/workspace"）。
    pub workspace_dir: String,
    /// 容器内执行用户（"root"）。
    pub sandbox_user: String,
    /// 样本 id（run id）。
    pub run_id: String,
    /// settled 后的宽限秒数（进程未在 EOF 退出则 kill）。
    pub settle_grace_seconds: f64,
}

/// 生成 reviewer 容器 task.py 内容（token 替换，机制与 planner 一致）。
pub fn generate_reviewer_task_py(params: &ReviewerTaskGenParams) -> Result<String> {
    let mut out = REVIEWER_TASK_TEMPLATE.to_string();

    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__MODE_JSON__", json(&params.mode)?),
        ("__SYSTEM_PROMPT_JSON__", json(&params.system_prompt)?),
        ("__DRIVER_PROMPT_JSON__", json(&params.driver_prompt)?),
        ("__OUTPUT_FILE_JSON__", json(&params.output_file)?),
        ("__AGT_EXT_JSON__", json(&params.agt_ext)?),
        ("__AGT_POLICY_PATH_JSON__", json(&params.agt_policy_path)?),
        ("__AGT_AUDIT_PATH_JSON__", json(&params.agt_audit_path)?),
        ("__PORT_BASE__", params.port_base.to_string()),
        ("__PI_MODEL_JSON__", json(&params.pi_model)?),
        ("__WORKSPACE_DIR_JSON__", json(&params.workspace_dir)?),
        ("__SANDBOX_USER_JSON__", json(&params.sandbox_user)?),
        ("__RUN_ID_JSON__", json(&params.run_id)?),
        (
            "__SETTLE_GRACE_SECONDS__",
            format!("{}", params.settle_grace_seconds),
        ),
    ];
    for (token, value) in inject {
        if !out.contains(token) {
            anyhow::bail!("reviewer_task template missing token {token}");
        }
        out = out.replace(token, value);
    }

    for token in [
        "__COMPOSE_FILE_JSON__",
        "__MODE_JSON__",
        "__SYSTEM_PROMPT_JSON__",
        "__DRIVER_PROMPT_JSON__",
        "__OUTPUT_FILE_JSON__",
        "__AGT_EXT_JSON__",
        "__AGT_POLICY_PATH_JSON__",
        "__AGT_AUDIT_PATH_JSON__",
        "__PORT_BASE__",
        "__PI_MODEL_JSON__",
        "__WORKSPACE_DIR_JSON__",
        "__SANDBOX_USER_JSON__",
        "__RUN_ID_JSON__",
        "__SETTLE_GRACE_SECONDS__",
    ] {
        if out.contains(token) {
            anyhow::bail!("reviewer_task token replacement incomplete: {token}");
        }
    }

    Ok(out)
}
