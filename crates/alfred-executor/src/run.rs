//! 一次执行运行的编排（R1 单节点）。
//!
//! 流程：建 run 目录 → 快照工作区 → 生成 compose + driver.py（宿主侧 Inspect
//! 容器驱动，非 eval；含 AGT 拦写层挂载）→ spawn `python3 driver.py` → 轮询
//! done 记录 → 产物采集（持久 ws git diff）→ 写 state.json + audit.jsonl。
//!
//! AGT 拦写层（属主钉死项：权限控制不让写文件——工具给到，越界写由工具级策略
//! 拦；属主拍板默认启用）：照 planner/reviewer 范式——`prepare_agt_work` 落策略
//! 到 `<run>/agt/` + compose 挂 `/tmp/.agt` ro（审计子目录 rw）+ driver env 注入
//! `-e` 扩展，容器内 pi 的 tool_call 命中策略即拒（越界写/rm -rf/sudo/秘密读取）
//! 并落审计。未设 `ALFRED_AGT_DIR` 用内置默认策略（`docker/agt/executor/`）；
//! `ALFRED_AGT_DISABLE=1` 显式关闭（见 crate::agt）。
//!
//! 依据（三容器 Inspect 统一管）：属主 08-27「容器统一用 Inspect AI 管理」——
//! 容器经 Inspect 容器管理接口（DockerSandboxEnvironment + sandbox_agent_bridge
//! + exec_remote）起容器/驱动 pi，不再经 `inspect eval` 评测包装。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crate::agt::{assets, prepare_agt_work, AgtSource};
use alfred_core::artifact::{Artifact, FileEntry};
use alfred_core::assignment::TaskAssignment;
use alfred_core::contract::{SandboxProfile, VolumeMount};
use alfred_core::request::OwnerRequest;
use alfred_core::util::{now_rfc3339, short_id};
use serde::Serialize;

use crate::artifact::{collect_artifact, snapshot_workspace};
use crate::compose_gen::{self, canonicalize_workspace, generate_executor_compose,
    validate_workspace_subdir, validate_workspace_subdirs, ExecutorMounts, CONTAINER_WORKSPACE_DIR};
use crate::config::ExecutorModel;
use crate::driver::{
    absolutize_cwd, poll_container_driver, spawn_container_driver, DriverOutcome, DriverSession,
};
use crate::task_gen::{generate_task_py, TaskGenParams};

/// 单次运行选项。
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// 运行目录（宿主，须位于 ~ 之下——E3）。
    pub run_dir: PathBuf,
    /// 工作区宿主目录（R6e：run 级单一持久 ws `<run>/ws`）。空 = 回退
    /// `run_dir/workspace`（旧 per-exec-N 布局，兼容独立调用/测试）。
    pub workspace_dir: PathBuf,
    /// 沙箱镜像。
    pub image: String,
    /// 任务环境 compose（G1 native_inspect 真实环境）：原任务 compose 的
    /// 绝对路径。`None` = 内置形态（network none 单容器 + 本 crate 生成的
    /// 挂载面）。`Some` = 复用原任务真实服务/环境变量/网络限制/资源上限，
    /// default 服务镜像换成 `image`，sidecar 服务（如 mysql）逐字保留。
    pub env_compose: Option<std::path::PathBuf>,
    /// 任务环境 compose 的 `${SAMPLE_METADATA_*}` 插值键值（driver 的
    /// sample_init 据此解析 compose 引用——与原任务装载同一解析链）。
    pub env_metadata: std::collections::BTreeMap<String, String>,
    /// 契约（prompt + acceptance_criteria）。
    pub assignment: TaskAssignment,
    /// 单样本时间上限（秒）。
    pub time_limit_secs: u32,
    /// 桥代理端口基数。
    pub port_base: u32,
    /// settled 后宽限（秒）。
    pub settle_grace_seconds: f64,
    /// 兼容保留（inspect ctl 已随去 eval 退役，当前无观测面轮询）。
    pub ctl_enabled: bool,
    /// AGT 拦写层源（默认内置策略；`ALFRED_AGT_DIR` 显式目录沿用覆盖；
    /// `ALFRED_AGT_DISABLE=1` 关）。Off = 不挂 AGT、不加载扩展。
    pub agt: AgtSource,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::new(),
            workspace_dir: PathBuf::new(),
            image: "alfred-executor:latest".to_string(),
            env_compose: None,
            env_metadata: std::collections::BTreeMap::new(),
            assignment: TaskAssignment::new(
                "task-unset",
                alfred_core::contract::Contract {
                    prompt: String::new(),
                    acceptance_criteria: String::new(),
                    reviewer_models: vec![],
                },
            ),
            time_limit_secs: 600,
            port_base: 13100,
            settle_grace_seconds: 20.0,
            ctl_enabled: true,
            agt: AgtSource::Builtin,
        }
    }
}

/// 运行结果（落盘 state.json）。
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub task_id: String,
    pub executor_model: String,
    /// 容器驱动状态（"success" / "error" / "timed_out" / "cancelled"——SIGTERM
    /// 等可捕获终止：shield 清理后如实落盘）。字段名沿用旧名
    /// `eval_status`（state.json 兼容，r6d.sh 断言 eval_status == "success"；
    /// 现承载 driver 状态，非 eval 状态）。
    pub eval_status: String,
    /// 驱动证据路径（`<work>/driver.done.json`）。字段名沿用旧名 `eval_location`
    /// （state.json 兼容；现承载 driver done 路径，非 eval 位置）。
    pub eval_location: Option<String>,
    pub artifact: Option<Artifact>,
    pub started_at: String,
    pub finished_at: String,
    /// 失败原因（容器驱动 status error / timed_out / crashed 各自填）；成功为 None。
    pub error: Option<String>,
    /// 原生 session 关联（driver done 从 pi RPC `get_state` 响应取得；poll 层
    /// 失败无 done 记录 = None）。身份与落盘分开：`file_exists` 仅在宿主文件
    /// 可定位时实测断言（W08：身份已分配 ≠ 文件存在）。
    pub session: Option<DriverSession>,
}

/// run 目录缺省基座（`ALFRED_STATE_DIR` 或 `~/.local/state/alfred/runs`）。
pub fn default_run_dir() -> PathBuf {
    let base = std::env::var("ALFRED_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            PathBuf::from(home).join(".local/state/alfred/runs")
        });
    base.join(short_id("run"))
}

/// R6e：解析执行工作区宿主目录。显式 `workspace_dir` 优先；空（未设）回退
/// `run_dir/workspace`（旧 per-exec-N 布局，兼容独立调用/测试）。
pub fn resolve_workspace_dir(run_dir: &Path, workspace_dir: &Path) -> PathBuf {
    if workspace_dir.as_os_str().is_empty() {
        run_dir.join("workspace")
    } else {
        workspace_dir.to_path_buf()
    }
}

/// R6e：治理 run 初始化单一持久 ws（`<run>/ws` + git init 基线快照）。
/// 三容器共享此 ws：executor 产物 rw、planner/reviewer 全量 ro（reviewer 自己
/// 看 git diff 基线）。幂等——`<run>/ws` 已 git 初始化则直接返回。
pub fn ensure_run_workspace(run_dir: &Path) -> Result<PathBuf> {
    let ws = run_dir.join("ws");
    std::fs::create_dir_all(&ws).with_context(|| format!("create run ws {}", ws.display()))?;
    init_workspace_git(&ws)?;
    Ok(ws)
}

/// R6e：把 ws 初始化为 git 仓库并提交基线（diff 基线）。
///
/// 幂等：`<dir>/.git` 已存在则直接返回（不重复 init/commit）。基线提交把
/// init 时刻 ws 里的既有内容（治理 run 预置的初态材料——委托方在 `alfred
/// run` 前布置的工作区整树）全部入库：执行者之后的改动（已跟踪文件修改/
/// 未跟踪新建）相对基线真实可见；预置材料本身不再混进"执行者新建"面
/// （空 ws 时 `git add -A` 无可暂存，`--allow-empty` 保持既有空提交形态，
/// 字节级行为不变）。reviewer 挂 ws 全量 ro，用只读 git 命令
/// （status/diff/log）对照基线看执行者改了什么。
pub fn init_workspace_git(dir: &Path) -> Result<()> {
    if dir.join(".git").exists() {
        return Ok(());
    }
    let run_git = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "alfred")
            .env("GIT_AUTHOR_EMAIL", "alfred@local")
            .env("GIT_COMMITTER_NAME", "alfred")
            .env("GIT_COMMITTER_EMAIL", "alfred@local")
            .output()
            .with_context(|| format!("git {} in {}", args.join(" "), dir.display()))?;
        if !out.status.success() {
            bail!(
                "git {} failed in {}: {}",
                args.join(" "),
                dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    };
    run_git(&["init", "-q"])?;
    // 预置初态材料入基线（I02 集成：委托方 seed 的任务工作区整树）——
    // 空 ws 无可暂存时该步空转，下一行 --allow-empty 落空提交（原行为）。
    run_git(&["add", "-A"])?;
    run_git(&["commit", "--allow-empty", "-q", "-m", "R6e baseline: run 级单一 ws 基线快照"])?;
    Ok(())
}

/// 校验 executor 沙箱档案（R6e + 9/3 方案②：宿主材料进路 ref_volumes 打通）。
///
/// - `workspace_subdirs`：非空强制（R6e 块B：executor ws 挂载非空保证——空声明
///   是计划缺陷，计划审查应打回重规划；不静默跳过挂载、不静默回退挂全量）+
///   首子目录名重复声明拒绝（A4 挂载锚：防 compose 双挂载同宿主目录 + 锚断言
///   矛盾，校验真源 [`compose_gen::validate_workspace_subdirs`]，单一真源）。
/// - `volumes`：**只读参考卷放行**（宿主材料进路：planner 声明
///   `{"host_path","container_path","mode"}`，把任务要读的宿主材料以 ro 挂进
///   执行容器）。逐卷校验放行条件：mode == "ro" + host 路径存在于宿主 +
///   container 路径合法（绝对、不与工作区冲突）——校验真源
///   [`validate_ref_volume`]（与 compose_gen 共用，单一真源）。
/// - runtime/packages remain unsupported; network must match the owner-bound
///   compose (or network none for the built-in environment).
fn validate_executor_sandbox(sandbox: &SandboxProfile, compose: Option<&Path>) -> Result<()> {
    compose_gen::validate_task_network(sandbox.network, compose)?;
    if sandbox.runtime.is_some() || !sandbox.packages.is_empty() {
        bail!("executor does not support runtime/packages overrides; dependencies must be delivered by the bound image");
    }
    for vol in &sandbox.volumes {
        validate_ref_volume(vol)?;
    }
    if sandbox.workspace_subdirs.is_empty() {
        // R6e 块B：executor ws 挂载非空保证——空声明 = 计划缺陷（计划审查应打回
        // 重规划），executor 侧防御性失败：不静默跳过挂载、不静默回退挂全量。
        bail!(
            "executor 沙箱 workspace_subdirs 为空：计划审查应拦截，executor 挂载不能为空（拒绝空声明，不挂全量）"
        );
    }
    // A4 挂载锚：首子目录名重复声明显式拒绝（防 compose 双挂载同宿主目录 +
    // mount_anchor_prompt 锚断言矛盾）。列表级校验与 compose 生成共用（单一真源）。
    validate_workspace_subdirs(&sandbox.workspace_subdirs)?;
    Ok(())
}

/// 只读参考卷单卷校验（9/3 方案②；`validate_executor_sandbox` 与 compose 生成
/// 层共用真源，此处为 wrapper）。
fn validate_ref_volume(vol: &VolumeMount) -> Result<()> {
    compose_gen::validate_ref_volume(vol)
}



///
/// R6d：执行容器只出产物——不再绑定 grader（内嵌 scorer 已移除，执行审查
/// 由 reviewer 容器承担，见 governance exec_review_step）。
pub fn execute_run(
    opts: &RunOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
) -> Result<RunOutcome> {
    // 沙箱档案校验（R6e + 9/3 方案②）：executor 支持契约声明的 workspace_subdirs
    // 子集挂载（R6e 块B：非空挂载保证，空声明防御性报错）+ 只读参考卷 volumes
    // Network must match the owner-bound compose, never a silent override.
    validate_executor_sandbox(&opts.assignment.sandbox, opts.env_compose.as_deref())?;
    let started_at = now_rfc3339();
    let run_id = match opts.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("run"),
    };
    let run_dir = &opts.run_dir;

    // 1) 目录与工作区（须先于 compose 生成存在）
    std::fs::create_dir_all(run_dir).with_context(|| format!("create run dir {}", run_dir.display()))?;
    // R6e：工作区 = run 级单一持久 ws（治理环传 `<run>/ws`；空 = 旧 per-exec-N 布局兜底）。
    let workspace_host = resolve_workspace_dir(run_dir, &opts.workspace_dir);
    std::fs::create_dir_all(&workspace_host)?;
    // R6e：契约声明的 workspace_subdirs 先建目录（空目录 = 执行者工作区根/可见子集）。
    // 逐项校验与 compose 生成共用 validate_workspace_subdir（单一真源）；列表级
    // 校验（非空 + 首名重复声明拒绝）已在 validate_executor_sandbox 入口做过。
    // 挂载点父目录 rw，执行者可在其下动态新建子目录（新建即宿主可见/git 可见）。
    for sub in &opts.assignment.sandbox.workspace_subdirs {
        validate_workspace_subdir(sub)?;
        std::fs::create_dir_all(workspace_host.join(sub))
            .with_context(|| format!("create workspace subdir {}", workspace_host.join(sub).display()))?;
    }
    canonicalize_workspace(&workspace_host)?;
    // R6e：git 基线（幂等）——executor 改动相对基线可见，reviewer 挂 ws 全量 ro 自己看 git diff。
    init_workspace_git(&workspace_host)?;

    // AGT 拦写层（默认启用）：落策略 + 扩展到 `<run>/agt/`（策略 ro），审计
    // 子目录 rw（审计 JSONL 落宿主）。未设 env = 内置默认策略；`ALFRED_AGT_DIR`
    // 显式目录沿用覆盖；`ALFRED_AGT_DISABLE=1` 不挂。照 planner/reviewer 范式
    // （prepare_agt_work + /tmp/.agt 挂载 + env 注入 `-e` 扩展）。
    let agt_work = prepare_agt_work(run_dir, &opts.agt, assets::EXECUTOR_POLICY)?;
    // 2) 执行前工作区快照（文件比对基线）
    let before = snapshot_workspace(&workspace_host)?;

    // 3) 生成 compose + driver.py（宿主侧 Inspect 容器驱动，非 eval Task）
    let compose_path = run_dir.join("executor.compose.yaml");
    // R6e：executor 容器挂载 = workspace_subdirs 声明子集（rw），非全量 ws（块B：
    // 非空挂载保证——空 subdirs 已被 validate_executor_sandbox 防御性报错）。
    // 9/3 方案②：契约 sandbox.volumes 的宿主参考材料以 ro 挂进执行容器（挂载矩阵
    // "只读参考卷由编排器按 SandboxProfile.volumes 动态追加"）；逐卷校验已过
    // （validate_executor_sandbox → validate_ref_volume）。AGT 拦写层照
    // reviewer 范式接入：策略目录 /tmp/.agt ro + 审计子目录 rw（审计落宿主）。
    let sessions_dir = run_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).context("create executor session directory")?;
    // 宿主保留路径取绝对（与 compose 挂载源同源）：driver 据此把 pi RPC 返回
    // 的容器内 sessionFile 精确映射到宿主保留位置（executor_driver.py.tmpl
    // `_session_record`）。
    let sessions_dir = sessions_dir
        .canonicalize()
        .with_context(|| format!("canonicalize sessions dir {}", sessions_dir.display()))?;
    let mounts = ExecutorMounts {
        workspace_subdirs: opts.assignment.sandbox.workspace_subdirs.clone(),
        // 参考卷：契约声明的只读宿主材料卷（方案②动态追加挂载）。
        ref_volumes: opts.assignment.sandbox.volumes.clone(),
        agt_dir: agt_work.clone(),
        agt_audit_dir: agt_work.as_ref().map(|p| p.join("audit")),
        sessions_dir: Some(sessions_dir.clone()),
    };
    let compose = match &opts.env_compose {
        // 任务环境 compose（G1 native_inspect 真实环境）：复用原任务真实
        // 服务/环境变量/网络限制/资源上限，default 镜像换执行镜像，alfred
        // 挂载面追加；sidecar（如 mysql）逐字保留（compose_gen 落码）。
        Some(orig) => {
            compose_gen::generate_task_env_compose(orig, &workspace_host, &opts.image, &mounts)?
        }
        // 内置形态（既有行为）：network none 单容器 + 本 crate 生成的挂载面。
        None => generate_executor_compose(&workspace_host, &opts.image, &mounts)?,
    };
    std::fs::write(&compose_path, compose)?;
    append_audit(
        run_dir,
        "executor_environment_form",
        &serde_json::json!({
            "form": if opts.env_compose.is_some() { "task_compose" } else { "builtin_network_none" },
            "env_compose": opts.env_compose.as_ref().map(|p| p.display().to_string()),
            "env_metadata_keys": opts.env_metadata.keys().collect::<Vec<_>>(),
            "sandbox_network": opts.assignment.sandbox.network,
        }),
    )?;
    // 注入 driver.py 的 compose 路径必须绝对（E1/E3：相对路径被 docker 静默变
    // named volume；colima 只共享 ~）。
    let compose_abs = compose_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", compose_path.display()))?;

    // 嵌入 driver.py 的 done 路径必须绝对：驱动进程 cwd 切到 work_dir 后，相对
    // 路径被二次解析（双拼）——与 spawn 层 absolutize_cwd 同源约束。
    let done_marker = absolutize_cwd(&run_dir.join("driver.done.json"));
    let driver_py = run_dir.join("driver.py");
    // AGT 扩展/策略/审计的容器内路径（照 planner/reviewer 范式；空串 = 不加载）。
    let (agt_ext, agt_policy_path, agt_audit_path) = match &agt_work {
        Some(_) => (
            "/tmp/.agt/agt-policy.ts".to_string(),
            "/tmp/.agt/policy.json".to_string(),
            "/tmp/.agt/audit/audit.jsonl".to_string(),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    let py = generate_task_py(&TaskGenParams {
        compose_file: compose_abs.to_string_lossy().into_owned(),
        contract_prompt: opts.assignment.contract.prompt.clone(),
        // 挂载翻译锚（task_gen::mount_anchor_prompt）：workspace_subdirs[0] 即
        // 执行者 /workspace 根——契约目录名不再字面嵌套（src 嵌套歧义治本）。
        workspace_subdirs: opts.assignment.sandbox.workspace_subdirs.clone(),
        port: opts.port_base,
        pi_model: "inspect-bridge/inspect".to_string(),
        bridge_model: format!("inspect/{}", model.inspect_model_id()),
        max_tokens: model.max_tokens,
        context_window: model.context_window,
        workspace_dir: CONTAINER_WORKSPACE_DIR.to_string(),
        sandbox_user: "root".to_string(),
        run_id: run_id.clone(),
        settle_grace_seconds: opts.settle_grace_seconds,
        time_limit_secs: opts.time_limit_secs,
        done_marker: done_marker.to_string_lossy().into_owned(),
        agt_ext,
        agt_policy_path,
        agt_audit_path,
        // 9/3 方案②：参考卷容器内挂载点注入 AGT 豁免面（AGT_REF_VOLUMES）。
        ref_volume_dirs: opts
            .assignment
            .sandbox
            .volumes
            .iter()
            .map(|v| v.container_path.clone())
            .collect(),
        task_name: "alfred-executor".to_string(),
        // 原生 session 宿主保留目录（绝对）：driver 据此映射 RPC 返回的容器内
        // sessionFile → 宿主保留路径（done 记录 session.host_file）。
        sessions_dir_host: sessions_dir.to_string_lossy().into_owned(),
        // G1 native_inspect：外层实验绑定透传（非秘密引用 JSON 对象；空 = 未
        // 绑定，如手工 feed 续跑）。driver 把它与实际环境关联写进 done 记录。
        evidence_binding: evidence_binding_from_env()?,
        // 任务环境 compose 的 ${SAMPLE_METADATA_*} 插值键（G1 真实环境）：
        // driver 的 sample_init 用它们解析 compose 引用（与原任务装载同一
        // 解析链；空 = compose 无插值引用，sample_init 行为与既有完全一致）。
        sandbox_metadata: opts.env_metadata.clone(),
    })?;
    std::fs::write(&driver_py, py)?;

    append_audit(run_dir, "run_started", &serde_json::json!({ "run_id": run_id, "task_id": opts.assignment.task_id }))?;

    // 4) spawn 宿主侧容器驱动（非 eval）
    let mut launch = spawn_container_driver(&driver_py, model, run_dir)?;
    append_audit(
        run_dir,
        "container_driver_launched",
        &serde_json::json!({ "pid": launch.pid, "done_marker": launch.done_marker }),
    )?;

    // 5) 轮询（timeout = time_limit + 缓冲）
    let poll_timeout = opts.time_limit_secs as u64 + 600;
    let outcome = match poll_container_driver(&mut launch, poll_timeout)? {
        DriverOutcome::Done(done) => done,
        DriverOutcome::TimedOut => {
            // 失败路径填 error（P3）：state.json 落 timed_out 原因后仍以 Err 上报。
            // 提交字节尽力冻结（G1 native_inspect）：poll 超时时驱动进程可能仍在
            // 收尾，快照标 stability=post-failure-snapshot——字节是时点事实，
            // manifest 不冒充最终态。
            let msg = format!(
                "container driver timed out after {}s (no done record in {})",
                poll_timeout,
                launch.done_marker.display()
            );
            if let Err(freeze_err) = freeze_submission(
                run_dir, &opts.assignment.task_id, &workspace_host, &before,
                "post-failure-snapshot",
            ) {
                append_audit(
                    run_dir,
                    "submission_freeze_failed",
                    &serde_json::json!({ "error": format!("{freeze_err:#}") }),
                )?;
            }
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                "timed_out", None, "container_driver_timed_out", &msg,
            )?;
            bail!(msg);
        }
        DriverOutcome::Crashed(exit_code) => {
            // 失败路径填 error（P3）：state.json 落 crashed 原因后仍以 Err 上报。
            // 提交字节尽力冻结（同上，stability 标注）。
            let msg = format!(
                "container driver process died without a done record (exit: {:?}, done: {})",
                exit_code,
                launch.done_marker.display()
            );
            if let Err(freeze_err) = freeze_submission(
                run_dir, &opts.assignment.task_id, &workspace_host, &before,
                "post-failure-snapshot",
            ) {
                append_audit(
                    run_dir,
                    "submission_freeze_failed",
                    &serde_json::json!({ "error": format!("{freeze_err:#}") }),
                )?;
            }
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                "crashed", None, "container_driver_crashed", &msg,
            )?;
            bail!(msg);
        }
    };

    // R6d：执行容器只出产物无审查行为——审查由 reviewer 容器承担（governance
    // exec_review_step）。驱动证据 = driver.done.json + driver.stdout/stderr.log
    // （P9 审计），不再有 .eval 文件可归档。
    append_audit(
        run_dir,
        "container_driver_done",
        &serde_json::json!({
            "status": outcome.status,
            "error": outcome.error,
            // 原生 session 关联进审计轨迹（P9）：身份来自 pi RPC 实测响应。
            "session": serde_json::to_value(&outcome.session)
                .unwrap_or_else(|_| serde_json::Value::Null),
        }),
    )?;
    // 7) 产物采集（执行后快照 → diff）+ 提交字节冻结（G1 native_inspect：
    // 本执行的产物字节在本函数返回前落盘到 <exec>/submission/——后续执行
    // 覆盖工作区之前字节已固定，外层按精确引用消费，不做赛后补拍）。
    let artifact = freeze_submission(run_dir, &opts.assignment.task_id, &workspace_host, &before, "post-execution")?;
    append_audit(
        run_dir,
        "artifact_collected",
        &serde_json::json!({ "change_count": artifact.changes.len(), "file_count": artifact.files.len() }),
    )?;

    // 8) 落盘 state.json（失败路径 error 填原因——P3：error 不再是死字段）
    let finished_at = now_rfc3339();
    let eval_error = (outcome.status != "success").then(|| {
        let err = outcome
            .error
            .clone()
            .unwrap_or_else(|| "container driver finished with non-success status".to_string());
        format!("container driver status '{}': {err}", outcome.status)
    });
    let rec = RunOutcome {
        run_id,
        task_id: opts.assignment.task_id.clone(),
        executor_model: model.inspect_model_id(),
        eval_status: outcome.status.clone(),
        eval_location: Some(
            launch
                .done_marker
                .to_string_lossy()
                .into_owned(),
        ),
        artifact: Some(artifact.clone()),
        started_at,
        finished_at,
        error: eval_error,
        // 原生 session 关联（done 记录消费）：driver 从 pi RPC get_state 取得。
        session: outcome.session,
    };
    write_state(run_dir, request, &rec)?;
    append_audit(run_dir, "run_finished", &serde_json::json!({ "status": outcome.status, "error": rec.error }))?;

    // task error ≠ crash：done 照发，但 status 是 error——state.json 已记 error，仍如实报给调用方
    if let Some(err) = &rec.error {
        bail!("{err}");
    }
    Ok(rec)
}

/// 读外层实验注入的证据绑定（`ALFRED_EVIDENCE_BINDING`，JSON 对象字符串）。
///
/// 绑定只含非秘密配置引用（run_ref / inputs_ref / evidence_config_ref /
/// terminal_evidence_spec_ref / collector_entrypoint_ref 等，G1 §二
/// EvidenceBinding）——凭证不进 binding，跨进程不传 Python 对象。空 = 未
/// 绑定（独立调用/手工 feed 续跑）；非对象 JSON 显式报错（契约违规可见，
/// 不静默丢弃）。
fn evidence_binding_from_env() -> Result<String> {
    let raw = std::env::var("ALFRED_EVIDENCE_BINDING").unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(String::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("parse ALFRED_EVIDENCE_BINDING as JSON")?;
    if !value.is_object() {
        bail!("ALFRED_EVIDENCE_BINDING must be a JSON object of non-secret refs");
    }
    Ok(raw)
}

/// 提交字节冻结（G1 native_inspect 子执行保全，阻塞 hook）：本执行的产物
/// 字节在 `execute_run` 返回前落盘到 `<exec>/submission/`。
///
/// 时序保证：治理环单线程推进——本函数运行时下一执行尚未开始（成功路径
/// driver done 已读、容器已收尾；失败路径为尽力时点快照，manifest 的
/// `stability` 如实标注）。后续执行覆盖工作区之前，本执行的产物字节已
/// 固定在 exec 目录内，外层按精确引用消费，不需要赛后补拍。
///
/// 产物：`submission/payload/<rel>`（created/modified 逐文件字节副本）+
/// `submission/manifest.json`（task_id / frozen_at / stability / files[] /
/// deleted[]，逐文件 sha256+size；副本回读校验，payload_sha256 与快照
/// sha256 不一致时并列两值，不静默修正）。`collect_artifact` 的差异/哈希
/// 照旧进 state.json（原生记录形态不变）。
fn freeze_submission(
    run_dir: &Path,
    task_id: &str,
    workspace_host: &Path,
    before: &std::collections::BTreeMap<String, FileEntry>,
    stability: &str,
) -> Result<Artifact> {
    use sha2::{Digest, Sha256};

    // Dependency restoration runs inside the actual mounted Inspect sandbox,
    // before pi. Its host-side snapshot is the baseline for this execution.
    let prepared_before;
    let before_path = run_dir.join("workspace.before.json");
    let before = if before_path.is_file() {
        prepared_before = serde_json::from_str::<std::collections::BTreeMap<String, FileEntry>>(
            &std::fs::read_to_string(&before_path)?
        ).context("read prepared workspace baseline")?;
        &prepared_before
    } else {
        before
    };
    let artifact = collect_artifact(task_id, workspace_host, before)?;
    let submission_dir = run_dir.join("submission");
    std::fs::create_dir_all(&submission_dir).context("create submission directory")?;
    let payload_dir = submission_dir.join("payload");
    let mut files: Vec<serde_json::Value> = Vec::new();
    for change in &artifact.changes {
        let after = match &change.after {
            Some(after) => after,
            None => continue, // deleted 走 deleted 列表，字节无副本
        };
        let src = workspace_host.join(&change.path);
        let dst = payload_dir.join(&change.path);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create submission payload dir {}", parent.display())
            })?;
        }
        std::fs::copy(&src, &dst).with_context(|| {
            format!(
                "copy submission payload {} -> {}",
                src.display(),
                dst.display()
            )
        })?;
        // 副本回读校验：工作区在复制期间被写入（失败路径的时点快照）时两值
        // 并列如实落 manifest——冻结不失败、也不冒称一致。
        let copied = std::fs::read(&dst)
            .with_context(|| format!("read back submission payload {}", dst.display()))?;
        let payload_sha256 = hex::encode(Sha256::digest(&copied));
        files.push(serde_json::json!({
            "path": change.path,
            "kind": format!("{:?}", change.kind).to_lowercase(),
            "size": after.size,
            "sha256": after.sha256,
            "payload_sha256": payload_sha256,
            "sha_match": payload_sha256 == after.sha256,
        }));
    }
    let deleted: Vec<serde_json::Value> = artifact
        .changes
        .iter()
        .filter(|c| c.after.is_none())
        .filter_map(|c| {
            c.before.as_ref().map(|b| {
                serde_json::json!({"path": c.path, "size": b.size, "sha256": b.sha256})
            })
        })
        .collect();
    let manifest_path = submission_dir.join("manifest.json");
    let manifest = serde_json::json!({
        "task_id": task_id,
        "frozen_at": now_rfc3339(),
        "stability": stability,
        "files": files,
        "deleted": deleted,
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).context("serialize submission manifest")?,
    )
    .with_context(|| format!("write submission manifest {}", manifest_path.display()))?;
    append_audit(
        run_dir,
        "submission_frozen",
        &serde_json::json!({
            "stability": stability,
            "file_count": artifact.changes.iter().filter(|c| c.after.is_some()).count(),
            "deleted_count": artifact.changes.iter().filter(|c| c.after.is_none()).count(),
        }),
    )?;
    Ok(artifact)
}
/// 失败路径统一构造 RunOutcome + 落 audit + 落盘 state.json（不悄悄放行）。
///
/// 容器驱动异常（timed_out/crashed/status error）共用——不再用 if-let 静默吞
/// 错误。填 error 后以 Err 上报给调用方（不悄悄放行）。
#[allow(clippy::too_many_arguments)]
fn fail_run(
    run_dir: &Path,
    request: &OwnerRequest,
    opts: &RunOptions,
    model: &ExecutorModel,
    run_id: &str,
    started_at: &str,
    eval_status: &str,
    eval_location: Option<&str>,
    event: &str,
    msg: &str,
) -> Result<()> {
    append_audit(run_dir, event, &serde_json::json!({ "run_id": run_id, "error": msg }))?;
    let rec = RunOutcome {
        run_id: run_id.to_string(),
        task_id: opts.assignment.task_id.clone(),
        executor_model: model.inspect_model_id(),
        eval_status: eval_status.to_string(),
        eval_location: eval_location.map(String::from),
        artifact: None,
        started_at: started_at.to_string(),
        finished_at: now_rfc3339(),
        error: Some(msg.to_string()),
        // poll 层失败（timed_out/crashed）无 done 记录——无 RPC 取得的身份，
        // 如实 None；不扫 sessions 目录反猜（驱动自身超时路径经 done 记录
        // 携带 session，不走本分支）。
        session: None,
    };
    write_state(run_dir, request, &rec)?;
    Ok(())
}

#[derive(Serialize)]
struct StateFile {
    run: RunOutcome,
    request: OwnerRequest,
}

fn write_state(run_dir: &Path, request: &OwnerRequest, rec: &RunOutcome) -> Result<()> {
    let state = StateFile {
        run: rec.clone(),
        request: request.clone(),
    };
    let path = run_dir.join("state.json");
    let text = serde_json::to_string_pretty(&state).context("serialize state.json")?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// 追加一行审计事件（P9 证据：编排器轨迹）。
fn append_audit(run_dir: &Path, event: &str, data: &serde_json::Value) -> Result<()> {
    let line = serde_json::json!({
        "ts": now_rfc3339(),
        "event": event,
        "data": data,
    });
    let path = run_dir.join("audit.jsonl");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("append {}", path.display()))?;
    Ok(())
}
