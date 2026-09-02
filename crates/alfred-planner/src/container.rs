//! planner 容器驱动（R6b：宿主直调 → 容器 Agent）。
//!
//! 依据：`.plans/对齐方案-三容器Agent化.md` v2（M1 已批：桥模式——容器断网 +
//! 宿主代发 LLM，与 executor 同机制）§二.3 planner 容器化改动清单 + R6a 挂载
//! 矩阵 §1.1 planner 行（`docker/planner.compose.yaml.tmpl` 落码）。
//!
//! 流程：
//!   1. 输入落盘：会话文档投影 / owner 消息 / request（+ maintain 触发事件）写到
//!      `<run_dir>/planner/inputs/`（R6a 模板约定：`/inputs/request.json`、
//!      `/inputs/session.json`、`/inputs/owner_message.txt`、`/inputs/contract.json`）。
//!   2. 渲染 `planner.compose.yaml.tmpl`（占位符 → canonicalize 后绝对路径，
//!      实施计划 E1/E3）→ 生成 planner driver.py（`templates/planner_driver.py.tmpl`，
//!      token 注入，非 eval Task）→ spawn `python3 driver.py`（Inspect 容器管理
//!      接口：DockerSandboxEnvironment + sandbox_agent_bridge + exec_remote）→
//!      轮询 done → 容器内 pi 读输入、按 system prompt（照搬 converse.rs /
//!      maintain.rs 的 schema prompt）产建图指令/会话文档 JSON → 写 `/outputs/` 挂载。
//!   3. 宿主读 `/outputs/<file>` 得原始输出文本（调用方解析/校验/落 llm-calls）。
//!
//! 桥模式选型（交付记录）：**复用 executor 的 sandbox_agent_bridge**（Inspect
//! 容器管理接口），不独立起桥。理由：独立桥需重实现 inspect `sandbox_service` 的
//! RPC 通道（docker exec stdin/stdout 透传 + 容器内 model_proxy），数百行脆弱
//! 协议代码；executor 桥 R0 已验证（network-none 容器内经桥调通 zhipu），三角色
//! 统一断网 + 宿主代发。三容器 Inspect 统一管（属主 08-27）：容器管理走 Inspect
//! 容器管理接口，不走 `inspect eval` 评测包装。

use std::path::{Path, PathBuf};

use alfred_executor::agt::{assets, prepare_agt_work, resolve_agt_source, AgtSource};
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_executor::compose_gen::canonicalize_workspace;
use alfred_executor::config::ExecutorModel;
use alfred_executor::driver::{
    absolutize_cwd, poll_container_driver, spawn_container_driver, DriverOutcome,
};
use anyhow::{bail, Context, Result};

use crate::maintain::MaintainTrigger;
use crate::task_gen::{generate_planner_task_py, PlannerTaskGenParams};

/// planner 容器驱动的工作目录名（`<run_dir>/planner/`）。
pub const PLANNER_WORK_DIR: &str = "planner";
/// 输入落盘目录（`<run_dir>/planner/inputs/`，R6a 模板 `/inputs` 挂载源）。
pub const INPUTS_DIR: &str = "inputs";
/// 输出挂载目录（`<run_dir>/planner/outputs/`，R6a 模板 `/outputs` 挂载源）。
pub const OUTPUTS_DIR: &str = "outputs";
/// converse 产出文件名（容器内写 `/outputs/instructions.json`）。
pub const CONVERSE_OUTPUT_FILE: &str = "/outputs/instructions.json";
/// converse 答复产出文件名（§2.4 两分支答复侧，容器内写 `/outputs/reply.txt`）。
pub const CONVERSE_REPLY_FILE: &str = "/outputs/reply.txt";
/// maintain 产出文件名（容器内写 `/outputs/session.json`）。
pub const MAINTAIN_OUTPUT_FILE: &str = "/outputs/session.json";

/// planner 容器选项（R6b；编排器从 `GovernanceOptions` 派生，见 [`from_governance`]）。
#[derive(Debug, Clone)]
pub struct PlannerContainerOptions {
    /// 治理 run 目录（llm-calls/ 与 ws/ 在此；planner 工作区 `<run_dir>/planner/`）。
    pub run_dir: PathBuf,
    /// 沙箱镜像。
    pub image: String,
    /// 桥代理端口基数（每样本自增）。
    pub port_base: u32,
    /// planner 容器驱动单样本时间上限（秒）。
    pub time_limit_secs: u32,
    /// settled 后宽限（秒）。
    pub settle_grace_seconds: f64,
    /// 兼容保留（inspect ctl 已随去 eval 退役，当前无观测面轮询）。
    pub ctl_enabled: bool,
    /// AGT 拦写层源（默认内置策略；`ALFRED_AGT_DIR` 显式目录沿用覆盖；
    /// `ALFRED_AGT_DISABLE=1` 关）。Off = 不挂 AGT、不加载扩展。
    pub agt: AgtSource,
}

impl Default for PlannerContainerOptions {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::new(),
            image: "alfred-executor:latest".to_string(),
            port_base: 13100,
            time_limit_secs: 600,
            settle_grace_seconds: 20.0,
            ctl_enabled: true,
            agt: AgtSource::Builtin,
        }
    }
}

impl PlannerContainerOptions {
    /// 从治理环运行选项派生容器选项（`run_dir` 由调用方填）。
    pub fn from_governance(run_dir: PathBuf, opts: &alfred_core::governance::GovernanceOptions) -> Self {
        Self {
            run_dir,
            image: opts.image.clone(),
            port_base: opts.port_base,
            time_limit_secs: opts.planner_time_limit_secs,
            settle_grace_seconds: opts.settle_grace_seconds,
            ctl_enabled: opts.ctl_enabled,
            agt: resolve_agt_source(),
        }
    }
}


/// planner 容器运行结果（宿主侧读取）。
#[derive(Debug, Clone)]
pub struct ContainerRunOutput {
    /// 容器产出的原始文本（converse：建图指令 JSON 数组 或 属主答复；maintain：会话文档 JSON）。
    pub output_text: String,
    /// 实际产出的输出文件（容器内路径；converse：/outputs/instructions.json 或
    /// /outputs/reply.txt；maintain：/outputs/session.json）。
    pub produced_file: String,
    /// 容器驱动状态（"success" / "error" / "timed_out"）。字段名沿用旧名
    /// `eval_status`（state.json 兼容；现承载 driver 状态，非 eval 状态）。
    pub eval_status: String,
    /// 驱动证据 location（driver.done.json，审计证据）。字段名沿用旧名
    /// `eval_location`（state.json 兼容；现承载 driver done 路径，非 eval 位置）。
    pub eval_location: Option<String>,
}

/// converse 容器驱动：会话文档投影 + 属主消息 + request → §2.4 两分支产出
/// （建图指令 JSON 或 属主答复；produced_file 区分）。
///
/// `append_system_prompt`：codux 注入的项目上下文（P2-1），经
/// [`crate::converse::converse_system_prompt`] 追加到 planner pi 的 system prompt；
/// 空串 = 不追加（基础建图 schema）。
pub fn run_converse_in_container(
    opts: &PlannerContainerOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
    append_system_prompt: &str,
) -> Result<ContainerRunOutput> {
    let inputs = converse_inputs(request, doc, owner_message)?;
    let system_prompt = crate::converse::converse_system_prompt(append_system_prompt);
    run_planner_container(
        opts,
        model,
        "converse",
        &system_prompt,
        CONVERSE_DRIVER_PROMPT,
        &[CONVERSE_OUTPUT_FILE, CONVERSE_REPLY_FILE],
        inputs,
    )
}

/// maintain 容器驱动：会话文档 + 触发事件 → 更新后会话文档 JSON 文本。
pub fn run_maintain_in_container(
    opts: &PlannerContainerOptions,
    model: &ExecutorModel,
    doc: &SessionDoc,
    trigger: &MaintainTrigger,
) -> Result<ContainerRunOutput> {
    let inputs = maintain_inputs(doc, trigger)?;
    run_planner_container(
        opts,
        model,
        "maintain",
        crate::maintain::MAINTAIN_SYSTEM_PROMPT,
        MAINTAIN_DRIVER_PROMPT,
        &[MAINTAIN_OUTPUT_FILE],
        inputs,
    )
}

/// converse 容器侧 driver prompt：读 /inputs → 按 SYSTEM_PROMPT 规则 → 写 /outputs。
pub const CONVERSE_DRIVER_PROMPT: &str = r#"你的任务：按两分支规则决定产出——建图指令序列 或 给属主的答复，写为文件，而不是聊天回复。

请按顺序读取输入文件：
- /inputs/request.json —— 属主请求（JSON 对象，含 id/title/description/acceptance_criteria）
- /inputs/session.json —— 会话文档（记忆，JSON 对象：key_file_paths / key_conclusions / owner_feedback）
- /inputs/owner_message.txt —— 属主本轮消息（文本）
- /inputs/contract.json —— 你先前落定的计划契约（JSON 对象：prompt + acceptance_criteria；重规划轮回看用，首轮为空对象 {}）

按上面 SYSTEM_PROMPT 的规则二选一（只产其中一种）：
- 若产出建图指令序列：把 JSON 数组写入 /outputs/instructions.json。
- 若产出给属主的答复：把答复文本写入 /outputs/reply.txt。
只能写其中一个文件；不要写 /workspace 下的任何文件（工作区只读，写了会被拒绝）。
写完即结束。"#;

/// maintain 容器侧 driver prompt：读 /inputs → 按 SYSTEM_PROMPT 规则 → 写 /outputs。
pub const MAINTAIN_DRIVER_PROMPT: &str = r#"你的任务：把更新后的会话文档 JSON 写入文件，而不是聊天回复。

请按顺序读取输入文件：
- /inputs/session.json —— 当前会话文档（JSON 对象：key_file_paths / key_conclusions / review_summary）
- /inputs/trigger.json —— 新信息（JSON 对象，含 kind 与触发内容）

按上面 SYSTEM_PROMPT 的规则，把更新后的完整三字段 JSON 写入 /outputs/session.json。
只写这一个文件；不要写 /workspace 下的任何文件（工作区只读，写了会被拒绝）。
写完即结束。"#;

/// converse 输入落盘（R6a 模板约定）：request / 会话文档投影 / 属主消息。
fn converse_inputs(
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
) -> Result<Vec<(String, String)>> {
    let projection = crate::converse::project_session_doc(doc);
    Ok(vec![
        (
            "request.json".to_string(),
            serde_json::to_string_pretty(request).context("serialize OwnerRequest")?,
        ),
        (
            "session.json".to_string(),
            serde_json::to_string_pretty(&projection).context("serialize projected SessionDoc")?,
        ),
        ("owner_message.txt".to_string(), owner_message.to_string()),
    ])
}

/// maintain 输入落盘：当前会话文档（全字段，maintain 更新真源）+ 触发事件。
fn maintain_inputs(doc: &SessionDoc, trigger: &MaintainTrigger) -> Result<Vec<(String, String)>> {
    let trigger_json = match trigger {
        MaintainTrigger::PlanReviewed { verdict, plan } => serde_json::json!({
            "kind": "plan_reviewed",
            "verdict": { "pass": verdict.pass, "reason": verdict.reason },
            "plan": plan,
        }),
        MaintainTrigger::OwnerMessage { message } => serde_json::json!({
            "kind": "owner_message",
            "message": message,
        }),
    };
    Ok(vec![
        (
            "session.json".to_string(),
            serde_json::to_string_pretty(doc).context("serialize SessionDoc")?,
        ),
        (
            "trigger.json".to_string(),
            serde_json::to_string_pretty(&trigger_json).context("serialize MaintainTrigger")?,
        ),
    ])
}

///
/// `output_files`：容器内候选产出文件（converse 两分支 = [instructions, reply]；
/// maintain = [session]）。驱动脚本强制恰好一个被写（多/零都报错）；宿主按候选集
/// 探测产出（`ContainerRunOutput::produced_file` 区分分支）。
#[allow(clippy::too_many_arguments)]
fn run_planner_container(
    opts: &PlannerContainerOptions,
    model: &ExecutorModel,
    mode: &str,
    system_prompt: &str,
    driver_prompt: &str,
    output_files: &[&str],
    inputs: Vec<(String, String)>,
) -> Result<ContainerRunOutput> {
    let run_dir = &opts.run_dir;
    let work = run_dir.join(PLANNER_WORK_DIR);
    let inputs_dir = work.join(INPUTS_DIR);
    let outputs_dir = work.join(OUTPUTS_DIR);
    // ws 持久目录（矩阵 §1.1 planner 行：ws 全量 ro 挂载源；R6b 先建空目录，
    // 持久 ws 语义见对齐方案 §二.7 后续工作）。
    let ws_dir = run_dir.join("ws");

    std::fs::create_dir_all(&inputs_dir)
        .with_context(|| format!("create planner inputs dir {}", inputs_dir.display()))?;
    std::fs::create_dir_all(&outputs_dir)
        .with_context(|| format!("create planner outputs dir {}", outputs_dir.display()))?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("create planner ws dir {}", ws_dir.display()))?;

    for (name, content) in &inputs {
        let path = inputs_dir.join(name);
        std::fs::write(&path, content)
            .with_context(|| format!("write planner input {}", path.display()))?;
    }

    // 契约挂载（矩阵 §1.1 第 7 行：planner 挂自己写的契约 ro 回看）。首轮规划无契约
    // → 写空占位，保证 bind mount 源存在（E1：docker 对不存在的宿主文件静默建目录，
    // 挂载会错）；真实内容由治理环 planning_step 在 dagspec 落定时写入
    // （governance.rs write_run_contract → DagSpec::contract_json 投影）——重规划轮
    // 起容器读到的就是 planner 上一轮自己写的契约。
    let contract_path = run_dir.join("contract.json");
    if !contract_path.exists() {
        std::fs::write(&contract_path, "{}")
            .with_context(|| format!("write planner contract placeholder {}", contract_path.display()))?;
    }

    // AGT 拦写层（默认启用）：落策略 + 扩展到 `<work>/agt/`（策略 ro），审计
    // 子目录 rw（审计 JSONL 落宿主）。未设 env = 内置默认策略（planner 用
    // `docker/agt/planner/policy.json`）；`ALFRED_AGT_DIR` 显式目录沿用覆盖；
    // `ALFRED_AGT_DISABLE=1` 不挂。
    let agt_work = prepare_agt_work(&work, &opts.agt, assets::PLANNER_POLICY)?;

    // E1/E3：挂载路径必须 canonicalize 成绝对路径（相对路径被 docker 静默变
    // named volume；colima 只共享 ~）。
    let ws_abs = canonicalize_workspace(&ws_dir)?;
    let inputs_abs = inputs_dir
        .canonicalize()
        .with_context(|| format!("canonicalize planner inputs {}", inputs_dir.display()))?;
    let outputs_abs = outputs_dir
        .canonicalize()
        .with_context(|| format!("canonicalize planner outputs {}", outputs_dir.display()))?;
    let contract_abs = contract_path
        .canonicalize()
        .with_context(|| format!("canonicalize planner contract {}", contract_path.display()))?;

    let compose = render_planner_compose(
        opts,
        &ws_abs,
        &inputs_abs,
        &contract_abs,
        &outputs_abs,
        mode == "maintain",
        agt_work.as_deref(),
    )?;
    let compose_path = work.join("compose.yaml");
    std::fs::write(&compose_path, compose)
        .with_context(|| format!("write planner compose {}", compose_path.display()))?;
    let compose_abs = compose_path
        .canonicalize()
        .with_context(|| format!("canonicalize planner compose {}", compose_path.display()))?;

    let run_id = run_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("run")
        .to_string();
    let (agt_ext, agt_policy_path, agt_audit_path) = match &agt_work {
        Some(_) => (
            "/tmp/.agt/agt-policy.ts".to_string(),
            "/tmp/.agt/policy.json".to_string(),
            "/tmp/.agt/audit/audit.jsonl".to_string(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    // 嵌入 driver.py 的 done 路径必须绝对：驱动进程 cwd 切到 work_dir 后，相对
    // 路径被二次解析（双拼）——与 spawn 层 absolutize_cwd 同源约束。
    let done_marker = absolutize_cwd(&work.join("driver.done.json"));
    let driver_py_path = work.join("driver.py");
    let py = generate_planner_task_py(&PlannerTaskGenParams {
        compose_file: compose_abs.to_string_lossy().into_owned(),
        mode: mode.to_string(),
        system_prompt: system_prompt.to_string(),
        driver_prompt: driver_prompt.to_string(),
        output_file: output_files[0].to_string(),
        output_file_alt: output_files.get(1).copied().unwrap_or("").to_string(),
        agt_ext,
        agt_policy_path,
        agt_audit_path,
        port: opts.port_base,
        pi_model: "inspect-bridge/inspect".to_string(),
        bridge_model: format!("inspect/{}", model.inspect_model_id()),
        max_tokens: model.max_tokens,
        workspace_dir: "/workspace".to_string(),
        sandbox_user: "root".to_string(),
        run_id,
        settle_grace_seconds: opts.settle_grace_seconds,
        time_limit_secs: opts.time_limit_secs,
        done_marker: done_marker.to_string_lossy().into_owned(),
        task_name: "alfred-planner".to_string(),
    })?;
    std::fs::write(&driver_py_path, py)
        .with_context(|| format!("write planner driver {}", driver_py_path.display()))?;

    // spawn 宿主侧容器驱动（非 eval；桥代发 = sandbox_agent_bridge，宿主侧 Inspect
    // 模型 = planner provider——桥服务调用日志即审计源，见 llm.rs）。
    let launch = spawn_container_driver(&driver_py_path, model, &work)?;

    let poll_timeout = opts.time_limit_secs as u64 + 600;
    let outcome = match poll_container_driver(&launch, poll_timeout)? {
        DriverOutcome::Done(done) => done,
        DriverOutcome::TimedOut => {
            bail!(
                "planner container driver timed out after {}s (no done record in {})",
                poll_timeout,
                launch.done_marker.display()
            )
        }
        DriverOutcome::Crashed => {
            bail!(
                "planner container driver crashed (done marker: {})",
                launch.done_marker.display()
            )
        }
    };

    if outcome.status != "success" {
        bail!(
            "planner container driver finished with status '{}' (error={:?})",
            outcome.status,
            outcome.error
        );
    }

    // 读产出：容器写 /outputs/<file>（bind mount 即时可见）。converse 两分支时
    // driver.py 已强制恰好一个候选文件被写；宿主按候选集探测产出（多/零都显式报错）。
    let mut produced: Vec<(&str, String)> = Vec::new();
    for f in output_files {
        let host = outputs_dir.join(
            f.trim_start_matches("/outputs/")
                .trim_start_matches('/'),
        );
        if let Ok(text) = std::fs::read_to_string(&host) {
            if !text.trim().is_empty() {
                produced.push((f, text));
            }
        }
    }
    let (produced_file, output_text) = match produced.as_slice() {
        [(f, text)] => ((*f).to_string(), text.clone()),
        [] => bail!(
            "planner container produced none of {} (status={}, error={:?})",
            output_files.join(", "),
            outcome.status,
            outcome.error
        ),
        _ => bail!(
            "planner container produced multiple outputs ({}): 两分支只能二选一",
            produced.iter().map(|(f, _)| *f).collect::<Vec<_>>().join(", ")
        ),
    };
    if output_text.trim().is_empty() {
        bail!("planner container produced empty output in {produced_file}");
    }

    Ok(ContainerRunOutput {
        output_text,
        produced_file,
        eval_status: outcome.status,
        eval_location: Some(done_marker.to_string_lossy().into_owned()),
    })
}

/// 渲染 planner compose：R6a 模板占位符 → canonicalize 后绝对路径。
///
/// AGT 目录为 None 时移除 `/tmp/.agt` 挂载行（最小环境不挂拦写层）。
/// AGT 为 Some 时追加审计子目录 rw 挂载（`/tmp/.agt/audit` rw——agent 可写审计
/// 但不可改策略，R6a 拆分挂载语义，与 reviewer 容器一致）。
fn render_planner_compose(
    opts: &PlannerContainerOptions,
    ws_abs: &Path,
    inputs_abs: &Path,
    contract_abs: &Path,
    outputs_abs: &Path,
    mount_trigger: bool,
    agt_work: Option<&Path>,
) -> Result<String> {
    // E1 修复：converse 不产 trigger.json，不能无条件挂载——docker 对不存在的
    // 宿主文件静默建目录（trigger.json 变目录），maintain 后写同名文件撞目录
    // （os error 21）。故仅 maintain 挂载，converse 填注释行。
    let trigger_mount = if mount_trigger {
        format!("- {}:/inputs/trigger.json:ro", inputs_abs.join("trigger.json").display())
    } else {
        "# (converse 模式：不挂载 trigger.json，避免 docker 静默建目录)".to_string()
    };
    let mut out = PLANNER_COMPOSE_TMPL
        .replace("{ws}", &ws_abs.display().to_string())
        .replace("{request_path}", &inputs_abs.join("request.json").display().to_string())
        .replace("{session_path}", &inputs_abs.join("session.json").display().to_string())
        .replace("{contract_path}", &contract_abs.display().to_string())
        .replace("{outputs_dir}", &outputs_abs.display().to_string())
        // P1 修复：属主本轮消息挂载（converse 对话面输入 /inputs/owner_message.txt；
        // 缺此挂载容器读不到属主消息，多轮对话容器模式失效）。
        .replace("{owner_message_path}", &inputs_abs.join("owner_message.txt").display().to_string())
        // P2 修复：maintain 触发事件挂载（/inputs/trigger.json；缺此挂载容器读不到
        // MaintainTrigger，真实模式 PlanReviewed/OwnerMessage 两触发时机失效）。
        .replace("{trigger_mount}", &trigger_mount)
        .replace(
            "image: \"alfred-executor:latest\"",
            &format!("image: \"{}\"", opts.image),
        );

    match agt_work {
        Some(agt) => {
            let agt_abs = agt
                .canonicalize()
                .with_context(|| format!("canonicalize planner agt dir {}", agt.display()))?;
            out = out.replace("{agt_dir}", &agt_abs.display().to_string());
            // 审计子目录 rw：追加到 volumes 列表（策略目录 ro + 审计 rw 拆开挂载）。
            out.push_str(&format!(
                "\n    - {}/audit:/tmp/.agt/audit:rw",
                agt_abs.display()
            ));
        }
        None => {
            // 移除 AGT 卷行（占位符先清卷行再清注释里的占位符）
            out = out.replace("- {agt_dir}:/tmp/.agt:ro", "");
            out = out.replace("{agt_dir}", "none");
        }
    }

    Ok(out)
}


/// 内嵌 planner compose 模板（R6a 落码，唯一真源）。
const PLANNER_COMPOSE_TMPL: &str = include_str!("../../../docker/planner.compose.yaml.tmpl");
