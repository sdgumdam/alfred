//!
//! R6c：新增容器路径（`PlanReviewOptions.container`）——reviewer 容器（ws 全量
//! ro + 对话记录 + 契约全字段）内 pi 读全量信息审忠实度，产出 verdict.json 到
//! /outputs；宿主读容器产出做 Pydantic 等价校验。离线回归（ALFRED_OFFLINE=1）
//! 与独立 `alfred plan-review`（container=None）保留旧 eval 直判路径。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_core::util::{now_rfc3339, short_id};
use alfred_core::verdict::PlanVerdict;
use alfred_executor::config::ExecutorModel;
use alfred_executor::driver::{
    archive_eval_log, parse_dump, poll_until_done, sample_score, spawn_eval, PollOutcome,
};
use serde::Serialize;
use serde_json::Value;

use crate::container::{run_plan_review_in_container, ReviewerContainerOptions};
use crate::task_gen::generate_plan_review_py;
use crate::verdict::parse_plan_verdict_json;

/// 计划审查选项。
#[derive(Debug, Clone)]
pub struct PlanReviewOptions {
    /// run 目录（须位于 ~ 之下——E3）。
    pub run_dir: PathBuf,
    /// 单样本时间上限（秒）。
    pub time_limit_secs: u32,
    /// 是否轮询 `inspect ctl` 观测面。
    pub ctl_enabled: bool,
    /// R6c：reviewer 容器驱动选项。Some = 非离线时走容器（ws 全量 ro + 对话记录）；
    /// None = 走旧 eval 直判（独立 plan-review / 离线回归）。
    pub container: Option<ReviewerContainerOptions>,
}

impl Default for PlanReviewOptions {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::new(),
            time_limit_secs: 300,
            ctl_enabled: true,
            container: None,
        }
    }
}

/// 计划审查结果（落盘 verdict.json + state.json）。
#[derive(Debug, Clone, Serialize)]
pub struct PlanReviewOutcome {
    pub run_id: String,
    pub request_id: String,
    pub reviewer_model: String,
    pub eval_status: String,
    pub eval_location: Option<String>,
    /// 解析出的计划审查结论（unscored 时为 None）。
    pub verdict: Option<PlanVerdict>,
    /// unscored 原因（plan_verdict_parse_failure 等）。
    pub unscored_reason: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub error: Option<String>,
}

/// run 目录缺省基座（`ALFRED_STATE_DIR` 或 `~/.local/state/alfred/runs`）。
pub fn default_review_dir() -> PathBuf {
    let base = std::env::var("ALFRED_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            PathBuf::from(home).join(".local/state/alfred/runs")
        });
    base.join(short_id("planreview"))
}

/// R6e(补A)：计划审查强制 workspace_subdirs 声明——每个执行节点必须有非空
/// `workspace_subdirs`（属主原话："执行者只能看到 ws 中的部分内容"，workspace_subdirs
/// 是执行节点的强制约束）。缺失/为空 → 计划不合格 → 打回重规划（空声明在计划层就
/// 被拦，executor 永远拿不到空挂载）。
///
/// 返回 `(节点 id, verdict.reason 用原因)`；None = 所有节点都已声明。
pub fn missing_workspace_subdirs(dagspec: &DagSpec) -> Option<(String, String)> {
    for node in &dagspec.nodes {
        if node.sandbox.workspace_subdirs.is_empty() {
            return Some((
                node.id.clone(),
                format!("节点 {} 未声明 workspace_subdirs，执行者无法获知可见范围", node.id),
            ));
        }
    }
    None
}

///
/// R6c 调度：`container` 为 Some 且非离线（`ALFRED_OFFLINE` 未设）→ 容器路径
/// （reviewer 容器读全量信息）；否则 → 旧 eval 直判路径（离线回归 / 独立子命令）。
pub fn execute_plan_review(
    opts: &PlanReviewOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    dagspec: &DagSpec,
    session_doc: Option<&SessionDoc>,
) -> Result<PlanReviewOutcome> {
    // R6e(补A)：计划审查结构闸门——任一执行节点缺/空 workspace_subdirs 声明 →
    // 直接判不合格（pass=false）打回重规划，不派模型。空声明在计划层就被拦，
    // executor 永远拿不到空挂载（属主："执行者只能看到 ws 中的部分内容"）。
    if let Some((node_id, reason)) = missing_workspace_subdirs(dagspec) {
        return reject_missing_workspace_subdirs(
            opts, model, request, dagspec, &node_id, &reason,
        );
    }
    let offline = std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1");
    match &opts.container {
        Some(container) if !offline => execute_plan_review_container(
            opts, container, model, request, dagspec, session_doc
        ),
        _ => execute_plan_review_eval(opts, model, request, dagspec, session_doc),
    }
}

/// R6e(补A)：计划审查结构闸门命中——节点缺 workspace_subdirs 声明 → 直接
/// pass=false 打回重规划（不派模型，省一次审查 eval）。落 audit + state.json /
/// verdict.json，调用方（治理环）读 `verdict` 判 PlanReviewRejected → 回退重规划。
fn reject_missing_workspace_subdirs(
    opts: &PlanReviewOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    dagspec: &DagSpec,
    node_id: &str,
    reason: &str,
) -> Result<PlanReviewOutcome> {
    let started_at = now_rfc3339();
    let run_id = match opts.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("planreview"),
    };
    let run_dir = &opts.run_dir;

    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;

    // 输入落盘（P9 证据 + R3 续跑输入）。
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(request).context("serialize OwnerRequest")?,
    )?;
    std::fs::write(
        run_dir.join("dagspec.json"),
        serde_json::to_string_pretty(dagspec).context("serialize DagSpec")?,
    )?;

    append_audit(
        run_dir,
        "plan_review_started",
        &serde_json::json!({ "run_id": run_id, "request_id": request.id }),
    )?;
    append_audit(
        run_dir,
        "plan_review_rejected_missing_workspace_subdirs",
        &serde_json::json!({ "node_id": node_id, "reason": reason }),
    )?;

    let rec = PlanReviewOutcome {
        run_id,
        request_id: request.id.clone(),
        reviewer_model: model.inspect_model_id(),
        // 结构闸门命中：不派模型，无 eval——状态显式标 skipped（诚实，非 success/error）。
        eval_status: "skipped".to_string(),
        eval_location: None,
        verdict: Some(PlanVerdict::new(false, reason)),
        unscored_reason: None,
        started_at,
        finished_at: now_rfc3339(),
        error: None,
    };
    write_state(run_dir, request, dagspec, &rec)?;
    append_audit(
        run_dir,
        "plan_review_finished",
        &serde_json::json!({ "status": "skipped", "verdict": rec.verdict, "error": rec.error }),
    )?;
    Ok(rec)
}

/// 旧 eval 直判路径（离线回归 / 独立 plan-review）。
fn execute_plan_review_eval(
    opts: &PlanReviewOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    dagspec: &DagSpec,
    session_doc: Option<&SessionDoc>,
) -> Result<PlanReviewOutcome> {
    let started_at = now_rfc3339();
    let run_id = match opts.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("planreview"),
    };
    let run_dir = &opts.run_dir;
    let evals_dir = run_dir.join("evals");

    std::fs::create_dir_all(run_dir).with_context(|| format!("create run dir {}", run_dir.display()))?;
    std::fs::create_dir_all(&evals_dir)?;

    // 输入落盘：request.json + dagspec.json（P9 证据 + R3 续跑输入）
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(request).context("serialize OwnerRequest")?,
    )?;
    std::fs::write(
        run_dir.join("dagspec.json"),
        serde_json::to_string_pretty(dagspec).context("serialize DagSpec")?,
    )?;

    let task_py = run_dir.join("plan_review.py");
    let py = generate_plan_review_py(request, dagspec, session_doc)?;
    std::fs::write(&task_py, py)?;

    append_audit(run_dir, "plan_review_started", &serde_json::json!({ "run_id": run_id, "request_id": request.id }))?;

    let launch = spawn_eval(&task_py, model, None, &evals_dir, opts.time_limit_secs)?;
    append_audit(
        run_dir,
        "plan_review_eval_launched",
        &serde_json::json!({ "run_id": launch.run_id, "output_file": launch.output_file }),
    )?;

    let poll_timeout = opts.time_limit_secs as u64 + 300;
    let outcome = match poll_until_done(&launch, poll_timeout, opts.ctl_enabled)? {
        PollOutcome::Done(done) => done,
        PollOutcome::TimedOut => {
            let msg = format!("plan review eval timed out after {poll_timeout}s");
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                "timed_out", None, "plan_review_timed_out", "eval_timed_out", &msg,
            )?;
            bail!(msg);
        }
        PollOutcome::Crashed => {
            let msg = format!(
                "plan review eval process died without done record (output: {})",
                launch.output_file.display()
            );
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                "crashed", None, "plan_review_crashed", "eval_crashed", &msg,
            )?;
            bail!(msg);
        }
    };

    // 归档 eval log + 从 dump 读 PlanVerdict。
    // R2Audit2 修复：归档/读取/解析任一失败都不再被 if-let 静默吞掉——落
    // audit 事件 + state.json 填 error + 以 Err 上报（§6：审查出错必须升级，
    // 不允许"出错就悄悄放行"）。
    let dump = match archive_eval_log(&outcome.location, &evals_dir) {
        Ok(dump) => {
            append_audit(run_dir, "plan_review_eval_archived", &serde_json::json!({ "dump": dump }))?;
            dump
        }
        Err(e) => {
            let msg = format!("archive plan review eval log failed: {e:#}");
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "plan_review_archive_failed", "eval_log_archive_failed", &msg,
            )?;
            bail!(msg);
        }
    };
    let text = match std::fs::read_to_string(&dump) {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("read plan review eval log dump {} failed: {e}", dump.display());
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "plan_review_log_read_failed", "eval_log_read_failed", &msg,
            )?;
            bail!(msg);
        }
    };
    let v = match parse_dump(&text) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("parse plan review eval log dump {} failed: {e:#}", dump.display());
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                &outcome.status, Some(&outcome.location),
                "plan_review_log_parse_failed", "eval_log_parse_failed", &msg,
            )?;
            bail!(msg);
        }
    };
    let out = extract_plan_verdict(&v);
    let verdict = out.verdict;
    let unscored_reason = out.unscored_reason;

    let eval_error = (outcome.status != "success").then(|| {
        format!(
            "plan review eval finished with status '{}' (location={})",
            outcome.status, outcome.location
        )
    });
    let rec = PlanReviewOutcome {
        run_id,
        request_id: request.id.clone(),
        reviewer_model: model.inspect_model_id(),
        eval_status: outcome.status.clone(),
        eval_location: Some(outcome.location.clone()),
        verdict,
        unscored_reason,
        started_at,
        finished_at: now_rfc3339(),
        error: eval_error,
    };
    write_state(run_dir, request, dagspec, &rec)?;
    append_audit(
        run_dir,
        "plan_review_finished",
        &serde_json::json!({ "status": outcome.status, "verdict": rec.verdict, "error": rec.error }),
    )?;

    if let Some(err) = &rec.error {
        bail!("{err}");
    }
    Ok(rec)
}

/// 失败路径统一构造 PlanReviewOutcome + 落 audit + 落盘 state.json/verdict.json
/// R6c 容器路径：reviewer 容器（ws 全量 ro + 对话记录 + 契约全字段）内 pi 读
/// 全量信息审忠实度 → 产出 verdict.json → 宿主 Pydantic 等价校验 → 落
/// state.json / verdict.json / audit。
///
/// 失败路径显式 `bail!`（不悄悄放行）：容器驱动失败（eval 超时/crash/status
/// error/未产出 verdict）→ `fail_review` 落盘后上报；verdict 解析失败 → unscored
/// 兜底（`plan_verdict_parse_failure`），调用方据此升级属主。
fn execute_plan_review_container(
    opts: &PlanReviewOptions,
    container: &ReviewerContainerOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    dagspec: &DagSpec,
    session_doc: Option<&SessionDoc>,
) -> Result<PlanReviewOutcome> {
    let started_at = now_rfc3339();
    let run_id = match container.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("planreview"),
    };
    let run_dir = &opts.run_dir;

    std::fs::create_dir_all(run_dir).with_context(|| format!("create run dir {}", run_dir.display()))?;
    append_audit(run_dir, "plan_review_started", &serde_json::json!({ "run_id": run_id, "request_id": request.id }))?;

    // 输入落盘：request.json + dagspec.json（P9 证据 + R3 续跑输入；容器输入由
    // container.rs 在 `<work>/inputs/` 落盘，这里落 run 目录供审计/续跑）。
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(request).context("serialize OwnerRequest")?,
    )?;
    std::fs::write(
        run_dir.join("dagspec.json"),
        serde_json::to_string_pretty(dagspec).context("serialize DagSpec")?,
    )?;

    // 对话记录（reviewer 独有挂载，§二.8）：从治理 run 目录读；不存在 → 空。
    let gov_dir = container
        .run_dir
        .parent()
        .context("reviewer work dir has no parent (治理 run 目录)")?;
    let ws_dir = gov_dir.join("ws");
    let conversation = alfred_core::conversation::load_conversation(gov_dir)
        .map_err(anyhow::Error::msg)
        .ok()
        .flatten();

    let out = match run_plan_review_in_container(
        container,
        model,
        request,
        dagspec,
        session_doc,
        conversation.as_ref(),
        &ws_dir,
    ) {
        Ok(out) => out,
        Err(e) => {
            let msg = format!("plan review container failed: {e:#}");
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                "error", None, "plan_review_container_failed", "container_failure", &msg,
            )?;
            bail!(msg);
        }
    };
    append_audit(
        run_dir,
        "plan_review_eval_launched",
        &serde_json::json!({ "eval_location": out.eval_location }),
    )?;

    // Pydantic 等价校验（verdict.rs）：容器产出 verdict.json → PlanVerdict。
    let verdict = match parse_plan_verdict_json(&out.output_text) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[alfred] warn: plan verdict parse failed: {e}");
            None
        }
    };
    let unscored_reason = if verdict.is_none() {
        Some("plan_verdict_parse_failure".to_string())
    } else {
        None
    };
    let eval_error = (out.eval_status != "success").then(|| {
        format!(
            "plan review container eval finished with status '{}' (location={:?})",
            out.eval_status, out.eval_location
        )
    });
    let rec = PlanReviewOutcome {
        run_id,
        request_id: request.id.clone(),
        reviewer_model: model.inspect_model_id(),
        eval_status: out.eval_status.clone(),
        eval_location: out.eval_location.clone(),
        verdict,
        unscored_reason,
        started_at,
        finished_at: now_rfc3339(),
        error: eval_error,
    };
    write_state(run_dir, request, dagspec, &rec)?;
    append_audit(
        run_dir,
        "plan_review_finished",
        &serde_json::json!({ "status": out.eval_status, "verdict": rec.verdict, "error": rec.error }),
    )?;

    if let Some(err) = &rec.error {
        bail!("{err}");
    }
    Ok(rec)
}

/// 失败路径统一构造 PlanReviewOutcome + 落 audit + 落盘 state.json/verdict.json
/// （不悄悄放行）。
///
/// R2Audit2 修复：eval 异常（timed_out/crashed）与 verdict 提取失败
/// （archive/read/parse）共用——不再用 if-let 静默吞错误。填 error 后以
/// Err 上报给调用方（§6：审查出错必须升级属主，不允许"出错就悄悄放行"）。
#[allow(clippy::too_many_arguments)]
fn fail_review(
    run_dir: &Path,
    request: &OwnerRequest,
    dagspec: &DagSpec,
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
    let rec = PlanReviewOutcome {
        run_id: run_id.to_string(),
        request_id: request.id.clone(),
        reviewer_model: model.inspect_model_id(),
        eval_status: eval_status.to_string(),
        eval_location: eval_location.map(String::from),
        verdict: None,
        unscored_reason: Some(unscored_reason.to_string()),
        started_at: started_at.to_string(),
        finished_at: now_rfc3339(),
        error: Some(msg.to_string()),
    };
    write_state(run_dir, request, dagspec, &rec)?;
    Ok(())
}

/// 从 dump JSON 提取计划审查结论。
pub struct PlanVerdictExtraction {
    pub verdict: Option<PlanVerdict>,
    pub unscored_reason: Option<String>,
}

pub fn extract_plan_verdict(dump: &Value) -> PlanVerdictExtraction {
    let score = match sample_score(dump, "plan_verdict_scorer") {
        Some(s) => s,
        None => {
            return PlanVerdictExtraction {
                verdict: None,
                unscored_reason: Some("plan_verdict_scorer_missing".into()),
            }
        }
    };
    let value = score.get("value").and_then(|v| v.as_str());
    let reason = score
        .get("explanation")
        .and_then(|e| e.as_str())
        .map(String::from)
        .or_else(|| {
            score
                .get("metadata")
                .and_then(|m| m.get("reason"))
                .and_then(|r| r.as_str())
                .map(String::from)
        })
        .unwrap_or_default();
    match value {
        Some("pass") => PlanVerdictExtraction {
            verdict: Some(PlanVerdict::new(true, reason)),
            unscored_reason: None,
        },
        Some("fail") => PlanVerdictExtraction {
            verdict: Some(PlanVerdict::new(false, reason)),
            unscored_reason: None,
        },
        _ => PlanVerdictExtraction {
            verdict: None,
            unscored_reason: score
                .get("metadata")
                .and_then(|m| m.get("unscored_reason"))
                .and_then(|u| u.as_str())
                .map(String::from)
                .or_else(|| Some("unscored".into())),
        },
    }
}

#[derive(Serialize)]
struct StateFile {
    plan_review: PlanReviewOutcome,
    request: OwnerRequest,
    dagspec: DagSpec,
}

fn write_state(
    run_dir: &Path,
    request: &OwnerRequest,
    dagspec: &DagSpec,
    rec: &PlanReviewOutcome,
) -> Result<()> {
    let state = StateFile {
        plan_review: rec.clone(),
        request: request.clone(),
        dagspec: dagspec.clone(),
    };
    let text = serde_json::to_string_pretty(&state).context("serialize state.json")?;
    std::fs::write(run_dir.join("state.json"), text).context("write state.json")?;

    let verdict_path = run_dir.join("verdict.json");
    let verdict_doc = serde_json::json!({
        "run_id": rec.run_id,
        "request_id": rec.request_id,
        "verdict": rec.verdict,
        "unscored_reason": rec.unscored_reason,
    });
    std::fs::write(
        verdict_path,
        serde_json::to_string_pretty(&verdict_doc).context("serialize verdict.json")?,
    )
    .context("write verdict.json")
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
    fn extract_plan_verdict_parses_pass() {
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "plan_verdict_scorer": {
                        "value": "pass",
                        "explanation": "plan covers all requirements",
                        "metadata": {"pass": true, "reason": "plan covers all requirements"}
                    }
                }
            }]
        }"#).unwrap();
        let out = extract_plan_verdict(&dump);
        let v = out.verdict.unwrap();
        assert!(v.pass);
        assert!(v.reason.contains("covers all"));
        assert_eq!(out.unscored_reason, None);
    }

    #[test]
    fn extract_plan_verdict_parses_fail() {
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "plan_verdict_scorer": {
                        "value": "fail",
                        "explanation": "plan does task B, request asks A",
                        "metadata": {"pass": false, "reason": "plan does task B"}
                    }
                }
            }]
        }"#).unwrap();
        let out = extract_plan_verdict(&dump);
        let v = out.verdict.unwrap();
        assert!(!v.pass);
    }

    #[test]
    fn plan_review_options_default_has_no_container() {
        // R6c：缺省走旧 eval 路径（独立 plan-review / 离线回归）
        let opts = PlanReviewOptions::default();
        assert!(opts.container.is_none());
    }

    // ---- R6e(补A)：计划审查强制 workspace_subdirs 声明 ----

    fn node_with_subdirs(id: &str, subdirs: &[&str]) -> alfred_core::dagspec::PlanNode {
        alfred_core::dagspec::PlanNode {
            id: id.into(),
            summary: "task".into(),
            contract: alfred_core::contract::Contract {
                prompt: "do the thing".into(),
                acceptance_criteria: "thing done".into(),
                reviewer_models: vec![],
            },
            sandbox: alfred_core::contract::SandboxProfile {
                workspace_subdirs: subdirs.iter().map(|s| s.to_string()).collect(),
                ..alfred_core::contract::SandboxProfile::default()
            },
        }
    }

    fn dag(nodes: Vec<alfred_core::dagspec::PlanNode>) -> DagSpec {
        DagSpec::new("req-1", nodes)
    }

    fn sample_model() -> ExecutorModel {
        ExecutorModel {
            provider: "test".into(),
            model: "test-model".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            max_tokens: 1024,
            raw_id: true,
        }
    }

    #[test]
    fn missing_workspace_subdirs_detects_empty_declaration() {
        // 节点缺/空 workspace_subdirs → 命中，reason 点名节点 id
        let d = dag(vec![node_with_subdirs("task-1", &[])]);
        let hit = missing_workspace_subdirs(&d);
        assert_eq!(hit.as_ref().map(|(id, _)| id.as_str()), Some("task-1"));
        let reason = hit.unwrap().1;
        assert!(
            reason.contains("task-1") && reason.contains("workspace_subdirs"),
            "reason 应点名节点并说明缺失：{reason}"
        );
        assert!(reason.contains("执行者无法获知可见范围"), "reason 应说明后果：{reason}");
    }

    #[test]
    fn missing_workspace_subdirs_allows_declared_nodes() {
        // 所有节点都声明非空 workspace_subdirs → 放行
        let d = dag(vec![
            node_with_subdirs("task-1", &["src"]),
            node_with_subdirs("task-2", &["tests", "docs"]),
        ]);
        assert!(missing_workspace_subdirs(&d).is_none());
    }

    #[test]
    fn missing_workspace_subdirs_detects_first_missing() {
        // 多节点：返回第一个缺声明的节点
        let d = dag(vec![
            node_with_subdirs("task-1", &["src"]),
            node_with_subdirs("task-2", &[]),
            node_with_subdirs("task-3", &[]),
        ]);
        let hit = missing_workspace_subdirs(&d);
        assert_eq!(hit.as_ref().map(|(id, _)| id.as_str()), Some("task-2"));
    }

    fn home_run_dir(tag: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap();
        let dir = Path::new(&home).join(format!(".local/state/alfred/test-plan-review-r6e-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn execute_plan_review_rejects_missing_workspace_subdirs() {
        // 计划审查结构闸门：节点缺 workspace_subdirs → 不派模型，直接 pass=false
        // 打回重规划（PlanReviewRejected → 回退 planner）。verdict/state/audit 落盘。
        let run_dir = home_run_dir("reject");
        let opts = PlanReviewOptions {
            run_dir: run_dir.clone(),
            time_limit_secs: 300,
            ctl_enabled: false,
            container: None,
        };
        let req = OwnerRequest::new("req-r6e-a", "t", "d", "a");
        let d = dag(vec![node_with_subdirs("task-1", &[])]);
        let out = execute_plan_review(&opts, &sample_model(), &req, &d, None).unwrap();

        // 结论：pass=false + reason 点名节点（不派模型：eval_status=skipped）
        assert_eq!(out.eval_status, "skipped");
        let v = out.verdict.expect("结构闸门必须产出 verdict");
        assert!(!v.pass, "缺 workspace_subdirs 的计划必须打回");
        assert!(
            v.reason.contains("task-1") && v.reason.contains("workspace_subdirs"),
            "reason 应点名节点：{}",
            v.reason
        );
        assert_eq!(out.unscored_reason, None);

        // 落盘证据：verdict.json（pass=false）+ state.json + audit.jsonl（拒绝事件）
        let verdict_text = std::fs::read_to_string(run_dir.join("verdict.json")).unwrap();
        let verdict_json: Value = serde_json::from_str(&verdict_text).unwrap();
        assert_eq!(verdict_json["verdict"]["pass"], false);
        let audit_text = std::fs::read_to_string(run_dir.join("audit.jsonl")).unwrap();
        assert!(
            audit_text.contains("plan_review_rejected_missing_workspace_subdirs"),
            "audit 应记录结构闸门拒绝事件"
        );
        assert!(run_dir.join("state.json").exists());
        assert!(run_dir.join("request.json").exists());
        assert!(run_dir.join("dagspec.json").exists());
        std::fs::remove_dir_all(&run_dir).ok();
    }
}
