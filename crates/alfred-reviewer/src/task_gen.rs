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

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::contract::Contract;

    fn sample_request() -> OwnerRequest {
        OwnerRequest::new(
            "req-r2-plan",
            "create hello.txt",
            "create hello.txt with Hello",
            "hello.txt exists with content Hello",
        )
    }

    fn sample_dagspec() -> DagSpec {
        DagSpec::new(
            "req-r2-plan",
            vec![alfred_core::dagspec::PlanNode::new(
                "task-1",
                "write hello.txt",
                Contract {
                    prompt: "create hello.txt with Hello".into(),
                    acceptance_criteria: "hello.txt exists with Hello".into(),
                    reviewer_models: vec![],
                },
            )],
        )
    }

    #[test]
    fn generates_plan_review_py() {
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec(), None).unwrap();
        assert!(py.contains("OWNER_REQUEST = json.loads("));
        assert!(py.contains("plan_review_task"));
        assert!(!py.contains("__OWNER_REQUEST_JSON__"));
        assert!(!py.contains("__DAGSPEC_JSON__"));
    }

    #[test]
    fn plan_review_py_mandates_mount_semantics() {
        // R6f：离线计划审查（plan_review.py.tmpl）同步挂载语义路径翻译——
        // workspace_subdirs[0] 即节点工作区根 /workspace，契约"根目录"措辞应
        // 与首个子目录一致（离线直判路径与容器 PLAN_REVIEW_*_PROMPT 同一语义）。
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec(), None).unwrap();
        assert!(
            py.contains("workspace_subdirs[0]") && py.contains("workspace root (/workspace)"),
            "plan_review.py 必须含挂载语义路径翻译:\n{py}"
        );
        assert!(
            py.contains("host ws/<workspace_subdirs[0]>"),
            "plan_review.py 必须给宿主 ws 翻译:\n{py}"
        );
    }

    #[test]
    fn injects_session_doc() {
        // P2 修复：计划审查模板评分器输入含会话文档（审查者全可见）；属主消息
        // 经 conversation.json 可达（R6cReview：owner_message.txt 死输入已移除）。
        let mut doc = SessionDoc::new();
        doc.key_conclusions.push("用 Rust".into());
        doc.review_summary.push("属主反馈：方案符合需求。".into());
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec(), Some(&doc)).unwrap();
        assert!(
            py.contains("SESSION_DOC = None if _SESSION_DOC_RAW"),
            "SESSION_DOC parse line missing"
        );
        assert!(py.contains("用 Rust"), "session doc key_conclusion missing");
        assert!(
            py.contains("属主反馈：方案符合需求。"),
            "review_summary missing"
        );
        assert!(!py.contains("__SESSION_DOC_JSON__"));
    }

    #[test]
    fn missing_context_injects_null() {
        // 独立 plan-review 子命令无治理上下文 → 会话文档为 null
        let py = generate_plan_review_py(&sample_request(), &sample_dagspec(), None).unwrap();
        assert!(py.contains("SESSION_DOC = None"), "expected None session doc");
    }
    fn reviewer_params(mode: &str) -> ReviewerTaskGenParams {
        ReviewerTaskGenParams {
            compose_file: "/run/reviewer/compose.yaml".into(),
            mode: mode.into(),
            system_prompt: "你是治理系统的审查者。".into(),
            driver_prompt: "读 /inputs，写 /outputs/verdict.json".into(),
            output_file: "/outputs/verdict.json".into(),
            agt_ext: "/tmp/.agt/agt-policy.ts".into(),
            agt_policy_path: "/tmp/.agt/policy.json".into(),
            agt_audit_path: "/tmp/.agt/audit/audit.jsonl".into(),
            port_base: 13300,
            pi_model: "inspect-bridge/inspect".into(),
            workspace_dir: "/workspace".into(),
            sandbox_user: "root".into(),
            run_id: "run-reviewer-test".into(),
            settle_grace_seconds: 20.0,
        }
    }

    #[test]
    fn generates_reviewer_task_py() {
        let py = generate_reviewer_task_py(&reviewer_params("plan_review")).unwrap();
        assert!(py.contains(r#"COMPOSE_FILE = "/run/reviewer/compose.yaml""#));
        assert!(py.contains(r#"MODE = "plan_review""#));
        assert!(py.contains(r#"RUN_ID = "run-reviewer-test""#));
        assert!(py.contains(r#"PORT_BASE = int(13300)"#));
        assert!(py.contains(r#"PI_MODEL = "inspect-bridge/inspect""#));
        assert!(py.contains(r#"OUTPUT_FILE = "/outputs/verdict.json""#));
        assert!(py.contains(r#"AGT_EXT = "/tmp/.agt/agt-policy.ts""#));
        assert!(py.contains(r#"AGT_POLICY_PATH = "/tmp/.agt/policy.json""#));
        assert!(py.contains(r#"AGT_AUDIT_PATH = "/tmp/.agt/audit/audit.jsonl""#));
        // 不得残留 token
        assert!(!py.contains("__COMPOSE_FILE_JSON__"));
        assert!(!py.contains("__MODE_JSON__"));
        assert!(!py.contains("__AGT_POLICY_PATH_JSON__"));
        assert!(!py.contains("__AGT_AUDIT_PATH_JSON__"));
        assert!(!py.contains("__SETTLE_GRACE_SECONDS__"));
    }

    #[test]
    fn reviewer_task_exec_mode_and_empty_agt() {
        let mut p = reviewer_params("exec_review");
        p.agt_ext = "".into();
        p.agt_policy_path = "".into();
        p.agt_audit_path = "".into();
        let py = generate_reviewer_task_py(&p).unwrap();
        assert!(py.contains(r#"MODE = "exec_review""#));
        assert!(py.contains(r#"AGT_EXT = """#));
        assert!(py.contains(r#"AGT_POLICY_PATH = """#));
        // 空 AGT_EXT 时模板的 `if AGT_EXT:` 分支保留（运行期跳过 env 注入）
        assert!(py.contains("if AGT_EXT:"));
    }
}
