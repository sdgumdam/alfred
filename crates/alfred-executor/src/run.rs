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
use alfred_core::verdict::ExecVerdict;
use serde::Serialize;

use crate::artifact::{collect_artifact, snapshot_workspace};
use crate::compose_gen::{canonicalize_workspace, generate_compose, CONTAINER_WORKSPACE_DIR};
use crate::config::ExecutorModel;
use crate::driver::{archive_eval_log, extract_exec_verdict, parse_dump, poll_until_done, spawn_eval, PollOutcome};
use crate::task_gen::{generate_task_py, TaskGenParams};

/// 单次运行选项。
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// 运行目录（宿主，须位于 ~ 之下——E3）。
    pub run_dir: PathBuf,
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
    /// 执行审查结论（R2：scorer 判 C/I/P）。unscored 时为 None。
    pub verdict: Option<ExecVerdict>,
    /// 执行审查 unscored 原因（verdict_parse_failure 等）；成功判分为 None。
    pub verdict_unscored_reason: Option<String>,
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

///
/// `grader` 为执行审查 scorer 的判分模型（config roles.reviewer）；R2 起
/// 执行 eval 内嵌 ExecVerdict scorer，grader 经 `--model-role` 绑定并注入
/// 其 provider key。
pub fn execute_run(
    opts: &RunOptions,
    model: &ExecutorModel,
    grader: Option<&ExecutorModel>,
    request: &OwnerRequest,
) -> Result<RunOutcome> {
    // 沙箱档案校验：R1 只支持默认档案（无挂卷 / 无 runtime / 无依赖 / 联网拒绝）。
    // 非默认档案此前被静默忽略——按审计约束改为显式拒绝，防止"申请的约束没生效"。
    if opts.assignment.sandbox != SandboxProfile::default() {
        bail!(
            "R1 不支持非默认沙箱档案（当前 sandbox={:?}）；仅支持默认档案（volumes 空、runtime 无、packages 空、network=false）",
            opts.assignment.sandbox
        );
    }
    let started_at = now_rfc3339();
    let run_id = match opts.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("run"),
    };
    let run_dir = &opts.run_dir;
    let evals_dir = run_dir.join("evals");

    // 1) 目录与工作区（须先于 compose 生成存在）
    std::fs::create_dir_all(run_dir).with_context(|| format!("create run dir {}", run_dir.display()))?;
    let workspace_host = run_dir.join("workspace");
    std::fs::create_dir_all(&workspace_host)?;
    std::fs::create_dir_all(&evals_dir)?;
    canonicalize_workspace(&workspace_host)?;

    // 2) 执行前工作区快照（文件比对基线）
    let before = snapshot_workspace(&workspace_host)?;

    // 3) 生成 compose + task.py
    let compose_path = run_dir.join("executor.compose.yaml");
    let compose = generate_compose(&workspace_host, &opts.image)?;
    std::fs::write(&compose_path, compose)?;

    let task_py = run_dir.join("task.py");
    let py = generate_task_py(&TaskGenParams {
        compose_file: compose_path.to_string_lossy().into_owned(),
        contract_prompt: opts.assignment.contract.prompt.clone(),
        acceptance_criteria: opts.assignment.contract.acceptance_criteria.clone(),
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
    let launch = spawn_eval(&task_py, model, grader, &evals_dir, opts.time_limit_secs)?;
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
                "timed_out", None, "eval_timed_out", "eval_timed_out", &msg,
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
                "crashed", None, "eval_crashed", "eval_crashed", &msg,
            )?;
            bail!(msg);
        }
    };

    // R2: 从 dump 结构化读取执行审查结论（verdict / unscored_reason）。
    // R2Audit2 修复：归档/读取/解析任一失败都不再被 if-let 静默吞掉——落
    // audit 事件 + state.json 填 error + 以 Err 上报（§6：审查出错必须升级，
    // 不允许"出错就悄悄放行"）。第二个重复的 archive 块一并删除。
    let dump = match archive_eval_log(&outcome.location, &evals_dir) {
        Ok(dump) => {
            append_audit(
                run_dir,
                "eval_log_archived",
                &serde_json::json!({ "dump": dump }),
            )?;
            dump
        }
        Err(e) => {
            let msg = format!("archive eval log failed: {e:#}");
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "eval_log_archive_failed", "eval_log_archive_failed", &msg,
            )?;
            bail!(msg);
        }
    };
    let text = match std::fs::read_to_string(&dump) {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("read eval log dump {} failed: {e}", dump.display());
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "eval_log_read_failed", "eval_log_read_failed", &msg,
            )?;
            bail!(msg);
        }
    };
    let v = match parse_dump(&text) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("parse eval log dump {} failed: {e:#}", dump.display());
            fail_run(
                run_dir, request, opts, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "eval_log_parse_failed", "eval_log_parse_failed", &msg,
            )?;
            bail!(msg);
        }
    };
    let review = extract_exec_verdict(&v);
    let verdict = review.verdict;
    let verdict_unscored_reason = review.unscored_reason;
    if let Some(detail) = review.detail {
        eprintln!("[alfred] warn: exec verdict detail: {detail}");
    }

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
        verdict,
        verdict_unscored_reason,
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
/// R2Audit2 修复：eval 异常（timed_out/crashed）与 verdict 提取失败
/// （archive/read/parse）共用——不再用 if-let 静默吞错误。填 error 后以
/// Err 上报给调用方（§6：审查出错必须升级属主，不允许"出错就悄悄放行"）。
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
    unscored_reason: &str,
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
        verdict: None,
        verdict_unscored_reason: Some(unscored_reason.to_string()),
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
        let err = execute_run(&opts, &model, None, &request).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("R1 不支持非默认沙箱档案"),
            "expected sandbox rejection, got: {text}"
        );
    }
}
