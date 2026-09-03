//! Inspect 容器管理驱动层：宿主侧 Python 驱动脚本（非 eval）起容器 + 驱动 pi。
//!
//! 依据：属主 08-27「容器统一用 Inspect AI 管理」——"管理容器"的正确接口是
//! Inspect 的容器管理机制（`DockerSandboxEnvironment`（docker compose 起容器）
//! + `sandbox_agent_bridge`（宿主 run_model_service + 容器内 exec_remote
//! model_proxy）+ `exec_remote` 驱动容器内 pi），不是 `inspect eval` 评测包装。
//! 属主 08-05「Inspect AI 只是一个可选的技术架构」——Inspect 只承担容器管理，
//! 不承担评测（去 eval 启动包装）。
//!
//! 流程：渲染 compose（挂载面矩阵，隔离机制不变）+ 生成宿主侧驱动脚本
//! （`driver.py`，独立 Python 脚本，非 eval Task）→ spawn `python3 driver.py`
//! （env 清洗白名单 + 单角色 provider 凭据）→ 轮询 `<work>/driver.done.json`
//! done 记录 → 读 bind mount 产物。容器生命周期（compose up/down）由驱动脚本
//! 经 Inspect 容器管理接口负责；宿主不再经 `inspect eval`。
//!
//! done 记录契约（驱动脚本产出，与旧 eval output_file 同构）：
//! - `{"event":"done","status":"success"}`：pi 正常完成。
//! - `{"event":"done","status":"error","error":"..."}`：任务级失败（pi 未
//!   settled / 容器未产出）——done 照发、驱动进程退出码非零；分支看 status。
//! - `{"event":"done","status":"timed_out","error":"..."}`：驱动脚本自限时
//!   （`anyio.fail_after`）触达——先清理容器再写 done。
//! - 进程消失无 done = crash / 被 kill。

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::ExecutorModel;

/// 宿主侧 Python 解析链（P1 恢复 + codux PTY 根治）：`ALFRED_PYTHON` 环境变量
/// 优先；其次本仓 venv `.plans/r0-lab/venv/bin/python`——**相对
/// `std::env::current_exe()`（canonicalize 解析 symlink 后）定位的仓根解析**
/// （exe → 上两级到仓根 → venv python）；最后 PATH 上的 `python3`。
///
/// 为什么不相对 `current_dir()`：属主在 codux 终端跑 alfred chat 时 cwd 是任意
/// 项目目录（≠ alfred 仓），相对 cwd 拼出的 venv 路径必 miss → 回退 PATH 上的
/// `python3`——codux PTY 的 PATH 上可能挂着坏 python（Python 2 风格 SyntaxError，
/// 文件本身合法 UTF-8、venv py3.12 编译通过）→ driver.py 起不来。相对 exe 解析
/// 对 symlink 安装（codux wrapper PATH 命中 `~/.local/bin/alfred` →
/// `target/debug/alfred`）同样成立：**macOS `current_exe()` 返回调用路径本身、
/// 不解析 symlink**（实测，`_NSGetExecutablePath` 语义），故先
/// `fs::canonicalize` 解析出真实 exe 再向上两级即达仓根。canonicalize 后的路径
/// 不再含 symlink，`..` 语义与字面一致。exe 定位的 venv 不存在（如未来
/// `cargo install` 到 CARGO_HOME）→ 保持 PATH 回退。
///
/// 返回绝对路径或裸命令名。裸 `python3` 由 spawn 时父进程 PATH 解析，不受子进程
/// cwd 影响。
pub fn python_binary() -> String {
    if let Ok(p) = std::env::var("ALFRED_PYTHON") {
        return p;
    }
    // exe 相对 venv：canonicalize 解析 symlink 安装（~/.local/bin/alfred 等）→
    // 真实 exe（<root>/target/{debug,release}/alfred）→ 上两级 = 仓根 → venv python。
    if let Ok(exe) = std::env::current_exe() {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            let venv_python = root.join(".plans/r0-lab/venv/bin/python");
            if venv_python.is_file() {
                return venv_python.to_string_lossy().into_owned();
            }
        }
    }
    "python3".to_string()
}

/// cwd 无关绝对化：相对路径按当前进程 cwd 拼成绝对路径，绝对路径原样返回。
///
/// 不用 `fs::canonicalize`：不要求路径已存在（done 记录等写入前路径），也不改写
/// symlink（macOS `/tmp` → `/private/tmp`）。凡交给子进程按其 cwd 解析的路径——
/// spawn argv（driver.py）、嵌入生成脚本的 done 记录路径——必须经此：spawn 后
/// 子进程 cwd 切到 work_dir，相对路径会被二次解析（路径双拼，e2e 传绝对路径只是
/// 恰好不触发）。
pub fn absolutize_cwd(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

/// 容器驱动进程的 launch 记录（持有 `Child`：poll 期 `try_wait()` 收割——
/// 只存 pid 时子进程崩溃成 zombie，`kill -0` 恒真，crash 被误报成等满超时）。
pub struct DriverLaunch {
    pub pid: i64,
    /// done 记录文件（驱动脚本写完即结束）。
    pub done_marker: PathBuf,
    /// 驱动子进程句柄（收割专用；drop 不 kill——超时路径保持旧语义）。
    child: Child,
}

/// done 记录（驱动脚本产出；status: "success" | "error" | "timed_out"）。
#[derive(Debug, Clone)]
pub struct DriverDone {
    pub status: String,
    pub error: Option<String>,
}

/// 轮询结果。
#[derive(Debug, Clone)]
pub enum DriverOutcome {
    /// 读到 done 记录。
    Done(DriverDone),
    /// 超时（未 done、进程仍活）。
    TimedOut,
    /// 进程已退出且无 done（crash / 被 kill）；`None` = 信号终止无退出码。
    Crashed(Option<i32>),
}

/// 生成宿主侧容器驱动脚本并 spawn（非 eval）。
///
/// 路径契约：`driver_py` / `work_dir` 在本层 cwd 无关绝对化——调用方传相对/绝对
/// 均正确（子进程 cwd 切到 work_dir 后，相对 argv 会被二次解析成双拼）。
///
/// 驱动脚本自身经 Inspect 容器管理接口（DockerSandboxEnvironment +
/// sandbox_agent_bridge + exec_remote）起容器、驱动容器内 pi、写 done 记录。
/// 本函数只负责 spawn + 记录 pid + 保留 Child（poll 期收割）+ done_marker 路径，
/// 不等待。
///
/// env 清洗：env_clear + 白名单 + 单角色 provider 凭据（`{PROVIDER}_API_KEY` /
/// `{PROVIDER}_BASE_URL` + `ALFRED_EXEC_API_KEY`，不进 argv，`ps` 不可见）。
/// 驱动进程只应能解析本角色模型（R0 审计约束 1：防容器内经桥点名其他 provider）。
pub fn spawn_container_driver(
    driver_py: &Path,
    model: &ExecutorModel,
    work_dir: &Path,
) -> Result<DriverLaunch> {
    // spawn 层绝对化（见 `absolutize_cwd`）：先于 done 记录 / 日志文件 / argv /
    // current_dir 全部使用——cwd 切换后相对路径不再被子进程二次解析。
    let work_dir = absolutize_cwd(work_dir);
    let driver_py = absolutize_cwd(driver_py);
    let done_marker = work_dir.join("driver.done.json");
    // 清陈旧 done 记录（重跑/续跑幂等）。
    if done_marker.exists() {
        std::fs::remove_file(&done_marker)
            .with_context(|| format!("remove stale done marker {}", done_marker.display()))?;
    }
    let stdout = File::create(work_dir.join("driver.stdout.log"))
        .with_context(|| format!("create driver stdout log in {}", work_dir.display()))?;
    let stderr = File::create(work_dir.join("driver.stderr.log"))
        .with_context(|| format!("create driver stderr log in {}", work_dir.display()))?;

    let mut cmd = Command::new(python_binary());
    cmd.arg(driver_py)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    cmd.env_clear();
    cmd.envs(container_child_env(model));

    let child = cmd.spawn().with_context(|| {
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
    Ok(DriverLaunch {
        pid: child.id() as i64,
        done_marker,
        child,
    })
}

/// 轮询 done 记录直到 Done / 进程 crash / 超时。
///
/// 完成判定只认 done 记录；进程退出无 done = crash（每轮先 `try_wait()` 收割
/// Child——不收割则 zombie 的 `kill -0` 恒真，crash 误报成等满超时的
/// TimedOut）；超时仍未 done 且进程还活 = timed out。`ctl_enabled` 观测面已随
/// `inspect ctl` 退役（无 eval 即无 ctl），不再轮询。
pub fn poll_container_driver(
    launch: &mut DriverLaunch,
    timeout_secs: u64,
) -> Result<DriverOutcome> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(done) = read_done_marker(&launch.done_marker)? {
            return Ok(DriverOutcome::Done(done));
        }
        if let Some(status) = launch.child.try_wait()? {
            return Ok(DriverOutcome::Crashed(status.code()));
        }
        if Instant::now() >= deadline {
            return Ok(DriverOutcome::TimedOut);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// 读 done 记录（驱动脚本写 `<work>/driver.done.json`，末行；容错非 JSON 行）。
pub fn read_done_marker(path: &Path) -> Result<Option<DriverDone>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read driver done marker"),
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
        return Ok(Some(DriverDone {
            status: v
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            error: v.get("error").and_then(|e| e.as_str()).map(String::from),
        }));
    }
    Ok(None)
}

/// 驱动子进程 env 白名单（`env_clear` 后注入）。
///
/// - 固定项：`PYTHONDONTWRITEBYTECODE=1`（驱动 import 时不写 __pycache__）。
/// - 基础变量：PATH/HOME/LANG/TZ/TERM（父进程有则保留）。
/// - 透传：`ALFRED_*`（ALFRED_STATE_DIR 等按需保留）。
///
/// 白名单从根上排除继承的凭据形态变量（KEY/TOKEN/SECRET/PASSWORD 及常见
/// LLM provider 前缀）；本角色凭据单独经 `{PROVIDER}_API_KEY` /
/// `{PROVIDER}_BASE_URL` / `ALFRED_EXEC_API_KEY` 注入（见 `container_child_env`）。
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

/// 组装驱动子进程 env（`env_clear` 语义）：白名单 + 单角色 provider 凭据。
///
/// Inspect `openai-api/<provider>/<model>` 从 `{PROVIDER}_API_KEY` /
/// `{PROVIDER}_BASE_URL` env 读 key/base_url（不设模型选项时）；`ALFRED_EXEC_API_KEY`
/// 为规范别名（探针/排障用）。两者均不进 argv——`ps` 不可见。raw 内建模型
/// （mockllm）无 key/base_url 不注入。
fn container_child_env(model: &ExecutorModel) -> Vec<(String, String)> {
    let mut envs = whitelisted_env();
    push_provider_creds(&mut envs, model);
    envs
}

fn push_provider_creds(envs: &mut Vec<(String, String)>, m: &ExecutorModel) {
    if m.raw_id || m.api_key.is_empty() {
        return;
    }
    let prefix = m.provider.to_ascii_uppercase().replace('-', "_");
    envs.push((format!("{prefix}_API_KEY"), m.api_key.clone()));
    if !m.base_url.is_empty() {
        envs.push((format!("{prefix}_BASE_URL"), m.base_url.clone()));
    }
    envs.push(("ALFRED_EXEC_API_KEY".to_string(), m.api_key.clone()));
}

