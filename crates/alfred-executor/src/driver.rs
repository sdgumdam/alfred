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
//! - `{"event":"done","status":"cancelled","error":"..."}`：可捕获终止
//!   （SIGTERM/SIGINT——宿主取消转发/人工终止）——先走同一 shield 清理
//!   （Inspect compose down）再写 done；非机械失败，恢复路由不自动重跑。
//! - 进程消失无 done = crash / 被不可捕获信号杀（SIGKILL；孤儿恢复按
//!   `driver.project.json` 的精确项目记录回收，见 [`reclaim_orphaned_project`]）。
//! - done 记录另携 `session`（原生 session 关联；未取到身份 = null）：
//!   `{id, file, host_file, file_exists}`——`id`/`file` 来自 pi RPC
//!   `get_state` 实测响应（身份在 RPC 启动即分配，先于 prompt），`host_file`
//!   经 compose 挂载固定前缀精确映射，`file_exists` 只在宿主文件可定位时
//!   isfile 实测断言（W08 核收：pi 需首条 assistant 消息才落盘，身份已
//!   分配 ≠ 文件存在）。

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
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

/// done 记录携带的原生 session 关联（driver 从 pi RPC `get_state` 响应取得；
/// 未取到身份 = None）。
///
/// W08 核收事实：pi 启动即分配 sessionFile/sessionId，但 `_persist()` 需首条
/// assistant 消息才写盘——身份与落盘分开记录；`file_exists` 仅在宿主路径可
/// 定位时由 isfile 实测断言，不以身份冒称文件存在。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriverSession {
    /// pi 分配的原生 session id（get_state.data.sessionId，原样）。
    pub id: String,
    /// 容器内 session 文件路径（get_state.data.sessionFile，原样）。
    pub file: String,
    /// 宿主侧保留路径（compose 挂载固定前缀的精确映射；前缀不符 = None）。
    pub host_file: Option<String>,
    /// done 落盘时点宿主文件实际存在（isfile 实测）；无法定位 = None。
    pub file_exists: Option<bool>,
}

/// done 记录（驱动脚本产出；status: "success" | "error" | "timed_out"）。
#[derive(Debug, Clone, PartialEq)]
pub struct DriverDone {
    pub status: String,
    pub error: Option<String>,
    /// 原生 session 关联（pi RPC get_state 实测响应；无 = None）。
    pub session: Option<DriverSession>,
}

/// 轮询结果。
#[derive(Debug, Clone, PartialEq)]
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
            // session 关联：只认实测字段（id+file 齐全才成身份；残缺/null →
            // None——不以部分字段造身份，与驱动的“如实缺席”语义一致）。
            session: v.get("session").and_then(|s| {
                Some(DriverSession {
                    id: s.get("id")?.as_str()?.to_string(),
                    file: s.get("file")?.as_str()?.to_string(),
                    host_file: s
                        .get("host_file")
                        .and_then(|h| h.as_str())
                        .map(String::from),
                    file_exists: s.get("file_exists").and_then(|e| e.as_bool()),
                })
            }),
        }));
    }
    Ok(None)
}

/// G1 孤儿项目回收：按孤儿 exec 目录的 `driver.project.json`（驱动在
/// sample_init 一返回就落盘的精确项目身份）把孤儿 compose 项目 down 掉。
///
/// 只用现成 Inspect 回收入口：`project_cleanup`（与驱动自身收尾、
/// `DockerSandboxEnvironment.sample_cleanup` 同一接口），ComposeProject 用记录
/// 里的项目名 + compose 文件重建——精确关联本孤儿，不做 docker 全局扫描/前缀
/// 猜测（项目名基座跨 run 共享，前缀不唯一）。解释器用驱动记录的
/// `sys.executable`（跑得起驱动的解释器必有 inspect_ai；缺失回落
/// [`python_binary`] 解析链）。
///
/// 返回值 = 审计事实（reclaimed/原因/错误），调用方落 audit：
/// - 无 `driver.project.json`（sample_init 返回前被杀）→ `reclaimed:false,
///   reason:"no driver.project.json"`——不猜项目名；
/// - done 记录已证 `cleanup_status == "released"`（可捕获中断的完整收尾）
///   → 不重复 down；
/// - 回收子进程失败/超时 → `reclaimed:false` + error——恢复路由不被资源
///   回收失败阻塞（状态处置与资源回收分开，各自如实）。
pub fn reclaim_orphaned_project(exec_dir: &Path) -> Result<Value> {
    // done 记录已证明容器释放（可捕获中断的完整收尾）→ 最便宜的既有事实
    // 先查：不重复 down，也不需要项目记录。
    if let Ok(done_text) = std::fs::read_to_string(exec_dir.join("driver.done.json")) {
        if let Ok(done) = serde_json::from_str::<Value>(&done_text) {
            if done.get("cleanup_status").and_then(|s| s.as_str()) == Some("released") {
                return Ok(serde_json::json!({
                    "reclaimed": false,
                    "reason": "driver done record certifies cleanup released",
                }));
            }
        }
    }
    let record_path = exec_dir.join("driver.project.json");
    let text = match std::fs::read_to_string(&record_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({
                "reclaimed": false,
                "reason": "no driver.project.json (driver killed before sample_init returned)",
            }));
        }
        Err(e) => return Err(anyhow::Error::new(e).context("read driver.project.json")),
    };
    let rec: Value =
        serde_json::from_str(&text).context("parse driver.project.json")?;
    let Some(project) = rec.get("project").and_then(|p| p.as_str()) else {
        return Ok(serde_json::json!({
            "reclaimed": false,
            "reason": "project identity missing in driver.project.json",
        }));
    };
    let compose = rec
        .get("compose_file")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let python = rec
        .get("python")
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .unwrap_or_else(python_binary);
    let script = r#"
import json, sys
import anyio
from inspect_ai.util._sandbox.docker.cleanup import project_cleanup
from inspect_ai.util._sandbox.docker.util import ComposeProject
name, config = sys.argv[1], (sys.argv[2] or None)
project = ComposeProject(name=name, config=config,
                         sample_id=None, epoch=None, env=None)
anyio.run(project_cleanup, project, True)
print(json.dumps({"down": name}))
"#;
    let mut cmd = Command::new(python);
    cmd.arg("-c")
        .arg(script)
        .arg(project)
        .arg(compose.clone().unwrap_or_default())
        .env_clear()
        .envs(whitelisted_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(serde_json::json!({
                "reclaimed": false,
                "project": project,
                "error": format!("spawn reclaim python failed: {e}"),
            }));
        }
    };
    // 有界等待：compose down 常规数秒；卡死不拖垮恢复路由。
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(if status.success() {
                    serde_json::json!({
                        "reclaimed": true,
                        "project": project,
                        "compose_file": compose,
                    })
                } else {
                    let stderr = child
                        .wait_with_output()
                        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                        .unwrap_or_default();
                    serde_json::json!({
                        "reclaimed": false,
                        "project": project,
                        "error": format!("reclaim python exited {status}: {stderr}"),
                    })
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(serde_json::json!({
                        "reclaimed": false,
                        "project": project,
                        "error": "reclaim python timed out after 120s",
                    }));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                return Ok(serde_json::json!({
                    "reclaimed": false,
                    "project": project,
                    "error": format!("reclaim python poll failed: {e}"),
                }));
            }
        }
    }
}

/// 驱动子进程 env 白名单（`env_clear` 后注入）。
///
/// - 固定项：`PYTHONDONTWRITEBYTECODE=1`（驱动 import 时不写 __pycache__）。
/// - 基础变量：PATH/HOME/LANG/TZ/TERM 和无凭据的 NO_PROXY/no_proxy 豁免列表。
/// - 透传：`ALFRED_*`（ALFRED_STATE_DIR 等按需保留）。
///
/// 白名单从根上排除继承的凭据形态变量（KEY/TOKEN/SECRET/PASSWORD 及常见
/// LLM provider 前缀）；本角色凭据单独经 `{PROVIDER}_API_KEY` /
/// `{PROVIDER}_BASE_URL` / `ALFRED_EXEC_API_KEY` 注入（见 `container_child_env`）。
fn whitelisted_env() -> Vec<(String, String)> {
    let mut envs = vec![("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string())];
    for key in ["PATH", "HOME", "LANG", "TZ", "TERM", "NO_PROXY", "no_proxy"] {
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

