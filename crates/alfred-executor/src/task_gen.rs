//! Inspect 容器驱动定义生成（Rust 生成 Python 文件，非 eval Task）。

use anyhow::{Context, Result};

/// 内嵌的 executor 容器驱动模板（见 templates/executor_driver.py.tmpl）。
const EXECUTOR_DRIVER_TEMPLATE: &str = include_str!("../templates/executor_driver.py.tmpl");

/// 生成参数（全部经 token 替换注入模板）。
pub struct TaskGenParams {
    /// 沙箱 compose 文件绝对路径（挂载面矩阵，隔离机制）。
    pub compose_file: String,
    /// 契约 prompt（给执行者 pi 的任务描述）。
    pub contract_prompt: String,
    /// 桥代理端口（每容器一桥，容器内 localhost 互不冲突）。
    pub port: u32,
    /// pi 模型（provider/model 形态，如 "inspect-bridge/inspect"）。
    pub pi_model: String,
    /// 宿主侧桥代发模型 id（`inspect/<provider>/<model>`，sandbox_agent_bridge 的
    /// fallback model；pi 请求 "inspect" 时桥解析到本模型）。
    pub bridge_model: String,
    /// 宿主侧模型 max_tokens（桥代发生成配置；eval 路径曾经 `--max-tokens` 传入）。
    pub max_tokens: u32,
    /// 容器内工作区路径（"/workspace"）。
    pub workspace_dir: String,
    /// 容器内执行用户（"root"）。
    pub sandbox_user: String,
    /// 样本 id（run id）。
    pub run_id: String,
    /// settled 后的宽限秒数（进程未在 EOF 退出则 kill）。
    pub settle_grace_seconds: f64,
    /// 驱动总时间上限（秒；anyio.fail_after 包裹整个容器运行）。
    pub time_limit_secs: u32,
    /// 宿主侧 done 记录文件绝对路径。
    pub done_marker: String,
    /// docker compose 项目名基座（Inspect 加 uuid 后缀）。
    pub task_name: String,
}

/// 生成 driver.py 内容。
pub fn generate_task_py(params: &TaskGenParams) -> Result<String> {
    let mut out = EXECUTOR_DRIVER_TEMPLATE.to_string();

    // 所有字符串值经 JSON 编码注入，模板用 json.loads 还原——避免引号/花括号/反斜杠破坏 Python 字面量。
    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__CONTRACT_PROMPT_JSON__", json(&params.contract_prompt)?),
        ("__PORT__", params.port.to_string()),
        ("__PI_MODEL_JSON__", json(&params.pi_model)?),
        ("__BRIDGE_MODEL_JSON__", json(&params.bridge_model)?),
        ("__MAX_TOKENS__", params.max_tokens.to_string()),
        ("__WORKSPACE_DIR_JSON__", json(&params.workspace_dir)?),
        ("__SANDBOX_USER_JSON__", json(&params.sandbox_user)?),
        ("__RUN_ID_JSON__", json(&params.run_id)?),
        (
            "__SETTLE_GRACE_SECONDS__",
            format!("{}", params.settle_grace_seconds),
        ),
        ("__TIME_LIMIT_SECS__", format!("{}", params.time_limit_secs)),
        ("__DONE_MARKER_JSON__", json(&params.done_marker)?),
        ("__TASK_NAME_JSON__", json(&params.task_name)?),
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
        "__PORT__",
        "__PI_MODEL_JSON__",
        "__BRIDGE_MODEL_JSON__",
        "__MAX_TOKENS__",
        "__WORKSPACE_DIR_JSON__",
        "__SANDBOX_USER_JSON__",
        "__RUN_ID_JSON__",
        "__TIME_LIMIT_SECS__",
        "__DONE_MARKER_JSON__",
        "__TASK_NAME_JSON__",
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
