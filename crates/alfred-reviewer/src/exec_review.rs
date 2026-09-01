//! 执行审查独立容器（R6c）：eval 内嵌 grader → 独立 reviewer 容器。
//!
//! 流程：建 exec-review run 目录 → 写 request.json/contract.json（证据）→
//! reviewer 容器（挂 ws 全量 ro + 契约全字段 + 对话记录）内 pi 读全量产物判
//! 分 → 产出 verdict.json 到 /outputs → 宿主 Pydantic 等价校验 → 落
//! verdict.json + state.json + audit.jsonl。
//!
//! 关键差异（vs 旧投影 grader）：旧 grader 只见 `Sample.target`（验收标准 + 产物
//! 摘要，截断 80KB/200 文件）；本路径 reviewer 挂 **ws 全量 ro**，自己读 ws diff /
//! git 历史 / 隐藏目录判产物——审查者看全量信息防合谋（R6c 验证核心：夹带私货
//! 用例必须被全量 reviewer 抓到）。
//!
//! 治理环接入：R6c 保留 executor eval 内嵌 scorer（R6d 移除）；本驱动为独立
//! 容器执行审查路径（`alfred exec-review` 子命令 + 后续治理环接入的落点）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::contract::Contract;
use alfred_core::governance::GovernanceOptions;
use alfred_core::request::OwnerRequest;
use alfred_core::util::{now_rfc3339, short_id};
use alfred_core::verdict::ExecVerdict;
use alfred_executor::config::ExecutorModel;
use serde::Serialize;

use crate::container::{run_exec_review_in_container, ReviewerContainerOptions};
use crate::verdict::parse_exec_verdict_json;

/// 执行审查选项（R6c）。
#[derive(Debug, Clone)]
pub struct ExecReviewOptions {
    /// reviewer 容器驱动选项（`run_dir` = exec-review 工作目录，须位于 ~ 之下）。
    pub container: ReviewerContainerOptions,
    /// ws 全量 ro 挂载源（持久 ws 目录）。
    pub ws_dir: PathBuf,
}

impl ExecReviewOptions {
    /// 从治理环运行选项派生（`exec_review_dir` = `<run>/exec-review`，`ws_dir` = `<run>/ws`）。
    pub fn from_governance(exec_review_dir: PathBuf, ws_dir: PathBuf, opts: &GovernanceOptions) -> Self {
        Self {
            container: ReviewerContainerOptions::from_governance(exec_review_dir, opts),
            ws_dir,
        }
    }
}

/// 执行审查结果（落盘 verdict.json + state.json）。
#[derive(Debug, Clone, Serialize)]
pub struct ExecReviewOutcome {
    pub run_id: String,
    pub request_id: String,
    pub reviewer_model: String,
    pub eval_status: String,
    pub eval_location: Option<String>,
    /// 解析出的执行审查结论（unscored 时为 None）。
    pub verdict: Option<ExecVerdict>,
    /// unscored 原因（verdict_parse_failure 等）。
    pub unscored_reason: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub error: Option<String>,
}

/// run 目录缺省基座（`ALFRED_STATE_DIR` 或 `~/.local/state/alfred/runs`）。
pub fn default_exec_review_dir() -> PathBuf {
    let base = std::env::var("ALFRED_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            PathBuf::from(home).join(".local/state/alfred/runs")
        });
    base.join(short_id("execreview"))
}

/// 执行审查容器驱动：request + 契约全字段 + 对话记录 + ws 全量 ro → ExecVerdict。
///
/// 失败路径显式 `bail!`（不悄悄放行）：容器驱动失败 → `fail_exec_review` 落盘后
/// 上报；verdict 解析失败 → unscored 兜底（调用方据此升级属主）。
pub fn execute_exec_review(
    opts: &ExecReviewOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    contract: &Contract,
    workspace_subdirs: &[String],
    conversation: Option<&alfred_core::conversation::ConversationLog>,
) -> Result<ExecReviewOutcome> {
    let started_at = now_rfc3339();
    let run_id = match opts.container.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("execreview"),
    };
    let run_dir = &opts.container.run_dir;

    std::fs::create_dir_all(run_dir).with_context(|| format!("create exec review dir {}", run_dir.display()))?;
    append_audit(run_dir, "exec_review_started", &serde_json::json!({ "run_id": run_id, "request_id": request.id }))?;

    // 输入落盘（P9 证据）：request + 契约全字段
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(request).context("serialize OwnerRequest")?,
    )?;
    std::fs::write(
        run_dir.join("contract.json"),
        serde_json::to_string_pretty(contract).context("serialize Contract")?,
    )?;

    let out = match run_exec_review_in_container(
        &opts.container,
        model,
        request,
        contract,
        workspace_subdirs,
        conversation,
        &opts.ws_dir,
    ) {
        Ok(out) => out,
        Err(e) => {
            let msg = format!("exec review container failed: {e:#}");
            fail_exec_review(run_dir, request, model, &run_id, &started_at, "error", None, "exec_review_container_failed", "container_failure", &msg)?;
            bail!(msg);
        }
    };
    append_audit(
        run_dir,
        "exec_review_eval_launched",
        &serde_json::json!({ "eval_location": out.eval_location }),
    )?;

    // Pydantic 等价校验（verdict.rs）：容器产出 verdict.json → ExecVerdict。
    let verdict = match parse_exec_verdict_json(&out.output_text) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[alfred] warn: exec verdict parse failed: {e}");
            None
        }
    };
    let unscored_reason = if verdict.is_none() {
        Some("verdict_parse_failure".to_string())
    } else {
        None
    };
    let eval_error = (out.eval_status != "success").then(|| {
        format!(
            "exec review container eval finished with status '{}' (location={:?})",
            out.eval_status, out.eval_location
        )
    });
    let rec = ExecReviewOutcome {
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
    write_state(run_dir, request, contract, &rec)?;
    append_audit(
        run_dir,
        "exec_review_finished",
        &serde_json::json!({ "status": out.eval_status, "verdict": rec.verdict, "error": rec.error }),
    )?;

    if let Some(err) = &rec.error {
        bail!("{err}");
    }
    Ok(rec)
}

/// 失败路径统一构造 ExecReviewOutcome + 落 audit + 落盘 state.json/verdict.json
/// （不悄悄放行）。
#[allow(clippy::too_many_arguments)]
fn fail_exec_review(
    run_dir: &Path,
    request: &OwnerRequest,
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
    let rec = ExecReviewOutcome {
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
    write_state(run_dir, request, &empty_contract(), &rec)?;
    Ok(())
}
/// 失败路径占位契约（state.json 契约字段的兜底；失败时无真实契约可写）。
fn empty_contract() -> Contract {
    Contract {
        prompt: String::new(),
        acceptance_criteria: String::new(),
        reviewer_models: Vec::new(),
    }
}

#[derive(Serialize)]
struct StateFile {
    exec_review: ExecReviewOutcome,
    request: OwnerRequest,
    contract: Contract,
}

fn write_state(
    run_dir: &Path,
    request: &OwnerRequest,
    contract: &Contract,
    rec: &ExecReviewOutcome,
) -> Result<()> {
    let state = StateFile {
        exec_review: rec.clone(),
        request: request.clone(),
        contract: contract.clone(),
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
