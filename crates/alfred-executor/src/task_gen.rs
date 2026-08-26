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
    /// 契约验收标准（给审查者 scorer 的判分依据——投影物理隔离）。
    pub acceptance_criteria: String,
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
        ("__ACCEPTANCE_CRITERIA_JSON__", json(&params.acceptance_criteria)?),
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
        "__ACCEPTANCE_CRITERIA_JSON__",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_python_with_values() {
        let py = generate_task_py(&TaskGenParams {
            compose_file: "/run/executor.compose.yaml".into(),
            contract_prompt: "Create hello.txt with content Hello".into(),
            acceptance_criteria: "hello.txt exists with content Hello".into(),
            port_base: 13100,
            pi_model: "inspect-bridge/inspect".into(),
            workspace_dir: "/workspace".into(),
            sandbox_user: "root".into(),
            run_id: "run-test-1".into(),
            settle_grace_seconds: 20.0,
        })
        .unwrap();

        assert!(py.contains(r#"COMPOSE_FILE = "/run/executor.compose.yaml""#));
        assert!(py.contains(r#"RUN_ID = "run-test-1""#));
        assert!(py.contains(r#"PORT_BASE = int(13100)"#));
        assert!(py.contains(r#"PI_MODEL = "inspect-bridge/inspect""#));
        assert!(py.contains(r#"ACCEPTANCE_CRITERIA = "hello.txt exists with content Hello""#));
        // 不得残留 token
        assert!(!py.contains("__COMPOSE_FILE_JSON__"));
        assert!(!py.contains("__CONTRACT_PROMPT_JSON__"));
        assert!(!py.contains("__ACCEPTANCE_CRITERIA_JSON__"));
    }

    #[test]
    fn prompt_with_quotes_and_newlines_survives() {
        let prompt = "Say \"hi\"\nand 'bye'\n\\backslash";
        let py = generate_task_py(&TaskGenParams {
            compose_file: "/x".into(),
            contract_prompt: prompt.into(),
            acceptance_criteria: "criteria".into(),
            port_base: 13100,
            pi_model: "inspect-bridge/inspect".into(),
            workspace_dir: "/workspace".into(),
            sandbox_user: "root".into(),
            run_id: "r".into(),
            settle_grace_seconds: 20.0,
        })
        .unwrap();
        // 字符串 token 注入的是 JSON 字面量（值语义一致），直接赋值即可
        assert!(py.contains("CONTRACT_PROMPT ="));
    }
}
