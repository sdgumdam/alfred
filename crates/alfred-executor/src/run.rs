//! 一次执行运行的编排（R1 单节点）。
//!
//! 流程：建 run 目录 → 快照工作区 → 生成 compose + task.py → spawn
//! `inspect eval --detach` → 轮询 done → 归档 eval log → 产物采集 →
//! 写 state.json + audit.jsonl。

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
use crate::driver::{archive_eval_log, poll_until_done, spawn_eval, PollOutcome};
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
    /// 是否轮询 `inspect ctl` 观测面。
    pub ctl_enabled: bool,
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
        }
    }
}

/// 运行结果（落盘 state.json）。
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub task_id: String,
    pub executor_model: String,
    pub eval_status: String,
    pub eval_location: Option<String>,
    pub artifact: Option<Artifact>,
    pub started_at: String,
    pub finished_at: String,
    /// 失败原因（eval status error / timed_out / crashed 各自填）；成功为 None。
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

/// 校验 executor 沙箱档案（R6e）：仅允许契约声明的 `workspace_subdirs` 子集挂载
/// （M5 显式声明制）。volumes/runtime/packages/network 执行驱动尚不支持——按审计
/// 约束显式拒绝而非静默忽略，防止"申请的约束没生效"。
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
    Ok(())
}

///
/// R6d：执行 eval 只出产物——不再绑定 grader（内嵌 scorer 已移除，执行审查
/// 由 reviewer 容器承担，见 governance exec_review_step）。
pub fn execute_run(
    opts: &RunOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
) -> Result<RunOutcome> {
    // 沙箱档案校验（R6e）：executor 支持契约声明的 workspace_subdirs 子集挂载
    // （M5 显式声明制）；volumes/runtime/packages/network 显式拒绝（审计约束：
    // 防止"申请的约束没生效"）。见 validate_executor_sandbox。
    validate_executor_sandbox(&opts.assignment.sandbox)?;
    let started_at = now_rfc3339();
    let run_id = match opts.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("run"),
    };
    let run_dir = &opts.run_dir;
    let evals_dir = run_dir.join("evals");

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
    std::fs::create_dir_all(&evals_dir)?;
    canonicalize_workspace(&workspace_host)?;
    // R6e：git 基线（幂等）——executor 改动相对基线可见，reviewer 挂 ws 全量 ro 自己看 git diff。
    init_workspace_git(&workspace_host)?;

    // 2) 执行前工作区快照（文件比对基线）
    let before = snapshot_workspace(&workspace_host)?;

    // 3) 生成 compose + task.py
    let compose_path = run_dir.join("executor.compose.yaml");
    // R6e：executor 容器挂载 = workspace_subdirs 声明子集（rw），非全量 ws（M5 显式
    // 声明制；空 subdirs = 不挂 ws）。参考卷/AGT 挂载留待后续块（当前 sandbox 校验
    // 已拒绝 volumes；AGT 未接入 run 路径）。
    let mounts = ExecutorMounts {
        workspace_subdirs: opts.assignment.sandbox.workspace_subdirs.clone(),
        ..Default::default()
    };
    let compose = generate_executor_compose(&workspace_host, &opts.image, &mounts)?;
    std::fs::write(&compose_path, compose)?;
    // 注入 task.py 的 compose 路径必须绝对：inspect 相对自身解析根再拼
    // 相对路径会双拼（实测 exec-N/<相对路径> 找不到 compose）。
    let compose_abs = compose_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", compose_path.display()))?;

    let task_py = run_dir.join("task.py");
    let py = generate_task_py(&TaskGenParams {
        compose_file: compose_abs.to_string_lossy().into_owned(),
        contract_prompt: opts.assignment.contract.prompt.clone(),
        port_base: opts.port_base,
        pi_model: "inspect-bridge/inspect".to_string(),
        workspace_dir: CONTAINER_WORKSPACE_DIR.to_string(),
        sandbox_user: "root".to_string(),
        run_id: run_id.clone(),
        settle_grace_seconds: opts.settle_grace_seconds,
    })?;
    std::fs::write(&task_py, py)?;

    append_audit(run_dir, "run_started", &serde_json::json!({ "run_id": run_id, "task_id": opts.assignment.task_id }))?;

    // 4) spawn detached eval
    let launch = spawn_eval(&task_py, model, None, &evals_dir, opts.time_limit_secs)?;
    append_audit(
        run_dir,
        "eval_launched",
        &serde_json::json!({ "run_id": launch.run_id, "output_file": launch.output_file, "log_dir": launch.log_dir }),
    )?;

    // 5) 轮询（timeout = time_limit + 缓冲）
    let poll_timeout = opts.time_limit_secs as u64 + 600;
    let outcome = match poll_until_done(&launch, poll_timeout, opts.ctl_enabled)? {
        PollOutcome::Done(done) => done,
        PollOutcome::TimedOut => {
            // 失败路径填 error（P3）：state.json 落 timed_out 原因后仍以 Err 上报
            let msg = format!(
                "eval timed out after {}s (no done record in {})",
                poll_timeout,
                launch.output_file.display()
            );
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                "timed_out", None, "eval_timed_out", &msg,
            )?;
            bail!(msg);
        }
        PollOutcome::Crashed => {
            // 失败路径填 error（P3）：state.json 落 crashed 原因后仍以 Err 上报
            let msg = format!(
                "eval process died without a done record (output: {})",
                launch.output_file.display()
            );
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                "crashed", None, "eval_crashed", &msg,
            )?;
            bail!(msg);
        }
    };

    // R6d：执行 eval 只出产物无审查行为——归档 eval log（P9 证据）但不再从
    // dump 提取执行审查结论（内嵌 scorer 已移除；审查由 reviewer 容器承担，
    // 见 governance exec_review_step）。归档失败仍为硬错误（证据链完整性）。
    match archive_eval_log(&outcome.location, &evals_dir) {
        Ok(dump) => {
            append_audit(
                run_dir,
                "eval_log_archived",
                &serde_json::json!({ "dump": dump }),
            )?;
        }
        Err(e) => {
            let msg = format!("archive eval log failed: {e:#}");
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "eval_log_archive_failed", &msg,
            )?;
            bail!(msg);
        }
    };
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
        format!(
            "eval finished with status '{}' (task_id={}, location={})",
            outcome.status, outcome.task_id, outcome.location
        )
    });
    let rec = RunOutcome {
        run_id,
        task_id: opts.assignment.task_id.clone(),
        executor_model: model.inspect_model_id(),
        eval_status: outcome.status.clone(),
        eval_location: Some(outcome.location.clone()),
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
/// eval 异常（timed_out/crashed）与 eval log 归档失败共用——不再用 if-let
/// 静默吞错误。填 error 后以 Err 上报给调用方（不悄悄放行）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_run_dir_is_under_home() {
        let d = default_run_dir();
        let home = std::env::var("HOME").unwrap();
        assert!(
            d.starts_with(home),
            "run dir {} must be under HOME (E3)",
            d.display()
        );
    }

    #[test]
    fn rejects_non_default_sandbox_profile() {
        let mut opts = RunOptions::default();
        // 非默认档案：network=true（或任一字段偏离默认）→ execute_run 必须拒绝
        opts.assignment.sandbox.network = true;
        let model = ExecutorModel {
            provider: "z".into(),
            model: "m".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let request = OwnerRequest {
            id: "req-x".into(),
            title: "t".into(),
            description: "d".into(),
            acceptance_criteria: "a".into(),
            created_at: "2026-08-25T00:00:00Z".into(),
        };
        let err = execute_run(&opts, &model, &request).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("executor 沙箱档案不支持 volumes/runtime/packages/network"),
            "expected sandbox rejection, got: {text}"
        );
    }

    #[test]
    fn validate_sandbox_allows_default_and_workspace_subdirs() {
        // R6e：默认档案 + workspace_subdirs 子集挂载通过（M5 显式声明制）
        assert!(validate_executor_sandbox(&SandboxProfile::default()).is_ok());
        let mut sb = SandboxProfile::default();
        sb.workspace_subdirs = vec!["src".into(), "tests".into()];
        assert!(validate_executor_sandbox(&sb).is_ok());
    }

    #[test]
    fn validate_sandbox_rejects_unsupported_fields() {
        // R6e：volumes/runtime/packages/network 任一非默认 → 显式拒绝
        let err = validate_executor_sandbox(&SandboxProfile {
            network: true,
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("不支持 volumes/runtime/packages/network"));

        let err = validate_executor_sandbox(&SandboxProfile {
            runtime: Some("rust".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("不支持 volumes/runtime/packages/network"));

        let err = validate_executor_sandbox(&SandboxProfile {
            packages: vec!["gcc".into()],
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("不支持 volumes/runtime/packages/network"));

        let err = validate_executor_sandbox(&SandboxProfile {
            volumes: vec![alfred_core::VolumeMount {
                host_path: "/refs".into(),
                container_path: "/references".into(),
            }],
            ..Default::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("不支持 volumes/runtime/packages/network"));
    }

    #[test]
    fn resolve_workspace_dir_prefers_explicit_and_falls_back() {
        // R6e：显式 workspace_dir 优先；空（未设）回退 run_dir/workspace（旧布局）
        let run = Path::new("/runs/exec-1");
        assert_eq!(
            resolve_workspace_dir(run, Path::new("/runs/run-abc/ws")),
            Path::new("/runs/run-abc/ws")
        );
        assert_eq!(
            resolve_workspace_dir(run, Path::new("")),
            run.join("workspace")
        );
    }

    #[test]
    fn ensure_run_workspace_creates_ws_with_git_baseline() {
        // R6e：治理 run 初始化建 `<run>/ws` + git init 基线（幂等）
        let home = std::env::var("HOME").unwrap();
        let dir = Path::new(&home).join(".local/state/alfred/test-run-ws-git");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ws = ensure_run_workspace(&dir).unwrap();
        assert_eq!(ws, dir.join("ws"));
        assert!(ws.join(".git").is_dir(), "ws 应初始化为 git 仓库：{}", ws.display());

        // 幂等：二次调用不报错、不新增提交
        ensure_run_workspace(&dir).unwrap();

        // 基线空提交存在
        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&ws)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&log.stdout);
        assert!(text.contains("R6e baseline"), "基线空提交缺失：{text}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn init_workspace_git_baseline_makes_executor_changes_visible() {
        // R6e：git init + 空提交基线后，executor 新建文件在 git status 可见
        //（reviewer 挂 ws 全量 ro 用只读 git 命令对照基线看 diff）。
        let home = std::env::var("HOME").unwrap();
        let dir = Path::new(&home).join(".local/state/alfred/test-ws-git-diff");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        init_workspace_git(&dir).unwrap();
        // 幂等：.git 已存在 → 直接返回（不重复 commit）
        init_workspace_git(&dir).unwrap();

        // executor 改动（未提交新建文件）
        std::fs::write(dir.join("hello.txt"), "Hello").unwrap();

        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(
            status.contains("hello.txt"),
            "executor 改动应相对基线可见（git status）：{status}"
        );
        // 基线仍是空提交（改动未提交——reviewer 看未提交文件）
        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let commits = String::from_utf8_lossy(&log.stdout);
        assert_eq!(commits.lines().count(), 1, "仅基线一个提交：{commits}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
