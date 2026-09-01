//! Inspect Task 定义生成（Rust 生成 Python 文件）。

use anyhow::{Context, Result};

/// 内嵌的 pi 执行任务模板（见 templates/pi_task.py.tmpl）。
const PI_TASK_TEMPLATE: &str = include_str!("../templates/pi_task.py.tmpl");

/// 生成参数（全部经 token 替换注入模板）。
pub struct TaskGenParams {
    /// 沙箱 compose 文件绝对路径。
    pub compose_file: String,
    /// 契约 prompt（给执行者 pi 的任务描述）。
    pub contract_prompt: String,
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
pub fn generate_task_py(params: &TaskGenParams) -> Result<String> {
    let mut out = PI_TASK_TEMPLATE.to_string();

    // 所有字符串值经 JSON 编码注入，模板用 json.loads 还原——避免引号/花括号/反斜杠破坏 Python 字面量。
    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__CONTRACT_PROMPT_JSON__", json(&params.contract_prompt)?),
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
            anyhow::bail!("template missing token {token}");
        }
        out = out.replace(token, value);
    }

    // 防呆：替换后不得残留任何 __TOKEN__
    for token in [
        "__COMPOSE_FILE_JSON__",
        "__CONTRACT_PROMPT_JSON__",
        "__PI_MODEL_JSON__",
        "__WORKSPACE_DIR_JSON__",
        "__SANDBOX_USER_JSON__",
        "__RUN_ID_JSON__",
    ] {
        if out.contains(token) {
            anyhow::bail!("template token replacement incomplete: {token}");
        }
    }

    Ok(out)
}

fn json(s: &str) -> Result<String> {
    serde_json::to_string(s).context("json-encode template value")
}
