//! Inspect 驱动层：spawn `inspect eval --detach --json` → 轮询 output_file
//! done 记录 → `inspect log dump` 取证据。
//!
//! CLI 观测契约（调研文档 + R0报告）：
//! - launch 记录: {event,run_id,pid,log_dir,control.socket_path,output_file}
//! - 完成判定: output_file 末行 done 记录；进程消失无 done = crash
//! - task error ≠ crash: done 照发、退出码 0——分支看 logs[].status 不看退出码！

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use alfred_core::verdict::{Confidence, ExecVerdict, FailureClass, VerdictGrade};
use serde_json::Value;

use crate::config::ExecutorModel;

/// inspect CLI 解析：`ALFRED_INSPECT` 环境变量优先，否则 PATH 上的 `inspect`。
pub fn inspect_binary() -> String {
    std::env::var("ALFRED_INSPECT").unwrap_or_else(|_| "inspect".to_string())
}

/// `inspect eval --detach` 的 launch 记录。
#[derive(Debug, Clone)]
pub struct LaunchRecord {
    pub run_id: String,
    pub pid: Option<i64>,
    pub log_dir: Option<String>,
    /// 分离进程的输出文件（含 done 记录）。
    pub output_file: PathBuf,
}

/// done 记录（logs[0] 为主任务）。
#[derive(Debug, Clone)]
pub struct EvalDone {
    pub task: String,
    pub task_id: String,
    pub eval_id: String,
    /// "success" / "error"（task error ≠ crash，看 status 不看退出码）
    pub status: String,
    /// .eval 文件绝对路径
    pub location: String,
}

/// 轮询结果。
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// 读到 done 记录。
    Done(EvalDone),
    /// 超时（未 done、进程仍活）。
    TimedOut,
    /// 进程消失且无 done（crash）。
    Crashed,
}

/// 只注入 executor/reviewer 两个 provider 凭据（经 env：`{PROVIDER}_API_KEY`
/// + `ALFRED_EXEC_API_KEY`，不进 argv），子进程 env 做 env_clear + 白名单
/// 清洗（R0 审计约束 1：防执行者经桥点名其他 provider）。
///
/// `grader` 为执行审查 scorer 的判分模型（config roles.reviewer）：经
/// `--model-role grader=<id>` 绑定，且其 provider key 一并注入 eval 进程。
/// 计划审查 eval（scorer 判忠实度）不传 grader——主模型即审查者。
///
/// inspect eval 的任务文件必须是相对路径（绝对路径会触发
/// `root_dir.glob(glob)` 的 NotImplementedError）——把 cwd 设为任务文件
/// 所在目录，传裸文件名。detached 子进程继承该 cwd；log-dir/compose 均
/// 为绝对路径，不受影响。
pub fn spawn_eval(
    task_py: &Path,
    model: &ExecutorModel,
    grader: Option<&ExecutorModel>,
    log_dir: &Path,
    time_limit_secs: u32,
) -> Result<LaunchRecord> {
    let task_dir = task_py
        .parent()
        .context("task.py has no parent dir")?
        .to_path_buf();
    let task_name = task_py
        .file_name()
        .and_then(|s| s.to_str())
        .context("task.py has no file name")?
        .to_string();

    let mut cmd = Command::new(inspect_binary());
    cmd.arg("eval")
        .arg(&task_name)
        .current_dir(&task_dir)
        .arg("--detach")
        .arg("--model")
        .arg(model.inspect_model_id());
    // 内建模型（mockllm 等）无 base_url——不传 --model-base-url
    if !model.base_url.is_empty() {
        cmd.arg("--model-base-url").arg(&model.base_url);
    }
    cmd.arg("--max-tokens")
        .arg(model.max_tokens.to_string())
        .arg("--time-limit")
        .arg(time_limit_secs.to_string())
        .arg("--log-dir")
        .arg(log_dir)
        .arg("--log-level")
        .arg("info");

    // 执行审查：grader 角色绑定审查者模型（scorer 里 get_model(role="grader")）
    if let Some(g) = grader {
        cmd.arg("--model-role")
            .arg(format!("grader={}", g.inspect_model_id()));
    }

    // env 清洗：env_clear + 白名单注入——继承的凭据形态变量（KEY/TOKEN/
    // SECRET/PASSWORD 或 LLM provider 前缀）从根上不进入 eval 进程（R0 审计
    // 约束 1：eval 进程只应能解析 executor/reviewer 两个 provider）。executor
    // key 改经 env 注入（{PROVIDER}_API_KEY 供 Inspect openai-api provider 读取，
    // ALFRED_EXEC_API_KEY 为规范名），不再走 `-M api_key=` argv——ps 不可见。
    cmd.env_clear();
    cmd.envs(eval_child_env(model, grader));

    let output = cmd
        .output()
        .with_context(|| {
            // 不打印完整 {:?}：Command Debug 会展开 env（含 executor key）。
            // 只留 program + args 供排障，env 一律 redact。
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            format!(
                "spawn {} {} (env redacted)",
                cmd.get_program().to_string_lossy(),
                args.join(" ")
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // --detach 在 stdout 打一行 launch 记录后退出 0
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("event").and_then(|e| e.as_str()) == Some("launch") {
                return parse_launch(v);
            }
        }
        // 非 launch 的末尾行：可能是 detach 失败诊断
        bail!("unexpected inspect eval stdout: {line}");
    }
    bail!(
        "inspect eval --detach produced no launch record\nstdout: {stdout}\nstderr: {stderr}"
    )
}

fn parse_launch(v: Value) -> Result<LaunchRecord> {
    let run_id = v
        .get("run_id")
        .and_then(|x| x.as_str())
        .context("launch record missing run_id")?
        .to_string();
    let pid = v.get("pid").and_then(|x| x.as_i64());
    let log_dir = v.get("log_dir").and_then(|x| x.as_str()).map(String::from);
    let output_file = v
        .get("output_file")
        .and_then(|x| x.as_str())
        .context("launch record missing output_file")?
        .to_string();
    Ok(LaunchRecord {
        run_id,
        pid,
        log_dir,
        output_file: PathBuf::from(output_file),
    })
}

/// 轮询 output_file 直到 done 记录 / 进程 crash / 超时。
///
/// 同时轮询 `inspect ctl task list --json` 记录状态到 stderr（观测面），
/// 完成判定只认 output_file 的 done 记录。
pub fn poll_until_done(
    launch: &LaunchRecord,
    timeout_secs: u64,
    ctl_enabled: bool,
) -> Result<PollOutcome> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(done) = read_done_record(&launch.output_file)? {
            return Ok(PollOutcome::Done(done));
        }
        if let Some(pid) = launch.pid {
            if !process_alive(pid) {
                return Ok(PollOutcome::Crashed);
            }
        }
        if ctl_enabled {
            // 观测面：记录 ctl 视角的任务状态（失败不阻断）
            let _ = ctl_task_status(launch);
        }
        if Instant::now() >= deadline {
            return Ok(PollOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// 读 output_file 中的 done 记录（末行；容错非 JSON 诊断行）。
pub fn read_done_record(output_file: &Path) -> Result<Option<EvalDone>> {
    let text = match std::fs::read_to_string(output_file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read detach output_file"),
    };
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("event").and_then(|e| e.as_str()) != Some("done") {
            continue;
        }
        return parse_done(v).map(Some);
    }
    Ok(None)
}

fn parse_done(v: Value) -> Result<EvalDone> {
    let logs = v
        .get("logs")
        .and_then(|x| x.as_array())
        .context("done record missing logs")?;
    let first = logs.first().context("done record logs empty")?;
    Ok(EvalDone {
        task: first
            .get("task")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        task_id: first
            .get("task_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        eval_id: first
            .get("eval_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        status: first
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        location: first
            .get("location")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// `inspect ctl task list --json` 的当前状态（观测面，失败忽略）。
fn ctl_task_status(launch: &LaunchRecord) -> Result<String> {
    let out = Command::new(inspect_binary())
        .args(["ctl", "task", "list", "--json"])
        .output()
        .context("run inspect ctl task list")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&text).context("parse ctl task list")?;
    let empty = Vec::new();
    let tasks = v.get("tasks").and_then(|x| x.as_array()).unwrap_or(&empty);
    let statuses: Vec<String> = tasks
        .iter()
        .filter_map(|t| {
            // ctl 行按 pid 关联（log_location 是日志目录，不含 run_id）
            let pid = t.get("pid").and_then(|x| x.as_i64());
            if pid == launch.pid {
                Some(
                    t.get("status")
                        .and_then(|x| x.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                )
            } else {
                None
            }
        })
        .collect();
    eprintln!("[alfred] ctl status: {}", statuses.join(","));
    Ok(statuses.join(","))
}

/// 归档 eval log（P9 证据）：复制 .eval 原文件 + `inspect log dump` JSON。
///
/// 若 .eval 已直接写在 dest_dir（本实现的默认：`--log-dir` 就是 evals 目录），
/// 复制步骤跳过——self-copy 会把文件截断成 0 字节。
pub fn archive_eval_log(location: &str, dest_dir: &Path) -> Result<PathBuf> {
    let src = PathBuf::from(location);
    let file_name = src
        .file_name()
        .with_context(|| format!("eval log has no file name: {location}"))?;

    // 1) 复制 .eval 原文件（源目标同路径则跳过）
    let copied = dest_dir.join(file_name);
    let same_file = src.canonicalize().ok() == copied.canonicalize().ok();
    if !same_file {
        std::fs::copy(&src, &copied).with_context(|| {
            format!("copy eval log {} -> {}", src.display(), copied.display())
        })?;
    }

    // 2) inspect log dump → JSON
    let out = Command::new(inspect_binary())
        .args(["log", "dump", location])
        .output()
        .with_context(|| format!("inspect log dump {location}"))?;
    if !out.status.success() {
        bail!(
            "inspect log dump failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let dump_path = dest_dir.join(format!(
        "{}.dump.json",
        file_name.to_string_lossy().replace(".eval", "")
    ));
    std::fs::write(&dump_path, &out.stdout)
        .with_context(|| format!("write dump {}", dump_path.display()))?;
    Ok(dump_path)
}

/// 解析 `inspect log dump` JSON 文本。
///
/// inspect 对 unscored score 的 `value` 写非标准 `NaN`（JSON 规范外），
/// serde_json 默认拒绝——先归一化为 null 再解析。R2Audit2 修复：朴素
/// `replace` 会篡改 rationale 里合法出现的 "NaN"/"Infinity" 字样——改为
/// token-aware 替换（只在 JSON 字符串字面量之外的位置替换非标准浮点 token）。
pub fn parse_dump(text: &str) -> Result<Value> {
    serde_json::from_str(&sanitize_nonstandard_floats(text)).context("parse inspect log dump JSON")
}

/// 把 JSON 字符串字面量之外的非标准浮点 token（`NaN`/`Infinity`/`-Infinity`）
/// 归一化为 `null`。字符串内的同名文本原样保留（token-aware，不误伤
/// rationale 里合法出现的 "NaN" 等字样）。
fn sanitize_nonstandard_floats(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            // 字符串字面量：整体透传（含转义序列），不做任何替换
            let start = i;
            i += 1;
            let mut escaped = false;
            while i < bytes.len() {
                if bytes[i] == b'\\' && !escaped {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if bytes[i] == b'"' && !escaped {
                    i += 1;
                    break;
                }
                escaped = false;
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        // 字符串外：按 token 前缀匹配替换（先 -Infinity，再 Infinity/NaN）
        if text[i..].starts_with("-Infinity") {
            out.push_str("null");
            i += "-Infinity".len();
        } else if text[i..].starts_with("Infinity") {
            out.push_str("null");
            i += "Infinity".len();
        } else if text[i..].starts_with("NaN") {
            out.push_str("null");
            i += "NaN".len();
        } else {
            out.push_str(&text[i..i + 1]);
            i += 1;
        }
    }
    out
}
/// 从 `inspect log dump` JSON 读某个 scorer 的首个 sample score（原始值）。
///
/// 返回 `samples[0].scores[<scorer_name>]`，形如
/// `{"value": "C", "answer": ..., "explanation": ..., "metadata": {...}}`；
/// scorer 缺失或无样本时返回 None。执行/计划审查都是单样本任务。
pub fn sample_score<'a>(dump: &'a Value, scorer_name: &str) -> Option<&'a Value> {
    let samples = dump.get("samples")?.as_array()?;
    let first = samples.first()?;
    let scores = first.get("scores")?.as_object()?;
    scores.get(scorer_name)
}

/// 执行审查结论（从 eval dump 的 `exec_verdict_scorer` 分数解析）。
///
/// 结构化读取：`value` 为等级（C/I/P），`metadata.failure_class` /
/// `metadata.rationale` 为分流依据；`value` 为 null 即 unscored（解析失败
/// 或打分器未产出），unscored 原因在 `metadata.unscored_reason`。
///
/// 注意：scorer 产出 `{grade, failure_class, rationale}`（限界上下文 §6.9
/// 的子集），Rust 侧映射为 alfred-core ExecVerdict 时 confidence 取 High
/// （temperature=0 确定性判分），evidence 空——对齐表未覆盖，记录到 R2 交付。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecReviewOutcome {
    /// 解析出的审查结论（unscored / 未知等级时为 None）。
    pub verdict: Option<ExecVerdict>,
    /// unscored 原因（verdict_parse_failure / scorer 缺失 / 未知等级等）。
    pub unscored_reason: Option<String>,
    /// 解析失败细节（pydantic 错误等）。
    pub detail: Option<String>,
}

/// 从 dump JSON 提取执行审查结论。
pub fn extract_exec_verdict(dump: &Value) -> ExecReviewOutcome {
    let score = match sample_score(dump, "exec_verdict_scorer") {
        Some(s) => s,
        None => {
            return ExecReviewOutcome {
                verdict: None,
                unscored_reason: Some("exec_verdict_scorer_missing".into()),
                detail: None,
            }
        }
    };
    let meta = score
        .get("metadata")
        .and_then(|m| m.as_object())
        .cloned()
        .unwrap_or_default();
    let value = score.get("value").and_then(|v| v.as_str());
    let Some(grade_str) = value else {
        // unscored：value 为 null
        return ExecReviewOutcome {
            verdict: None,
            unscored_reason: meta
                .get("unscored_reason")
                .and_then(|u| u.as_str())
                .map(String::from)
                .or_else(|| Some("unscored".into())),
            detail: meta
                .get("detail")
                .and_then(|d| d.as_str())
                .map(String::from),
        };
    };
    let grade = match grade_str {
        "C" => VerdictGrade::C,
        "I" => VerdictGrade::I,
        "P" => VerdictGrade::P,
        other => {
            return ExecReviewOutcome {
                verdict: None,
                unscored_reason: Some("unknown_grade".into()),
                detail: Some(other.to_string()),
            }
        }
    };
    let failure_class = meta
        .get("failure_class")
        .and_then(|f| f.as_str())
        .and_then(|s| serde_json::from_str::<FailureClass>(&format!("\"{s}\"")).ok());
    let rationale = meta
        .get("rationale")
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_string();
    let explanation = score
        .get("explanation")
        .and_then(|e| e.as_str())
        .map(String::from)
        .unwrap_or_else(|| rationale.clone());
    match ExecVerdict::new(grade, failure_class, Confidence::High, vec![], explanation) {
        Ok(v) => ExecReviewOutcome {
            verdict: Some(v),
            unscored_reason: None,
            detail: None,
        },
        Err(e) => ExecReviewOutcome {
            verdict: None,
            unscored_reason: Some("verdict_invariant_violation".into()),
            detail: Some(e),
        },
    }
}

/// eval 子进程 env 白名单（`env_clear` 后注入）。
///
/// - 固定项：`PYTHONDONTWRITEBYTECODE=1`（任务 import 时不写 __pycache__）。
/// - 基础变量：PATH/HOME/LANG/TZ/TERM（父进程有则保留）。
/// - 透传：`ALFRED_*`（ALFRED_INSPECT/ALFRED_STATE_DIR 等按需保留）。
///
/// 白名单从根上排除继承的凭据形态变量（KEY/TOKEN/SECRET/PASSWORD 及常见
/// LLM provider 前缀）；executor 凭据单独经 `{PROVIDER}_API_KEY` /
/// `ALFRED_EXEC_API_KEY` 注入（见 `eval_child_env`）。
fn whitelisted_env() -> Vec<(String, String)> {
    let mut envs = vec![("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string())];
    for key in ["PATH", "HOME", "LANG", "TZ", "TERM"] {
        if let Ok(v) = std::env::var(key) {
            envs.push((key.to_string(), v));
        }
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("ALFRED_") {
            envs.push((k, v));
        }
    }
    envs
}

/// 组装 eval 子进程 env（`env_clear` 语义）：白名单 + executor/reviewer 凭据。
///
/// Inspect `openai-api/<provider>/<model>` 从 `{PROVIDER}_API_KEY` env 读 key
/// （不设 `api_key` 模型选项时）；`ALFRED_EXEC_API_KEY` 为规范别名（探针/排障
/// 用）。两者均不进 argv——`ps` 不可见。reviewer（grader）若与 executor 不同
/// provider，其 key 一并注入；raw 内建模型（mockllm）无 key 不注入。
fn eval_child_env(model: &ExecutorModel, grader: Option<&ExecutorModel>) -> Vec<(String, String)> {
    let mut envs = whitelisted_env();
    push_provider_key(&mut envs, model, true);
    if let Some(g) = grader {
        push_provider_key(&mut envs, g, false);
        // grader 无显式 base_url（model-role 配置不接受 base_url 字段）——
        // 经 INSPECT_EVAL_MODEL_BASE_URL env 兜底（inspect model_base_url 末级回退）。
        // 主模型（executor）已有 --model-base-url，不受影响。
        if !g.raw_id && !g.base_url.is_empty() {
            envs.push(("INSPECT_EVAL_MODEL_BASE_URL".to_string(), g.base_url.clone()));
        }
    }
    envs
}

fn push_provider_key(envs: &mut Vec<(String, String)>, m: &ExecutorModel, is_main: bool) {
    if m.raw_id || m.api_key.is_empty() {
        return;
    }
    envs.push((
        format!("{}_API_KEY", m.provider.to_ascii_uppercase().replace('-', "_")),
        m.api_key.clone(),
    ));
    if is_main {
        envs.push(("ALFRED_EXEC_API_KEY".to_string(), m.api_key.clone()));
    }
}

/// 进程是否存活（`/bin/kill -0 <pid>`）。
fn process_alive(pid: i64) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn whitelist_env_is_minimal_and_credential_free() {
        let map: HashMap<String, String> = whitelisted_env().into_iter().collect();
        // 固定项
        assert_eq!(
            map.get("PYTHONDONTWRITEBYTECODE").map(String::as_str),
            Some("1")
        );
        // 基础变量保留（测试进程必有 PATH/HOME）
        assert!(map.contains_key("PATH"));
        assert!(map.contains_key("HOME"));
        // 白名单不得含凭据形态变量（ALFRED_* 是显式透传的命名空间，豁免检查）
        for k in map.keys() {
            if k.starts_with("ALFRED_") {
                continue;
            }
            let upper = k.to_ascii_uppercase();
            for pat in ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD"] {
                assert!(!upper.contains(pat), "whitelist leaked credential-like var: {k}");
            }
        }
    }

    #[test]
    fn eval_child_env_injects_key_only_via_env() {
        let model = ExecutorModel {
            provider: "zhipucoding".into(),
            model: "glm-5.2".into(),
            base_url: "http://x".into(),
            api_key: "sk-test-secret-123".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let envs = eval_child_env(&model, None);
        let map: HashMap<String, String> = envs.into_iter().collect();
        // key 经 env（Inspect openai-api provider 读取的派生名 + 规范别名）
        assert_eq!(
            map.get("ZHIPUCODING_API_KEY").map(String::as_str),
            Some("sk-test-secret-123")
        );
        assert_eq!(
            map.get("ALFRED_EXEC_API_KEY").map(String::as_str),
            Some("sk-test-secret-123")
        );
        // 白名单固定项仍在
        assert_eq!(
            map.get("PYTHONDONTWRITEBYTECODE").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn eval_child_env_injects_grader_provider_key_too() {
        let exec = ExecutorModel {
            provider: "zhipucoding".into(),
            model: "glm-5.2".into(),
            base_url: "http://x".into(),
            api_key: "sk-exec".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let grader = ExecutorModel {
            provider: "anthropic".into(),
            model: "claude-x".into(),
            base_url: "http://y".into(),
            api_key: "sk-grader".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let envs = eval_child_env(&exec, Some(&grader));
        let map: HashMap<String, String> = envs.into_iter().collect();
        assert_eq!(map.get("ZHIPUCODING_API_KEY").map(String::as_str), Some("sk-exec"));
        assert_eq!(map.get("ANTHROPIC_API_KEY").map(String::as_str), Some("sk-grader"));
        // ALFRED_EXEC_API_KEY 只由主模型写
        assert_eq!(map.get("ALFRED_EXEC_API_KEY").map(String::as_str), Some("sk-exec"));
    }

    #[test]
    fn eval_child_env_skips_raw_mock_model() {
        let exec = ExecutorModel {
            provider: "zhipucoding".into(),
            model: "glm-5.2".into(),
            base_url: "http://x".into(),
            api_key: "sk-exec".into(),
            max_tokens: 1024,
            raw_id: false,
        };
        let mock = ExecutorModel {
            provider: "mockllm".into(),
            model: "mockllm/model".into(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 1024,
            raw_id: true,
        };
        let envs = eval_child_env(&exec, Some(&mock));
        let map: HashMap<String, String> = envs.into_iter().collect();
        // mockllm 无 key 不注入
        assert!(!map.contains_key("MOCKLLM_API_KEY"));
        assert_eq!(map.get("ZHIPUCODING_API_KEY").map(String::as_str), Some("sk-exec"));
    }

    #[test]
    fn extract_exec_verdict_parses_grade_and_failure_class() {
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "exec_verdict_scorer": {
                        "value": "I",
                        "answer": "artifact",
                        "explanation": "file content mismatch",
                        "metadata": {
                            "grade": "I",
                            "failure_class": "fidelity_dispute",
                            "rationale": "file content mismatch"
                        }
                    }
                }
            }]
        }"#).unwrap();
        let out = extract_exec_verdict(&dump);
        let v = out.verdict.unwrap();
        assert_eq!(v.value, VerdictGrade::I);
        assert_eq!(v.failure_class, Some(FailureClass::FidelityDispute));
        assert!(v.explanation.contains("file content mismatch"));
        assert_eq!(out.unscored_reason, None);
    }

    #[test]
    fn extract_exec_verdict_marks_unscored() {
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "exec_verdict_scorer": {
                        "value": null,
                        "explanation": "NOT JSON",
                        "metadata": {
                            "unscored_reason": "verdict_parse_failure",
                            "detail": "Invalid JSON"
                        }
                    }
                }
            }]
        }"#).unwrap();
                let out = extract_exec_verdict(&dump);
        assert!(out.verdict.is_none());
        assert_eq!(out.unscored_reason.as_deref(), Some("verdict_parse_failure"));
        assert_eq!(out.detail.as_deref(), Some("Invalid JSON"));
    }

    #[test]
    fn extract_exec_verdict_parses_partial_grade() {
        // R2Audit2 修复：P 档（部分兑现）构造用例——grade P 必须有 failure_class
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "exec_verdict_scorer": {
                        "value": "P",
                        "answer": "artifact",
                        "explanation": "one of two files created",
                        "metadata": {
                            "grade": "P",
                            "failure_class": "contract_fault",
                            "rationale": "hello.txt created but world.txt missing"
                        }
                    }
                }
            }]
        }"#).unwrap();
        let out = extract_exec_verdict(&dump);
        let v = out.verdict.unwrap();
        assert_eq!(v.value, VerdictGrade::P);
        assert_eq!(v.failure_class, Some(FailureClass::ContractFault));
        assert!(v.explanation.contains("one of two files created"));
        assert_eq!(out.unscored_reason, None);
    }

    #[test]
    fn extract_exec_verdict_rejects_grade_p_without_failure_class() {
        // P 档不变量：value=P 必须带 failure_class——缺失即构造失败（unscored）
        let dump: Value = serde_json::from_str(r#"{
            "samples": [{
                "scores": {
                    "exec_verdict_scorer": {
                        "value": "P",
                        "metadata": {"grade": "P", "failure_class": null, "rationale": "partial"}
                    }
                }
            }]
        }"#).unwrap();
        let out = extract_exec_verdict(&dump);
        assert!(out.verdict.is_none());
        assert_eq!(out.unscored_reason.as_deref(), Some("verdict_invariant_violation"));
    }

    #[test]
    fn parse_dump_normalizes_nonstandard_floats_token_aware() {
        // 字符串外的 NaN/Infinity/-Infinity → null；字符串内同名文本原样保留
        let dump = r#"{"a": NaN, "b": Infinity, "c": -Infinity, "rationale": "got NaN and -Infinity"}"#;
        let v = parse_dump(dump).unwrap();
        assert!(v["a"].is_null());
        assert!(v["b"].is_null());
        assert!(v["c"].is_null());
        assert_eq!(v["rationale"].as_str(), Some("got NaN and -Infinity"));
    }

    #[test]
    fn parse_dump_preserves_escaped_quotes_in_strings() {
        // 转义引号不中断字符串跟踪；字符串内 "Infinity" 原样保留
        let s = r#"{"a": "say \"Infinity\" now", "b": Infinity}"#;
        let v = parse_dump(s).unwrap();
        assert_eq!(v["a"].as_str(), Some("say \"Infinity\" now"));
        assert!(v["b"].is_null());
    }

    #[test]
    fn parse_dump_rejects_invalid_json() {
        // dump 损坏用例：非 JSON 输入必须 Err（R2Audit2：不再被 if-let 静默吞）
        let err = parse_dump(r#"{ not json at all"#).unwrap_err();
        assert!(
            format!("{err:#}").contains("parse inspect log dump JSON"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_launch_and_done_records() {
        let launch_json = r#"{"event":"launch","run_id":"abc","eval_set_id":"s","pid":123,"log_dir":"/tmp/l","control":{"socket_path":"/tmp/c.sock"},"output_file":"/tmp/o.out"}"#;
        let v: Value = serde_json::from_str(launch_json).unwrap();
        let lr = parse_launch(v).unwrap();
        assert_eq!(lr.run_id, "abc");
        assert_eq!(lr.pid, Some(123));
        assert_eq!(lr.output_file, PathBuf::from("/tmp/o.out"));

        let done_json = r#"{"event":"done","run_id":"abc","logs":[{"task":"executor_task","task_id":"t1","eval_id":"e1","status":"success","location":"/tmp/l/2026-01-01_task_x.eval"}]}"#;
        let v: Value = serde_json::from_str(done_json).unwrap();
        let done = parse_done(v).unwrap();
        assert_eq!(done.status, "success");
        assert_eq!(done.task_id, "t1");
        assert!(done.location.contains("task_x.eval"));
    }

    #[test]
    fn archive_skips_self_copy_and_writes_dump() {
        let dir = std::env::temp_dir().join("alfred-archive-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 写一个"假 .eval"，dump 用 mockllm 真实跑一次（或跳过 dump 校验）
        let eval_path = dir.join("fake.eval");
        std::fs::write(&eval_path, b"not-a-real-eval").unwrap();
        // 同路径归档：不得截断原文件
        let res = archive_eval_log(eval_path.to_str().unwrap(), &dir);
        // dump 会对假 eval 报错——但我们只关心 self-copy 不截断；dump 失败返回 Err 也正常
        match res {
            Ok(_) => {}
            Err(_) => {}
        }
        let content = std::fs::read(&eval_path).unwrap();
        assert_eq!(content, b"not-a-real-eval", "self-copy must not truncate");
        std::fs::remove_dir_all(&dir).ok();
    }
}
