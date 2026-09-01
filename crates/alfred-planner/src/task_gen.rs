//! planner Inspect Task 定义生成（Rust 生成 Python 文件）。
//!
//! 复用 executor 的 token 替换机制：字符串值经 JSON 编码注入（值语义一致），
//! 数字 token 注入裸数字。模板见 templates/planner_task.py.tmpl。

use anyhow::{Context, Result};

/// 内嵌的 planner pi 任务模板（见 templates/planner_task.py.tmpl）。
const PLANNER_TASK_TEMPLATE: &str = include_str!("../templates/planner_task.py.tmpl");

/// 生成参数（全部经 token 替换注入模板）。
pub struct PlannerTaskGenParams {
    /// 沙箱 compose 文件绝对路径。
    pub compose_file: String,
    /// "converse" | "maintain"（容器内 pi 的任务模式）。
    pub mode: String,
    /// 容器侧 system prompt（converse 建图 schema / maintain 会话文档维护，逐字搬自 converse.rs / maintain.rs）。
    pub system_prompt: String,
    /// 容器侧 driver prompt（读 /inputs → 按 SYSTEM_PROMPT 规则 → 写 /outputs/<file>）。
    pub driver_prompt: String,
    /// 容器内主产出文件绝对路径（"/outputs/instructions.json" 或 "/outputs/session.json"）。
    pub output_file: String,
    /// 容器内次产出文件绝对路径（converse §2.4 两分支答复侧 "/outputs/reply.txt"）；
    /// 空串 = 单文件（maintain）。
    pub output_file_alt: String,
    /// AGT 扩展路径（"/tmp/.agt/agt-policy.ts"）；空串 = 不加载。
    pub agt_ext: String,
    /// AGT 策略文件容器内路径（"/tmp/.agt/policy.json"）；与 `agt_ext` 同空。
    pub agt_policy_path: String,
    /// AGT 审计文件容器内路径（"/tmp/.agt/audit/audit.jsonl"）；与 `agt_ext` 同空。
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

/// 生成 task.py 内容。
pub fn generate_planner_task_py(params: &PlannerTaskGenParams) -> Result<String> {
    let mut out = PLANNER_TASK_TEMPLATE.to_string();

    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__MODE_JSON__", json(&params.mode)?),
        ("__SYSTEM_PROMPT_JSON__", json(&params.system_prompt)?),
        ("__DRIVER_PROMPT_JSON__", json(&params.driver_prompt)?),
        ("__OUTPUT_FILE_JSON__", json(&params.output_file)?),
        ("__OUTPUT_FILE_ALT_JSON__", json(&params.output_file_alt)?),
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
            anyhow::bail!("planner_task template missing token {token}");
        }
        out = out.replace(token, value);
    }

    // 防呆：替换后不得残留任何 __TOKEN__
    for token in [
        "__COMPOSE_FILE_JSON__",
        "__MODE_JSON__",
        "__SYSTEM_PROMPT_JSON__",
        "__DRIVER_PROMPT_JSON__",
        "__OUTPUT_FILE_JSON__",
        "__OUTPUT_FILE_ALT_JSON__",
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
            anyhow::bail!("planner_task token replacement incomplete: {token}");
        }
    }

    Ok(out)
}

fn json(s: &str) -> Result<String> {
    serde_json::to_string(s).context("json-encode template value")
}
