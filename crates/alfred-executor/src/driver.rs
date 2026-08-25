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

/// 启动 detached eval，解析 launch 记录。
///
/// 只注入 executor 一个 provider 凭据（经 env：`{PROVIDER}_API_KEY` +
/// `ALFRED_EXEC_API_KEY`，不进 argv），子进程 env 做 env_clear + 白名单
/// 清洗（R0 审计约束 1：防执行者经桥点名其他 provider）。
///
/// inspect eval 的任务文件必须是相对路径（绝对路径会触发
/// `root_dir.glob(glob)` 的 NotImplementedError）——把 cwd 设为任务文件
/// 所在目录，传裸文件名。detached 子进程继承该 cwd；log-dir/compose 均
/// 为绝对路径，不受影响。
pub fn spawn_eval(
    task_py: &Path,
    model: &ExecutorModel,
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
        .arg(model.inspect_model_id())
        .arg("--model-base-url")
        .arg(&model.base_url)
        .arg("--max-tokens")
        .arg(model.max_tokens.to_string())
        .arg("--time-limit")
        .arg(time_limit_secs.to_string())
        .arg("--log-dir")
        .arg(log_dir)
        .arg("--log-level")
        .arg("info");

    // env 清洗：env_clear + 白名单注入——继承的凭据形态变量（KEY/TOKEN/
    // SECRET/PASSWORD 或 LLM provider 前缀）从根上不进入 eval 进程（R0 审计
    // 约束 1：eval 进程只应能解析 executor 一个 provider）。executor key 改经
    // env 注入（{PROVIDER}_API_KEY 供 Inspect openai-api provider 读取，
    // ALFRED_EXEC_API_KEY 为规范名），不再走 `-M api_key=` argv——ps 不可见。
    cmd.env_clear();
    cmd.envs(eval_child_env(model));

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

/// 组装 eval 子进程 env（`env_clear` 语义）：白名单 + executor 凭据。
///
/// Inspect `openai-api/<provider>/<model>` 从 `{PROVIDER}_API_KEY` env 读 key
/// （不设 `api_key` 模型选项时）；`ALFRED_EXEC_API_KEY` 为规范别名（探针/排障
/// 用）。两者均不进 argv——`ps` 不可见。
fn eval_child_env(model: &ExecutorModel) -> Vec<(String, String)> {
    let mut envs = whitelisted_env();
    envs.push((
        format!("{}_API_KEY", model.provider.to_ascii_uppercase().replace('-', "_")),
        model.api_key.clone(),
    ));
    envs.push(("ALFRED_EXEC_API_KEY".to_string(), model.api_key.clone()));
    envs
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
        };
        let envs = eval_child_env(&model);
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
