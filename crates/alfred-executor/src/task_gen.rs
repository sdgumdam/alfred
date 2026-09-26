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
    /// 契约声明的工作区子目录（挂载锚用其首个名字做正例；非空由
    /// `validate_executor_sandbox` 保证，见 run.rs `execute_run` 入口）。
    pub workspace_subdirs: Vec<String>,
    /// 桥代理端口（每容器一桥，容器内 localhost 互不冲突）。
    pub port: u32,
    /// pi 模型（provider/model 形态，如 "inspect-bridge/inspect"）。
    pub pi_model: String,
    /// 宿主侧桥代发模型 id（`inspect/<provider>/<model>`，sandbox_agent_bridge 的
    /// fallback model；pi 请求 "inspect" 时桥解析到本模型）。
    pub bridge_model: String,
    /// 宿主侧模型 max_tokens（桥代发生成配置；eval 路径曾经 `--max-tokens` 传入）。
    pub max_tokens: u32,
    /// 宿主侧模型声明上下文窗口（config.yml `contextWindow`；None = 未声明
    /// ——env 覆盖/mockllm 路径不猜容量）。渲染为 driver.py 的 CONTEXT_WINDOW
    /// （Python 字面量：整数或 None），声明时写进 pi models.json。
    pub context_window: Option<u32>,
    /// 容器内工作区路径（"/workspace"）。
    pub workspace_dir: String,
    /// 容器内执行用户（"root"）。
    pub sandbox_user: String,
    /// AGT 扩展路径（"/tmp/.agt/agt-policy.ts"）；空串 = 不加载。
    pub agt_ext: String,
    /// AGT 策略文件容器内路径（"/tmp/.agt/policy.json"）；与 `agt_ext` 同空。
    pub agt_policy_path: String,
    /// AGT 审计文件容器内路径（"/tmp/.agt/audit/audit.jsonl"）；与 `agt_ext` 同空。
    pub agt_audit_path: String,
    /// 只读参考卷容器内挂载点列表（9/3 方案②；注入 driver env
    /// `AGT_REF_VOLUMES`——AGT 扩展边界判定豁免面）。空 = 无参考卷。
    pub ref_volume_dirs: Vec<String>,
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
    /// 宿主侧原生 session 保留目录（`<run>/sessions`，绝对路径；compose 挂载
    /// 源）。driver 据此把 pi RPC 返回的容器内 sessionFile 精确映射到宿主
    /// 保留位置（见 executor_driver.py.tmpl `_session_record`）。
    pub sessions_dir_host: String,
    /// G1 native_inspect 外层实验绑定（非秘密引用的 JSON 对象原文；空串 =
    /// 未绑定）。注入 driver 后随 done 记录回传——外层按 run_ref 关联本
    /// 执行，native_exec_id 保持 exec 目录身份，两者不混用。
    pub evidence_binding: String,
    /// 任务环境 compose 的 `${SAMPLE_METADATA_*}` 插值键值（G1 真实环境：
    /// driver 的 sample_init 用它们解析 compose 引用，与原任务装载同一
    /// 解析链）。空 = compose 无插值引用（sample_init 行为与既有一致）。
    pub sandbox_metadata: std::collections::BTreeMap<String, String>,
}

/// 执行者驱动 prompt 的工作区挂载锚（src 嵌套歧义治本）。
///
/// 挂载事实：workspace_subdirs[0] 被直接挂为执行者 /workspace 根——契约里该
/// 子目录名指的就是 /workspace 本身，不是 /workspace 下的字面子目录。执行者
/// 曾把「在 src 下创建 f.txt」写成 /workspace/src/f.txt（多嵌套一层，宿主落
/// ws/src/src/），故驱动侧强制注入翻译规则 + 正例 + 反例，锚定挂载语义。
fn mount_anchor_prompt(workspace_subdirs: &[String]) -> String {
    let first = workspace_subdirs
        .first()
        .map(String::as_str)
        .unwrap_or("（未声明）");
    format!(
        r#"【路径语义：工作区挂载翻译（必读，执行任何文件操作前先理解）】
你的工作区根 /workspace 就是「{first}」目录本身：容器把工作区声明的首个子目录「{first}」直接挂载为你的 /workspace 根——你的 /workspace 下不存在名为「{first}」的嵌套子目录。
- 契约（上面的任务描述）里「在 {first} 下创建/修改文件」的落点就是 /workspace 根下本身。
- 正例：契约说「在 {first} 下创建 hello.txt」⇒ 你应写 /workspace/hello.txt。
- 反例（错误，禁止）：写 /workspace/{first}/hello.txt——这会凭空多嵌套一层，产物位置错误。
- 若声明了其余子目录，它们按名字挂为 /workspace/<子目录名>，契约里按字面路径使用。
- 相对路径一律相对 /workspace 根解析；不要在根下再造与「{first}」同名的目录层。"#
    )
}

/// 生成 driver.py 内容。
pub fn generate_task_py(params: &TaskGenParams) -> Result<String> {
    let mut out = EXECUTOR_DRIVER_TEMPLATE.to_string();

    // 所有字符串值经 JSON 编码注入，模板用 json.loads 还原——避免引号/花括号/反斜杠破坏 Python 字面量。
    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__CONTRACT_PROMPT_JSON__", json(&params.contract_prompt)?),
        (
            "__MOUNT_ANCHOR_JSON__",
            json(&mount_anchor_prompt(&params.workspace_subdirs))?,
        ),
        ("__PORT__", params.port.to_string()),
        ("__PI_MODEL_JSON__", json(&params.pi_model)?),
        ("__BRIDGE_MODEL_JSON__", json(&params.bridge_model)?),
        ("__MAX_TOKENS__", params.max_tokens.to_string()),
        (
            "__CONTEXT_WINDOW__",
            params
                .context_window
                .map(|n| n.to_string())
                .unwrap_or_else(|| "None".into()),
        ),
        ("__WORKSPACE_DIR_JSON__", json(&params.workspace_dir)?),
        ("__SANDBOX_USER_JSON__", json(&params.sandbox_user)?),
        ("__AGT_EXT_JSON__", json(&params.agt_ext)?),
        ("__AGT_POLICY_PATH_JSON__", json(&params.agt_policy_path)?),
        ("__AGT_AUDIT_PATH_JSON__", json(&params.agt_audit_path)?),
        (
            "__REF_VOLUMES_JSON__",
            json(&params.ref_volume_dirs.join(":"))?,
        ),
        ("__RUN_ID_JSON__", json(&params.run_id)?),
        (
            "__SETTLE_GRACE_SECONDS__",
            format!("{}", params.settle_grace_seconds),
        ),
        ("__TIME_LIMIT_SECS__", format!("{}", params.time_limit_secs)),
        ("__SESSIONS_DIR_HOST_JSON__", json(&params.sessions_dir_host)?),
        (
            "__EVIDENCE_BINDING_JSON__",
            json(if params.evidence_binding.trim().is_empty() {
                "null"
            } else {
                &params.evidence_binding
            })?,
        ),
        ("__DONE_MARKER_JSON__", json(&params.done_marker)?),
        ("__TASK_NAME_JSON__", json(&params.task_name)?),
        // 任务环境插值键（JSON 对象字面量，driver 直接作 Python dict 用）。
        (
            "__SANDBOX_METADATA_JSON__",
            serde_json::to_string(&params.sandbox_metadata)
                .context("json-encode sandbox metadata")?,
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
        "__MOUNT_ANCHOR_JSON__",
        "__PORT__",
        "__PI_MODEL_JSON__",
        "__BRIDGE_MODEL_JSON__",
        "__MAX_TOKENS__",
        "__CONTEXT_WINDOW__",
        "__WORKSPACE_DIR_JSON__",
        "__AGT_AUDIT_PATH_JSON__",
        "__REF_VOLUMES_JSON__",
        "__AGT_EXT_JSON__",
        "__AGT_POLICY_PATH_JSON__",
        "__AGT_AUDIT_PATH_JSON__",
        "__RUN_ID_JSON__",
        "__TIME_LIMIT_SECS__",
        "__DONE_MARKER_JSON__",
        "__TASK_NAME_JSON__",
        "__SESSIONS_DIR_HOST_JSON__",
        "__EVIDENCE_BINDING_JSON__",
        "__SANDBOX_METADATA_JSON__",
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
