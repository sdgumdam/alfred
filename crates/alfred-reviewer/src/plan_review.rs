//!
//! 计划审查（宿主 pi 化）：宿主 pi agent 全可见（ws 产物 + 对话记录 + verdict
//! 历史 + 契约全字段经绝对路径自由读）审忠实度，产出 verdict.json 到
//! `<review_dir>/outputs/`；宿主收割做 serde 等价校验。离线（ALFRED_OFFLINE=1）
//! 不跑 pi——审查跳过 → unscored → 调用方升级属主（§六继承项，不悄悄放行）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_core::util::{now_rfc3339, short_id};
use alfred_core::verdict::PlanVerdict;
use alfred_executor::config::ExecutorModel;
use serde::Serialize;

use crate::host::{run_plan_review_on_host, ReviewerHostOptions};
use crate::verdict::parse_plan_verdict_json;

/// 计划审查选项。
#[derive(Debug, Clone)]
pub struct PlanReviewOptions {
    /// run 目录（reviewer 工作目录 = `<run>/plan-review`）。
    pub run_dir: PathBuf,
    /// 单样本时间上限（秒）。
    pub time_limit_secs: u32,
    /// 宿主 pi 驱动选项（cwd/AGT/时间上限）。计划审查一律跑宿主 pi（无配置 =
    /// 无法审查）。
    pub host: ReviewerHostOptions,
}

impl Default for PlanReviewOptions {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::new(),
            time_limit_secs: 300,
            host: ReviewerHostOptions::default(),
        }
    }
}

/// 计划审查结果（落盘 verdict.json + state.json）。
#[derive(Debug, Clone, Serialize)]
pub struct PlanReviewOutcome {
    pub run_id: String,
    pub request_id: String,
    pub reviewer_model: String,
    /// 容器驱动状态（"success" / "error" / "timed_out"；结构闸门/离线为 "skipped"）。
    /// 字段名沿用旧名 `eval_status`（state.json 兼容；现承载 driver 状态，非 eval 状态）。
    pub eval_status: String,
    /// 驱动证据路径（`<work>/driver.done.json`）。字段名沿用旧名 `eval_location`
    /// （state.json 兼容；现承载 driver done 路径，非 eval 位置）。
    pub eval_location: Option<String>,
    /// 解析出的计划审查结论（unscored 时为 None）。
    pub verdict: Option<PlanVerdict>,
    /// unscored 原因（plan_verdict_parse_failure / offline 等）。
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
///
/// A2：reason 必须属主口吻、零 schema 字段名/角色名（"任务 X 需要指定工作目录"，
/// 不出现 workspace_subdirs/执行者/可见范围/节点）——该 reason 经 `disguise_rejection`
/// 原文进伪装通道（owner.message 轮 + PlanReviewed 维护载荷），源头措辞比事后替换更稳。
pub fn missing_workspace_subdirs(dagspec: &DagSpec) -> Option<(String, String)> {
    for node in &dagspec.nodes {
        if node.sandbox.workspace_subdirs.is_empty() {
            return Some((
                node.id.clone(),
                format!("任务 {} 没说明要在哪些目录里干活，需要指定工作目录。", node.id),
            ));
        }
    }
    None
}

/// 计划审查调度：
/// - R6e(补A) 结构闸门命中（节点缺 workspace_subdirs）→ 直接判不合格打回。
/// - 离线（`ALFRED_OFFLINE=1`）→ 不跑 pi → unscored（升级属主）。
/// - 否则 → 宿主 pi reviewer（全可见读全量信息）。
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
    if offline {
        return plan_review_skipped_offline(opts, model, request, dagspec);
    }
    execute_plan_review_host(opts, model, request, dagspec, session_doc)
}

/// 离线跳过（`ALFRED_OFFLINE=1`）：不跑容器（无 docker）——计划审查跳过 →
/// unscored（升级属主，§六继承项，不悄悄放行）。
fn plan_review_skipped_offline(
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

    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
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
    let reason = "offline: 计划审查容器跳过（ALFRED_OFFLINE=1，无 docker）";
    append_audit(
        run_dir,
        "plan_review_offline_skipped",
        &serde_json::json!({ "reason": reason }),
    )?;

    let rec = PlanReviewOutcome {
        run_id,
        request_id: request.id.clone(),
        reviewer_model: model.inspect_model_id(),
        eval_status: "skipped".to_string(),
        eval_location: None,
        verdict: None,
        unscored_reason: Some(reason.to_string()),
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

/// R6e(补A)：计划审查结构闸门命中——节点缺 workspace_subdirs 声明 → 直接
/// pass=false 打回重规划（不派模型，省一次审查容器）。落 audit + state.json /
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
        // 结构闸门命中：不派模型，无容器——状态显式标 skipped（诚实，非 success/error）。
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

/// 宿主路径：宿主 pi reviewer（全可见：ws 产物 + 对话记录 + verdict 历史 + 契约
/// 全字段经绝对路径自由读）审忠实度 → 产出 `<review_dir>/outputs/verdict.json` →
/// 宿主 serde 等价校验 → 落 state.json / verdict.json / audit / llm-calls。
///
/// 失败路径显式 `bail!`（不悄悄放行）：pi 超时/退出码非零/未产出 verdict →
/// `fail_review` 落盘后上报；verdict 解析失败 → unscored 兜底
/// （`plan_verdict_parse_failure`），调用方据此升级属主。
fn execute_plan_review_host(
    opts: &PlanReviewOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    dagspec: &DagSpec,
    session_doc: Option<&SessionDoc>,
) -> Result<PlanReviewOutcome> {
    let started_at = now_rfc3339();
    let run_id = match opts.host.run_dir.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => short_id("planreview"),
    };
    let run_dir = &opts.run_dir;

    std::fs::create_dir_all(run_dir).with_context(|| format!("create run dir {}", run_dir.display()))?;
    append_audit(run_dir, "plan_review_started", &serde_json::json!({ "run_id": run_id, "request_id": request.id }))?;

    // 输入落盘：request.json + dagspec.json（P9 证据 + R3 续跑输入；宿主 pi 的
    // inputs/ 由 host.rs 在 `<work>/inputs/` 落盘，这里落 review 目录供审计/续跑）。
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(request).context("serialize OwnerRequest")?,
    )?;
    std::fs::write(
        run_dir.join("dagspec.json"),
        serde_json::to_string_pretty(dagspec).context("serialize DagSpec")?,
    )?;

    // 对话记录（reviewer 全可见，§二.8）：从治理 run 目录读；不存在 → 空。
    let gov_dir = opts
        .host
        .run_dir
        .parent()
        .context("reviewer work dir has no parent (治理 run 目录)")?;
    let ws_dir = gov_dir.join("ws");
    let conversation = alfred_core::conversation::load_conversation(gov_dir)
        .map_err(anyhow::Error::msg)
        .ok()
        .flatten();

    let out = match run_plan_review_on_host(
        &opts.host,
        model,
        request,
        dagspec,
        session_doc,
        conversation.as_ref(),
        &ws_dir,
    ) {
        Ok(out) => out,
        Err(e) => {
            let msg = format!("plan review host pi failed: {e:#}");
            fail_review(
                run_dir, request, dagspec, model, &run_id, &started_at,
                "error", None, "plan_review_host_failed", "host_failure", &msg,
            )?;
            bail!(msg);
        }
    };
    append_audit(
        run_dir,
        "plan_review_driver_done",
        &serde_json::json!({ "eval_location": out.eval_location }),
    )?;

    // serde 等价校验（verdict.rs）：宿主 pi 产出 verdict.json → PlanVerdict。
    let verdict = match parse_plan_verdict_json(&out.output_text) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[orchestrator] warn: plan verdict parse failed: {e}");
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
            "plan review host pi finished with status '{}' (location={:?})",
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
/// 容器驱动异常（timed_out/crashed/status error）与 verdict 提取失败
/// （read/parse）共用——不再用 if-let 静默吞错误。填 error 后以
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
