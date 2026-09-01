//!
//! R6c：reviewer 容器驱动生成（`templates/reviewer_driver.py.tmpl`）——计划审查/
//! 执行审查都走同一容器 pi 驱动模板，差异只在 MODE + SYSTEM/DRIVER prompt。
//!
//! 三容器 Inspect 统一管（属主 08-27）：驱动脚本是非 eval 的 Inspect 容器管理
//! 驱动（DockerSandboxEnvironment + sandbox_agent_bridge + exec_remote），不再
//! 生成 `inspect eval` 评测 Task（旧 `plan_review.py.tmpl` eval 路径已移除）。

use anyhow::{Context, Result};

/// 内嵌的 reviewer 容器驱动模板（见 templates/reviewer_driver.py.tmpl）。
const REVIEWER_DRIVER_TEMPLATE: &str = include_str!("../templates/reviewer_driver.py.tmpl");

/// reviewer 容器驱动生成参数。
pub struct ReviewerTaskGenParams {
    /// 沙箱 compose 文件绝对路径（挂载面矩阵，隔离机制）。
    pub compose_file: String,
    /// "plan_review" | "exec_review"（容器内 pi 的任务模式）。
    pub mode: String,
    /// 容器侧 system prompt（计划审查忠实度 / 执行审查判产物 vs 验收标准）。
    pub system_prompt: String,
    /// 容器侧 driver prompt（读 /inputs + /workspace → 写 /outputs/verdict.json）。
    pub driver_prompt: String,
    /// 容器内 verdict 产出文件绝对路径（"/outputs/verdict.json"）。
    pub output_file: String,
    /// AGT 扩展路径（"/tmp/.agt/agt-policy.ts"）；空串 = 不加载。
    pub agt_ext: String,
    /// AGT 策略文件容器内路径（"/tmp/.agt/policy.json"）；与 `agt_ext` 同空。
    pub agt_policy_path: String,
    /// AGT 审计文件容器内路径（"/tmp/.agt/audit/audit.jsonl"）；与 `agt_ext` 同空。
    pub agt_audit_path: String,
    /// 桥代理端口（每容器一桥，容器内 localhost 互不冲突）。
    pub port: u32,
    /// pi 模型（provider/model 形态，如 "inspect-bridge/inspect"）。
    pub pi_model: String,
    /// 宿主侧桥代发模型 id（`inspect/<provider>/<model>`）。
    pub bridge_model: String,
    /// 宿主侧模型 max_tokens（桥代发生成配置）。
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

/// 生成 reviewer 容器 driver.py 内容（token 替换，机制与 planner 一致）。
pub fn generate_reviewer_task_py(params: &ReviewerTaskGenParams) -> Result<String> {
    let mut out = REVIEWER_DRIVER_TEMPLATE.to_string();

    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__MODE_JSON__", json(&params.mode)?),
        ("__SYSTEM_PROMPT_JSON__", json(&params.system_prompt)?),
        ("__DRIVER_PROMPT_JSON__", json(&params.driver_prompt)?),
        ("__OUTPUT_FILE_JSON__", json(&params.output_file)?),
        ("__AGT_EXT_JSON__", json(&params.agt_ext)?),
        ("__AGT_POLICY_PATH_JSON__", json(&params.agt_policy_path)?),
        ("__AGT_AUDIT_PATH_JSON__", json(&params.agt_audit_path)?),
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
            anyhow::bail!("reviewer_driver template missing token {token}");
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
        "__AGT_EXT_JSON__",
        "__AGT_POLICY_PATH_JSON__",
        "__AGT_AUDIT_PATH_JSON__",
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
            anyhow::bail!("reviewer_driver token replacement incomplete: {token}");
        }
    }

    Ok(out)
}

fn json(s: &str) -> Result<String> {
    serde_json::to_string(s).context("json-encode template value")
}
