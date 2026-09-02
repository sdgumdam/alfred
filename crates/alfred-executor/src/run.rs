//! 一次执行运行的编排（R1 单节点）。
//!
//! 流程：建 run 目录 → 快照工作区 → 生成 compose + driver.py（宿主侧 Inspect
//! 容器驱动，非 eval；含 AGT 拦写层挂载）→ spawn `python3 driver.py` → 轮询
//! done 记录 → 产物采集（持久 ws git diff）→ 写 state.json + audit.jsonl。
//!
//! AGT 拦写层（属主钉死项：权限控制不让写文件——工具给到，越界写由工具级策略
//! 拦）：照 planner/reviewer 范式——`prepare_agt_work` 拷策略到 `<run>/agt/` +
//! compose 挂 `/tmp/.agt` ro（审计子目录 rw）+ driver env 注入 `-e` 扩展，容器
//! 内 pi 的 tool_call 命中策略即拒（越界写/rm -rf/sudo/秘密读取）并落审计。
//!
//! 依据（三容器 Inspect 统一管）：属主 08-27「容器统一用 Inspect AI 管理」——
//! 容器经 Inspect 容器管理接口（DockerSandboxEnvironment + sandbox_agent_bridge
//! + exec_remote）起容器/驱动 pi，不再经 `inspect eval` 评测包装。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::artifact::Artifact;
use alfred_core::assignment::TaskAssignment;
use alfred_core::contract::SandboxProfile;
use alfred_core::request::OwnerRequest;
use alfred_core::util::{now_rfc3339, short_id};
use serde::Serialize;

use crate::artifact::{collect_artifact, snapshot_workspace};
use crate::compose_gen::{
    canonicalize_workspace, generate_executor_compose, validate_workspace_subdir, ExecutorMounts,
    CONTAINER_WORKSPACE_DIR,
};
use crate::config::ExecutorModel;
use crate::driver::{absolutize_cwd, poll_container_driver, spawn_container_driver, DriverOutcome};
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
    /// AGT 策略 + 扩展目录（源：含 agt-policy.ts + policy.json）。拷贝到
    /// `<run>/agt/`（策略 ro + 审计子目录 rw）挂 `/tmp/.agt`，容器内 pi 经
    /// `-e` 加载扩展拦截 tool_call（越界写/危险命令，审计 JSONL 落宿主）。
    /// None = 不挂 AGT、不加载扩展（`ALFRED_AGT_DIR` 未设，测试/最小环境）。
    pub agt_dir: Option<PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::new(),
            workspace_dir: PathBuf::new(),
            image: "alfred-executor:latest".to_string(),
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
            agt_dir: None,
        }
    }
}

/// 运行结果（落盘 state.json）。
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub task_id: String,
    pub executor_model: String,
    /// 容器驱动状态（"success" / "error" / "timed_out"）。字段名沿用旧名
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

/// R6e：把 ws 初始化为 git 仓库并提交基线空提交（diff 基线）。
///
/// 幂等：`<dir>/.git` 已存在则直接返回（不重复 init/commit）。基线 = 空提交
/// （`git init` 后 `commit --allow-empty`）：executor 之后的改动（未跟踪新建/
/// 已跟踪修改）相对基线可见；reviewer 容器挂 ws 全量 ro，用只读 git 命令
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
    run_git(&["commit", "--allow-empty", "-q", "-m", "R6e baseline: run 级单一 ws 基线快照"])?;
    Ok(())
}

/// 校验 executor 沙箱档案（R6e）：仅允许契约声明的 `workspace_subdirs` 子集挂载。
/// **空声明 = 防御性报错**（R6e 块B：executor ws 挂载非空保证——空声明是计划
/// 缺陷，计划审查应打回重规划；不静默跳过挂载、不静默回退挂全量）。
/// volumes/runtime/packages/network 执行驱动尚不支持——按审计约束显式拒绝而非
/// 静默忽略，防止"申请的约束没生效"。
fn validate_executor_sandbox(sandbox: &SandboxProfile) -> Result<()> {
    if !sandbox.volumes.is_empty()
        || sandbox.runtime.is_some()
        || !sandbox.packages.is_empty()
        || sandbox.network
    {
        bail!(
            "executor 沙箱档案不支持 volumes/runtime/packages/network（当前 sandbox={sandbox:?}）；仅支持 workspace_subdirs 子集挂载"
        );
    }
    if sandbox.workspace_subdirs.is_empty() {
        // R6e 块B：executor ws 挂载非空保证——空声明 = 计划缺陷（计划审查应打回
        // 重规划），executor 侧防御性失败：不静默跳过挂载、不静默回退挂全量。
        bail!(
            "executor 沙箱 workspace_subdirs 为空：计划审查应拦截，executor 挂载不能为空（拒绝空声明，不挂全量）"
        );
    }
    Ok(())
}

/// AGT 目录解析：`ALFRED_AGT_DIR`（executor 边界策略目录）；未设 → None。
/// 与 planner/reviewer 共用同一 env（策略文件内容不同：executor 用
/// `tests/e2e/agt/policy.json` 的沙箱边界策略——workspace-write-only /
/// no-sudo / recursive-delete / host-secret-read / no-host-path-touch）。
pub fn resolve_agt_dir() -> Option<PathBuf> {
    std::env::var("ALFRED_AGT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// AGT 拦写层准备（照 planner/reviewer 范式）：拷贝源 agt 目录（agt-policy.ts +
/// policy.json）到 `<work>/agt/`，建审计子目录 `audit/`（rw 挂载源，审计 JSONL
/// 落宿主）。None → 不挂 AGT。
pub fn prepare_agt_work(work: &Path, agt_dir: &Option<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(src) = agt_dir else {
        return Ok(None);
    };
    let dest = work.join("agt");
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("create executor agt dir {}", dest.display()))?;
    std::fs::create_dir_all(dest.join("audit")).with_context(|| {
        format!(
            "create executor agt audit dir {}",
            dest.join("audit").display()
        )
    })?;
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


///
/// R6d：执行容器只出产物——不再绑定 grader（内嵌 scorer 已移除，执行审查
/// 由 reviewer 容器承担，见 governance exec_review_step）。
pub fn execute_run(
    opts: &RunOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
) -> Result<RunOutcome> {
    // 沙箱档案校验（R6e）：executor 支持契约声明的 workspace_subdirs 子集挂载
    // （R6e 块B：非空挂载保证，空声明防御性报错）；volumes/runtime/packages/network
    // 显式拒绝（审计约束：防止"申请的约束没生效"）。见 validate_executor_sandbox。
    validate_executor_sandbox(&opts.assignment.sandbox)?;
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
    // 校验与 compose 生成共用 validate_workspace_subdir（单一真源）；挂载点父目录 rw，
    // 执行者可在其下动态新建子目录（新建即宿主可见/git 可见）。
    for sub in &opts.assignment.sandbox.workspace_subdirs {
        validate_workspace_subdir(sub)?;
        std::fs::create_dir_all(workspace_host.join(sub))
            .with_context(|| format!("create workspace subdir {}", workspace_host.join(sub).display()))?;
    }
    canonicalize_workspace(&workspace_host)?;
    // R6e：git 基线（幂等）——executor 改动相对基线可见，reviewer 挂 ws 全量 ro 自己看 git diff。
    init_workspace_git(&workspace_host)?;

    // AGT 拦写层（属主钉死项：权限控制不让写文件）：拷贝策略 + 扩展到
    // `<run>/agt/`（策略 ro），审计子目录 rw（审计 JSONL 落宿主）。照
    // planner/reviewer 接入范式（prepare_agt_work + /tmp/.agt 挂载 + env 注入
    // `-e` 扩展）；None = 不挂（`ALFRED_AGT_DIR` 未设，测试/最小环境）。
    let agt_work = prepare_agt_work(run_dir, &opts.agt_dir)?;
    // 2) 执行前工作区快照（文件比对基线）
    let before = snapshot_workspace(&workspace_host)?;

    // 3) 生成 compose + driver.py（宿主侧 Inspect 容器驱动，非 eval Task）
    let compose_path = run_dir.join("executor.compose.yaml");
    // R6e：executor 容器挂载 = workspace_subdirs 声明子集（rw），非全量 ws（块B：
    // 非空挂载保证——空 subdirs 已被 validate_executor_sandbox 防御性报错）。
    // 参考卷仍拒绝（sandbox 校验已拒 volumes）；AGT 拦写层照 reviewer 范式接入：
    // 策略目录 /tmp/.agt ro（agent 不可改策略）+ 审计子目录 rw（审计落宿主）。
    let mounts = ExecutorMounts {
        workspace_subdirs: opts.assignment.sandbox.workspace_subdirs.clone(),
        // 参考卷：sandbox 校验已拒绝 volumes（执行驱动不支持），恒空。
        ref_volumes: vec![],
        agt_dir: agt_work.clone(),
        agt_audit_dir: agt_work.as_ref().map(|p| p.join("audit")),
    };
    let compose = generate_executor_compose(&workspace_host, &opts.image, &mounts)?;
    std::fs::write(&compose_path, compose)?;
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
        workspace_dir: CONTAINER_WORKSPACE_DIR.to_string(),
        sandbox_user: "root".to_string(),
        run_id: run_id.clone(),
        settle_grace_seconds: opts.settle_grace_seconds,
        time_limit_secs: opts.time_limit_secs,
        done_marker: done_marker.to_string_lossy().into_owned(),
        agt_ext,
        agt_policy_path,
        agt_audit_path,
        task_name: "alfred-executor".to_string(),
    })?;
    std::fs::write(&driver_py, py)?;

    append_audit(run_dir, "run_started", &serde_json::json!({ "run_id": run_id, "task_id": opts.assignment.task_id }))?;

    // 4) spawn 宿主侧容器驱动（非 eval）
    let launch = spawn_container_driver(&driver_py, model, run_dir)?;
    append_audit(
        run_dir,
        "container_driver_launched",
        &serde_json::json!({ "pid": launch.pid, "done_marker": launch.done_marker }),
    )?;

    // 5) 轮询（timeout = time_limit + 缓冲）
    let poll_timeout = opts.time_limit_secs as u64 + 600;
    let outcome = match poll_container_driver(&launch, poll_timeout)? {
        DriverOutcome::Done(done) => done,
        DriverOutcome::TimedOut => {
            // 失败路径填 error（P3）：state.json 落 timed_out 原因后仍以 Err 上报
            let msg = format!(
                "container driver timed out after {}s (no done record in {})",
                poll_timeout,
                launch.done_marker.display()
            );
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                "timed_out", None, "container_driver_timed_out", &msg,
            )?;
            bail!(msg);
        }
        DriverOutcome::Crashed => {
            // 失败路径填 error（P3）：state.json 落 crashed 原因后仍以 Err 上报
            let msg = format!(
                "container driver process died without a done record (done: {})",
                launch.done_marker.display()
            );
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
        &serde_json::json!({ "status": outcome.status, "error": outcome.error }),
    )?;
    // 7) 产物采集（执行后快照 → diff）
    let artifact = collect_artifact(&opts.assignment.task_id, &workspace_host, &before)?;
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
    };
    write_state(run_dir, request, &rec)?;
    append_audit(run_dir, "run_finished", &serde_json::json!({ "status": outcome.status, "error": rec.error }))?;

    // task error ≠ crash：done 照发，但 status 是 error——state.json 已记 error，仍如实报给调用方
    if let Some(err) = &rec.error {
        bail!("{err}");
    }

    Ok(rec)
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
