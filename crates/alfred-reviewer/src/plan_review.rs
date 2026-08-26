//! 计划审查编排（R2）：独立 eval 判 DagSpec vs OwnerRequest 忠实度。
//!
//! 流程：建 run 目录 → 写 request.json/dagspec.json → 生成 plan_review.py →
//! spawn `inspect eval --detach`（主模型 = 审查者）→ 轮询 done → 归档 eval
//! log → 从 dump 结构化读取 PlanVerdict → 落 verdict.json + state.json +
//! audit.jsonl。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::util::{now_rfc3339, short_id};
use alfred_core::verdict::PlanVerdict;
use alfred_executor::config::ExecutorModel;
use alfred_executor::driver::{
    archive_eval_log, parse_dump, poll_until_done, sample_score, spawn_eval, PollOutcome,
};
use serde::Serialize;
use serde_json::Value;

use crate::task_gen::generate_plan_review_py;

/// 计划审查选项。
#[derive(Debug, Clone)]
pub struct PlanReviewOptions {
    /// run 目录（须位于 ~ 之下——E3）。
    pub run_dir: PathBuf,
    /// 单样本时间上限（秒）。
    pub time_limit_secs: u32,
    /// 是否轮询 `inspect ctl` 观测面。
    pub ctl_enabled: bool,
}

impl Default for PlanReviewOptions {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::new(),
            time_limit_secs: 300,
            ctl_enabled: true,
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

/// 执行一次计划审查（同步阻塞直到 eval 结束）。
pub fn execute_plan_review(
    opts: &PlanReviewOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    dagspec: &DagSpec,
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
    let py = generate_plan_review_py(request, dagspec)?;
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
    fn extract_plan_verdict_marks_unscored() {
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "plan_verdict_scorer": {
                        "value": null,
                        "explanation": "NOT JSON",
                        "metadata": {"unscored_reason": "plan_verdict_parse_failure"}
                    }
                }
            }]
        }"#).unwrap();
        let out = extract_plan_verdict(&dump);
        assert!(out.verdict.is_none());
        assert_eq!(out.unscored_reason.as_deref(), Some("plan_verdict_parse_failure"));
    }
}
