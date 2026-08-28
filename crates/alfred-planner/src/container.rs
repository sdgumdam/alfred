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
//!      实施计划 E1/E3）→ 生成 planner task.py（`templates/planner_task.py.tmpl`，
//!      token 注入）→ spawn `inspect eval --detach`（复用 executor 驱动）→ 轮询
//!      done → 容器内 pi 读输入、按 system prompt（照搬 converse.rs / maintain.rs
//!      的 schema prompt）产建图指令/会话文档 JSON → 写 `/outputs/` 挂载。
//!   3. 宿主读 `/outputs/<file>` 得原始输出文本（调用方解析/校验/落 llm-calls）。
//!
//! 桥模式选型（交付记录）：**复用 executor 的 sandbox_agent_bridge**（inspect eval
//! 内），不独立起桥。理由：独立桥需重实现 inspect `sandbox_service` 的 RPC 通道
//! （docker exec stdin/stdout 透传 + 容器内 model_proxy），数百行脆弱协议代码；
//! executor 桥 R0 已验证（network-none 容器内经桥调通 zhipu），三角色统一断网 +
//! 宿主代发。D4 的"直接 docker run 直驱"待属主拍板（见 R6b 交付文档）。

use std::path::{Path, PathBuf};

use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_executor::compose_gen::canonicalize_workspace;
use alfred_executor::config::ExecutorModel;
use alfred_executor::driver::{poll_until_done, spawn_eval, PollOutcome};
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
    /// planner eval 单样本时间上限（秒）。
    pub time_limit_secs: u32,
    /// settled 后宽限（秒）。
    pub settle_grace_seconds: f64,
    /// 是否轮询 `inspect ctl` 观测面。
    pub ctl_enabled: bool,
    /// AGT 策略 + 扩展目录（挂 `/tmp/.agt` ro；含 policy.json + agt-policy.ts）。
    /// None = 不挂 AGT、不加载扩展（测试/最小环境）。
    pub agt_dir: Option<PathBuf>,
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
            agt_dir: None,
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
            agt_dir: resolve_agt_dir(),
        }
    }
}

/// AGT 目录解析：`ALFRED_AGT_DIR`（规划器拦写策略目录）；未设 → None。
pub fn resolve_agt_dir() -> Option<PathBuf> {
    std::env::var("ALFRED_AGT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// planner 容器运行结果（宿主侧读取）。
#[derive(Debug, Clone)]
pub struct ContainerRunOutput {
    /// 容器产出的原始文本（converse：建图指令 JSON 数组；maintain：会话文档 JSON）。
    pub output_text: String,
    /// eval 状态（"success" / "error"）。
    pub eval_status: String,
    /// eval 日志 location（evals/ 下的 .eval 路径，审计证据）。
    pub eval_location: Option<String>,
}

/// converse 容器驱动：会话文档投影 + 属主消息 + request → 建图指令 JSON 文本。
pub fn run_converse_in_container(
    opts: &PlannerContainerOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
) -> Result<ContainerRunOutput> {
    let inputs = converse_inputs(request, doc, owner_message)?;
    run_planner_container(
        opts,
        model,
        "converse",
        crate::converse::CONVERSE_SYSTEM_PROMPT,
        CONVERSE_DRIVER_PROMPT,
        CONVERSE_OUTPUT_FILE,
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
        MAINTAIN_OUTPUT_FILE,
        inputs,
    )
}

/// converse 容器侧 driver prompt：读 /inputs → 按 SYSTEM_PROMPT 规则 → 写 /outputs。
pub const CONVERSE_DRIVER_PROMPT: &str = r#"你的任务：把建图指令序列产出为文件，而不是聊天回复。

请按顺序读取输入文件：
- /inputs/request.json —— 属主请求（JSON 对象，含 id/title/description/acceptance_criteria）
- /inputs/session.json —— 会话文档（记忆，JSON 对象：key_file_paths / key_conclusions / owner_feedback）
- /inputs/owner_message.txt —— 属主本轮消息（文本）

按上面 SYSTEM_PROMPT 的规则，把建图指令序列（JSON 数组）写入 /outputs/instructions.json。
只写这一个文件；不要写 /workspace 下的任何文件（工作区只读，写了会被拒绝）。
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

/// planner 容器驱动公共流程。
///
/// 失败路径全部显式 `bail!`（不悄悄放行）：eval 超时/crash/status error 与
/// 容器未产出输出文件都算失败，调用方（converse/maintain）据此升级属主。
#[allow(clippy::too_many_arguments)]
fn run_planner_container(
    opts: &PlannerContainerOptions,
    model: &ExecutorModel,
    mode: &str,
    system_prompt: &str,
    driver_prompt: &str,
    output_file: &str,
    inputs: Vec<(String, String)>,
) -> Result<ContainerRunOutput> {
    let run_dir = &opts.run_dir;
    let work = run_dir.join(PLANNER_WORK_DIR);
    let inputs_dir = work.join(INPUTS_DIR);
    let outputs_dir = work.join(OUTPUTS_DIR);
    let evals_dir = work.join("evals");
    // ws 持久目录（矩阵 §1.1 planner 行：ws 全量 ro 挂载源；R6b 先建空目录，
    // 持久 ws 语义见对齐方案 §二.7 后续工作）。
    let ws_dir = run_dir.join("ws");

    std::fs::create_dir_all(&inputs_dir)
        .with_context(|| format!("create planner inputs dir {}", inputs_dir.display()))?;
    std::fs::create_dir_all(&outputs_dir)
        .with_context(|| format!("create planner outputs dir {}", outputs_dir.display()))?;
    std::fs::create_dir_all(&evals_dir)
        .with_context(|| format!("create planner evals dir {}", evals_dir.display()))?;
    std::fs::create_dir_all(&ws_dir)
        .with_context(|| format!("create planner ws dir {}", ws_dir.display()))?;

    for (name, content) in &inputs {
        let path = inputs_dir.join(name);
        std::fs::write(&path, content)
            .with_context(|| format!("write planner input {}", path.display()))?;
    }

    // 契约挂载（矩阵 §1.1 第 7 行：planner 挂自己写的契约 ro 回看）；首轮规划
    // 无契约 → 写空占位，保证 bind mount 源存在（E1：docker 对不存在的宿主文件
    // 静默建目录，挂载会错）。
    let contract_path = run_dir.join("contract.json");
    if !contract_path.exists() {
        std::fs::write(&contract_path, "{}")
            .with_context(|| format!("write planner contract placeholder {}", contract_path.display()))?;
    }

    // AGT 拦写层：拷贝策略 + 扩展到 `<work>/agt/`（策略 ro），审计子目录 rw
    // （审计 JSONL 落宿主）。None = 不挂 AGT。
    let agt_work = prepare_agt_work(&work, &opts.agt_dir)?;

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
    let task_py_path = work.join("task.py");
    let py = generate_planner_task_py(&PlannerTaskGenParams {
        compose_file: compose_abs.to_string_lossy().into_owned(),
        mode: mode.to_string(),
        system_prompt: system_prompt.to_string(),
        driver_prompt: driver_prompt.to_string(),
        output_file: output_file.to_string(),
        agt_ext,
        agt_policy_path,
        agt_audit_path,
        port_base: opts.port_base,
        pi_model: "inspect-bridge/inspect".to_string(),
        workspace_dir: "/workspace".to_string(),
        sandbox_user: "root".to_string(),
        run_id,
        settle_grace_seconds: opts.settle_grace_seconds,
    })?;
    std::fs::write(&task_py_path, py)
        .with_context(|| format!("write planner task {}", task_py_path.display()))?;

    // spawn `inspect eval --detach`（复用 executor 驱动；桥代发 = sandbox_agent_bridge，
    // 宿主侧 Inspect 模型 = planner provider——桥服务调用日志即审计源，见 llm.rs）。
    let launch = spawn_eval(&task_py_path, model, None, &evals_dir, opts.time_limit_secs)?;

    let poll_timeout = opts.time_limit_secs as u64 + 600;
    let outcome = match poll_until_done(&launch, poll_timeout, opts.ctl_enabled)? {
        PollOutcome::Done(done) => done,
        PollOutcome::TimedOut => {
            bail!(
                "planner container eval timed out after {}s (no done record in {})",
                poll_timeout,
                launch.output_file.display()
            )
        }
        PollOutcome::Crashed => {
            bail!(
                "planner container eval crashed (output: {})",
                launch.output_file.display()
            )
        }
    };

    if outcome.status != "success" {
        bail!(
            "planner container eval finished with status '{}' (location={})",
            outcome.status,
            outcome.location
        );
    }

    // 读产出：容器写 /outputs/<file>（bind mount 即时可见）。
    let output_host = outputs_dir.join(
        output_file
            .trim_start_matches("/outputs/")
            .trim_start_matches('/'),
    );
    let output_text = std::fs::read_to_string(&output_host)
        .with_context(|| format!("read planner output {}", output_host.display()))?;
    if output_text.trim().is_empty() {
        bail!("planner container produced empty output in {}", output_host.display());
    }

    Ok(ContainerRunOutput {
        output_text,
        eval_status: outcome.status,
        eval_location: Some(outcome.location),
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
    agt_work: Option<&Path>,
) -> Result<String> {
    let mut out = PLANNER_COMPOSE_TMPL
        .replace("{ws}", &ws_abs.display().to_string())
        .replace("{request_path}", &inputs_abs.join("request.json").display().to_string())
        .replace("{session_path}", &inputs_abs.join("session.json").display().to_string())
        .replace("{contract_path}", &contract_abs.display().to_string())
        .replace("{outputs_dir}", &outputs_abs.display().to_string())
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

/// AGT 拦写层准备：拷贝源 agt 目录（agt-policy.ts + policy.json）到 `<work>/agt/`，
/// 建审计子目录 `audit/`（rw 挂载源）。None → 不挂 AGT。
fn prepare_agt_work(work: &Path, agt_dir: &Option<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(src) = agt_dir else {
        return Ok(None);
    };
    let dest = work.join("agt");
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("create planner agt dir {}", dest.display()))?;
    std::fs::create_dir_all(dest.join("audit"))
        .with_context(|| format!("create planner agt audit dir {}", dest.join("audit").display()))?;
    std::fs::copy(src.join("agt-policy.ts"), dest.join("agt-policy.ts")).with_context(|| {
        format!(
            "copy agt extension {} -> {}",
            src.join("agt-policy.ts").display(),
            dest.join("agt-policy.ts").display()
        )
    })?;
    std::fs::copy(src.join("policy.json"), dest.join("policy.json")).with_context(|| {
        format!(
            "copy agt policy {} -> {}",
            src.join("policy.json").display(),
            dest.join("policy.json").display()
        )
    })?;
    Ok(Some(dest))
}

/// 内嵌 planner compose 模板（R6a 落码，唯一真源）。
const PLANNER_COMPOSE_TMPL: &str = include_str!("../../../docker/planner.compose.yaml.tmpl");

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::contract::Contract;
    use alfred_core::dagspec::DagSpec;
    use alfred_core::util::now_rfc3339;

    fn home_dir(tag: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap();
        let dir = Path::new(&home).join(format!(".local/state/alfred/test-planner-container-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        std::fs::remove_dir_all(dir).ok();
    }

    fn opts(tag: &str) -> PlannerContainerOptions {
        PlannerContainerOptions {
            run_dir: home_dir(tag),
            ..Default::default()
        }
    }

    #[test]
    fn render_compose_mounts_planner_matrix() {
        // 矩阵 §1.1 planner 行：ws 全量 ro + request/session/contract ro + outputs rw
        // + AGT 策略 ro + 审计子目录 rw
        let o = opts("matrix");
        let work = o.run_dir.join(PLANNER_WORK_DIR);
        let inputs = work.join(INPUTS_DIR);
        let outputs = work.join(OUTPUTS_DIR);
        std::fs::create_dir_all(&inputs).unwrap();
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(inputs.join("request.json"), "{}").unwrap();
        std::fs::write(inputs.join("session.json"), "{}").unwrap();
        let ws = o.run_dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let contract = o.run_dir.join("contract.json");
        std::fs::write(&contract, "{}").unwrap();
        let agt_src = o.run_dir.join(".agt");
        std::fs::create_dir_all(&agt_src).unwrap();
        std::fs::write(agt_src.join("agt-policy.ts"), "// fake ext").unwrap();
        std::fs::write(agt_src.join("policy.json"), "{}").unwrap();

        let o2 = PlannerContainerOptions {
            agt_dir: Some(agt_src.clone()),
            ..o
        };
        let agt_work = prepare_agt_work(&work, &o2.agt_dir).unwrap().unwrap();
        let yaml = render_planner_compose(
            &o2,
            &ws.canonicalize().unwrap(),
            &inputs.canonicalize().unwrap(),
            &contract.canonicalize().unwrap(),
            &outputs.canonicalize().unwrap(),
            Some(&agt_work),
        )
        .unwrap();
        // ws 全量 ro
        assert!(
            yaml.contains(&format!("{}:/workspace:ro", ws.canonicalize().unwrap().display())),
            "ws ro mount missing:\n{yaml}"
        );
        // request/session/contract ro
        for (host, name) in [
            (inputs.canonicalize().unwrap().join("request.json"), "request.json"),
            (inputs.canonicalize().unwrap().join("session.json"), "session.json"),
            (contract.canonicalize().unwrap(), "contract.json"),
        ] {
            assert!(
                yaml.contains(&format!("{}:/inputs/{}:ro", host.display(), name)),
                "{name} ro mount missing:\n{yaml}"
            );
        }
        // outputs rw
        assert!(
            yaml.contains(&format!("{}:/outputs", outputs.canonicalize().unwrap().display())),
            "outputs mount missing:\n{yaml}"
        );
        // AGT 策略 ro + 审计子目录 rw
        assert!(
            yaml.contains(&format!("{}:/tmp/.agt:ro", agt_work.canonicalize().unwrap().display())),
            "agt ro mount missing:\n{yaml}"
        );
        assert!(
            yaml.contains(&format!(
                "{}/audit:/tmp/.agt/audit:rw",
                agt_work.canonicalize().unwrap().display()
            )),
            "agt audit rw mount missing:\n{yaml}"
        );
        assert!(yaml.contains("network_mode: none"), "network none missing:\n{yaml}");
        cleanup(&o2.run_dir);
    }

    #[test]
    fn render_compose_without_agt_drops_mount_line() {
        let o = opts("noagt");
        let work = o.run_dir.join(PLANNER_WORK_DIR);
        let inputs = work.join(INPUTS_DIR);
        let outputs = work.join(OUTPUTS_DIR);
        std::fs::create_dir_all(&inputs).unwrap();
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(inputs.join("request.json"), "{}").unwrap();
        std::fs::write(inputs.join("session.json"), "{}").unwrap();
        let ws = o.run_dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let contract = o.run_dir.join("contract.json");
        std::fs::write(&contract, "{}").unwrap();

        let yaml = render_planner_compose(
            &o,
            &ws.canonicalize().unwrap(),
            &inputs.canonicalize().unwrap(),
            &contract.canonicalize().unwrap(),
            &outputs.canonicalize().unwrap(),
            None,
        )
        .unwrap();
        assert!(
            !yaml.contains("- none:/tmp/.agt:ro"),
            "AGT 卷行应移除（agt_dir=None，不得渲染 stray named volume 'none'）:\n{yaml}"
        );
        cleanup(&o.run_dir);
    }

    #[test]
    fn render_compose_replaces_image() {
        let o = opts("img");
        let work = o.run_dir.join(PLANNER_WORK_DIR);
        let inputs = work.join(INPUTS_DIR);
        let outputs = work.join(OUTPUTS_DIR);
        std::fs::create_dir_all(&inputs).unwrap();
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(inputs.join("request.json"), "{}").unwrap();
        std::fs::write(inputs.join("session.json"), "{}").unwrap();
        let ws = o.run_dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let contract = o.run_dir.join("contract.json");
        std::fs::write(&contract, "{}").unwrap();

        let o2 = PlannerContainerOptions {
            image: "custom-image:v3".into(),
            ..o
        };
        let yaml = render_planner_compose(
            &o2,
            &ws.canonicalize().unwrap(),
            &inputs.canonicalize().unwrap(),
            &contract.canonicalize().unwrap(),
            &outputs.canonicalize().unwrap(),
            None,
        )
        .unwrap();
        assert!(yaml.contains("custom-image:v3"), "image not replaced:\n{yaml}");
        assert!(
            !yaml.contains("alfred-executor:latest"),
            "default image not replaced:\n{yaml}"
        );
        cleanup(&o2.run_dir);
    }

    #[test]
    fn converse_inputs_projects_session_doc() {
        let req = OwnerRequest::new("req-1", "t", "d", "a");
        let mut doc = SessionDoc::new();
        doc.key_conclusions.push("用 Rust".into());
        doc.review_summary
            .push("The plan was rejected by the reviewer: fails to match.".into());
        let inputs = converse_inputs(&req, &doc, "属主：继续").unwrap();
        let files: std::collections::HashMap<String, String> = inputs.into_iter().collect();
        assert!(files.contains_key("request.json"));
        assert!(files.contains_key("owner_message.txt"));
        assert_eq!(files["owner_message.txt"], "属主：继续");
        let session: serde_json::Value = serde_json::from_str(&files["session.json"]).unwrap();
        // 投影：第三段改名 owner_feedback，且不含结构化否决信号
        assert!(
            session.get("owner_feedback").is_some(),
            "projection missing owner_feedback: {session}"
        );
        assert!(session.get("review_summary").is_none(), "raw field leaked: {session}");
        let fb = session["owner_feedback"][0].as_str().unwrap();
        assert_eq!(
            fb,
            crate::disguise::OWNER_FEEDBACK_NEUTRAL_TEMPLATE,
            "contaminated entry not neutralized: {fb}"
        );
    }

    #[test]
    fn maintain_inputs_serialize_triggers() {
        let doc = SessionDoc::new();
        // owner_message 触发
        let inputs = maintain_inputs(
            &doc,
            &MaintainTrigger::OwnerMessage {
                message: "技术选型用 Rust".into(),
            },
        )
        .unwrap();
        let files: std::collections::HashMap<String, String> = inputs.into_iter().collect();
        let t: serde_json::Value = serde_json::from_str(&files["trigger.json"]).unwrap();
        assert_eq!(t["kind"], "owner_message");
        assert_eq!(t["message"], "技术选型用 Rust");
        assert!(files.contains_key("session.json"));

        // plan_reviewed 触发
        let plan = DagSpec::new(
            "req-1",
            vec![alfred_core::dagspec::PlanNode::new(
                "task-1",
                "s",
                Contract {
                    prompt: "p".into(),
                    acceptance_criteria: "a".into(),
                    reviewer_models: vec![],
                },
            )],
        );
        let verdict = alfred_core::verdict::PlanVerdict::new(true, "ok");
        let inputs = maintain_inputs(
            &doc,
            &MaintainTrigger::PlanReviewed { verdict, plan },
        )
        .unwrap();
        let files: std::collections::HashMap<String, String> = inputs.into_iter().collect();
        let t: serde_json::Value = serde_json::from_str(&files["trigger.json"]).unwrap();
        assert_eq!(t["kind"], "plan_reviewed");
        assert_eq!(t["verdict"]["pass"], true);
        assert_eq!(t["plan"]["request_id"], "req-1");
    }

    #[test]
    fn timestamp_helper_available() {
        assert!(now_rfc3339().contains('T'));
    }
}
