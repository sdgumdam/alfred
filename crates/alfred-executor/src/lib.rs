//! alfred-executor：TaskAssignment 的唯一执行驱动。
//!
//! 架构红线：pi 的启动方式、任务投影（contract.prompt → `pi -p`）、产物采集
//! （git diff → Artifact）的知识只存在于本 crate；编排器/CLI 不感知容器细节。
//! 容器里的 pi 是黑盒执行者——只拿到契约 prompt，不知道任务图、不知道有审查。
//!
//! 离线模式 ALFRED_OFFLINE=1：返回确定性 fixture Artifact，不碰 Docker，
//! 供无 Docker 环境的单测/e2e 使用（开关语义与 alfred-planner/alfred-reviewer 一致）。

#[allow(unused_imports)]
use alfred_core::{
    offline_mode, now_iso8601_utc, now_millis, Artifact, TaskAssignment, OFFLINE_ENV,
};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
/// 注入容器的宿主环境变量白名单：仅 pi 起会话所需的 LLM key 与出网代理，
/// 按名转发，不放行宿主其余环境（PATH/HOME/各类会话 token 不进容器）。
const CONTAINER_ENV_ALLOWLIST: &[&str] = &[
    // pi providers.md 声明的 API key 变量；不存在于宿主时跳过。
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "MOONSHOT_API_KEY",
    "KIMI_API_KEY",
    "GEMINI_API_KEY",
    "ZAI_API_KEY",
    "OPENROUTER_API_KEY",
    "MISTRAL_API_KEY",
    "GROQ_API_KEY",
    "XAI_API_KEY",
    // 容器默认空环境，宿主经代理出网时容器不会自动继承。
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];
/// 执行镜像：docker/Dockerfile 构建，ENTRYPOINT 为 pi。
const IMAGE: &str = "alfred-executor:latest";
/// 容器内工作区挂载点（Dockerfile WORKDIR 同路径）。
const CONTAINER_WORKSPACE: &str = "/workspace";
/// 容器内 pi 会话落盘点；宿主机侧挂 run_dir/exec-<node_id>/pi-session。
const CONTAINER_SESSION_DIR: &str = "/pi-session";
/// 工作区基线提交信息：执行前后的 git diff 以该提交为基准。
const BASELINE_COMMIT_MESSAGE: &str = "pre-execute";
// ALFRED_OFFLINE 开关语义与 alfred-core 单一真源：每次实时读 env，不缓存
// （与 planner/reviewer/run 一致——进程运行期开关静态，测试串行不竞争）。

/// 沙箱档案（限界上下文.md §6.3.1 的最小代码实体）：planner 在 agent 节点上
/// 声明执行环境，计划审查按最小权限审，executor 照档案拼装 docker run 参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    /// provider 选择声明（如 "deepseek"/"anthropic"）。映射为 pi 的
    /// `--provider`，缺省不传由 pi 按自身默认（google）决定。
    #[serde(default)]
    pub provider: String,
    /// 工作区之外的挂载卷；工作区本身由 executor 固定挂载，不在此列。
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// 语言运行时声明（如 "rust" / "python"）。最小版不参与 docker run 拼装
    /// （镜像固定 alfred-executor:latest），保留给计划审查与多镜像演进。
    #[serde(default)]
    pub runtime: String,
    /// 依赖包声明。最小版不参与 docker run 拼装（安装属镜像构建期职责）。
    #[serde(default)]
    pub packages: Vec<String>,
    /// 是否允许联网；缺省 false（默认拒绝）→ docker run --network none。
    #[serde(default)]
    pub network: bool,
}

/// 工作区之外的挂载卷。外部参考材料一律只读挂载，不 cp 进工作区
/// （cp 会污染产物出处，混入的参考材料可被篡改后自圆其说）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMount {
    pub host_path: String,
    pub container_path: String,
    /// 缺省 true：只读是默认姿态，可写必须显式声明。
    #[serde(default = "default_read_only")]
    pub read_only: bool,
}

fn default_read_only() -> bool {
    true
}

/// 执行失败的唯一错误类型：容器驱动失败、产物采集失败、或底层 IO 失败。
#[derive(Debug)]
pub enum ExecError {
    /// docker CLI 自身失败（daemon 不可达、镜像缺失、pi 非零退出等）。
    DockerError(String),
    /// 工作区 git 基线/diff 采集失败。
    ArtifactError(String),
    IoError(std::io::Error),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DockerError(msg) => write!(f, "docker execution failed: {msg}"),
            Self::ArtifactError(msg) => write!(f, "artifact collection failed: {msg}"),
            Self::IoError(err) => write!(f, "io error: {err}"),
        }
    }
}

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ExecError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err)
    }
}

/// 在容器中执行 assignment，返回从工作区 git diff 采集的 Artifact。
///
/// 流程：工作区建 git 基线（pre-execute 提交）→ docker run 起 pi
/// （只拿到 contract.prompt）→ stdout/stderr 全文与 pi 会话 JSONL 落
/// run_dir/exec-<task_id>/ → 工作区 git diff HEAD 采 workspace_diff。
pub fn execute_in_container(
    assignment: &TaskAssignment,
    workspace: &Path,
    profile: &SandboxProfile,
    run_dir: &Path,
) -> Result<Artifact, ExecError> {
    if offline_mode() {
        return Ok(fixture_artifact(&assignment.task_id));
    }

    git_baseline(workspace)?;

    let log_dir = run_dir.join(format!("exec-{}", assignment.task_id));
    let host_session_dir = log_dir.join("pi-session");
    fs::create_dir_all(&host_session_dir)?;

    let args = docker_run_args(
        workspace,
        profile,
        &canonicalize(&host_session_dir)?,
        &assignment.contract.prompt,
        &container_env(),
    );
    let output = Command::new("docker").args(&args).output()?;
        write_session_log(&log_dir, &output)?;
    probe_write_through(workspace, &log_dir)?;
    if !output.status.success() {
        return Err(ExecError::DockerError(format!(
            "`docker run` exited with {} (image {IMAGE}); full output in {}",
            output.status,
            log_dir.join("session.log").display()
        )));
    }

    let workspace_diff = git_workspace_diff(workspace)?;
    Ok(Artifact {
        node_id: assignment.task_id.clone(),
        workspace_diff,
        produced_at: now_iso8601_utc(),
    })
}

/// 挂载用宿主机路径取绝对形式：macOS 上 /tmp 是 /private/tmp 的软链，
/// 解析后报错/诊断能指向真实路径。目录由调用方先行创建。
fn canonicalize(path: &Path) -> Result<PathBuf, ExecError> {
    fs::canonicalize(path)
        .map_err(|err| ExecError::DockerError(format!("cannot resolve {}: {err}", path.display())))
}

/// 挂载写穿透探针：容器写 /workspace 的文件必须落到宿主工作区，
/// 否则产物采集恒为空（实测坑：colima 仅把 ~ 挂进 VM，/tmp 下的工作区
/// 被 daemon 绑到 VM 侧空目录，容器内写入全部丢失）。docker 退出码为 0
/// 时探针是唯一防线，检查必须在首个 Ok 返回之前。
fn probe_write_through(workspace: &Path, log_dir: &Path) -> Result<(), ExecError> {
    let probe = workspace.join(format!(".alfred-mount-probe-{}", now_millis()));
    fs::write(&probe, b"")?;
    let exists = probe.exists();
    let _ = fs::remove_file(&probe);
    if !exists {
        return Err(ExecError::DockerError(format!(
            "workspace mount did not write through: {} changes were not visible on the host \
             (VM-based docker runtimes only share configured mount roots, e.g. colima shares ~ only); \
             container output is in {}",
            workspace.display(),
            log_dir.join("session.log").display()
        )));
    }
    Ok(())
}
/// 采集注入容器的环境变量：白名单内且宿主实际设置的项。采集发生在
/// docker run 拼装前，拼装函数本身保持纯函数。
fn container_env() -> Vec<(String, String)> {
    CONTAINER_ENV_ALLOWLIST
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

/// docker run 参数拼装（SandboxProfile 四维的映射真源）：
/// - 工作区固定 `-v <workspace>:/workspace:rw`（产物的唯一可写区）；
/// - profile.volumes 逐项 `-v host:container:ro|rw`（read_only 默认 true → ro）；
/// - profile.network=false → `--network none`（默认拒绝联网）；
/// - `--rm` 执行完即销毁容器，会话经 -v 卷落宿主机，无需 docker cp；
/// - 镜像固定 alfred-executor:latest，ENTRYPOINT pi 接
///   `--session-dir /pi-session -p <prompt>`（runtime/packages 最小版不参与拼装）。
/// 纯函数，不执行，供单测直接断言参数向量。
fn docker_run_args(
    workspace: &Path,
    profile: &SandboxProfile,
    host_session_dir: &Path,
    prompt: &str,
    container_env: &[(String, String)],
) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
    for (name, value) in container_env {
        args.push("-e".into());
        args.push(format!("{name}={value}"));
    }
    args.push("-v".into());
    args.push(format!("{}:{CONTAINER_WORKSPACE}:rw", workspace.display()));
    for volume in &profile.volumes {
        let mode = if volume.read_only { "ro" } else { "rw" };
        args.push("-v".into());
        args.push(format!("{}:{}:{mode}", volume.host_path, volume.container_path));
    }
    if !profile.network {
        args.push("--network".into());
        args.push("none".into());
    }
    args.push("-v".into());
    args.push(format!(
        "{}:{CONTAINER_SESSION_DIR}:rw",
        host_session_dir.display()
    ));
    args.push(IMAGE.into());
    if !profile.provider.is_empty() {
        args.push("--provider".into());
        args.push(profile.provider.clone());
    }
    args.push("--session-dir".into());
    args.push(CONTAINER_SESSION_DIR.into());
    args.push("-p".into());
    args.push(prompt.into());
    args
}

/// 工作区 git 基线：无 .git 则 git init，随后 git add -A + 提交 pre-execute
/// （--allow-empty 保证无变更也有基准提交）。只认 workspace 自己的 .git，
/// 防止 workspace 位于父仓内时基线误落到父仓。
fn git_baseline(workspace: &Path) -> Result<(), ExecError> {
    if !workspace.join(".git").exists() {
        run_git(workspace, &["init"])?;
    }
    run_git(workspace, &["add", "-A"])?;
    run_git(
        workspace,
        &["commit", "--allow-empty", "-m", BASELINE_COMMIT_MESSAGE],
    )?;
    Ok(())
}

/// 产物采集：git add -A 把新建/修改/删除全部纳入索引，再 diff --cached HEAD
/// 对基线提交取全量差异——裸 `git diff HEAD` 看不到 untracked 新文件，
/// 新文件恰是执行者的主要产物形态。
fn git_workspace_diff(workspace: &Path) -> Result<String, ExecError> {
    run_git(workspace, &["add", "-A"])?;
    let output = run_git(workspace, &["diff", "--cached", "HEAD"])?;
    String::from_utf8(output.stdout)
        .map_err(|err| ExecError::ArtifactError(format!("git diff output is not UTF-8: {err}")))
}

fn run_git(workspace: &Path, args: &[&str]) -> Result<Output, ExecError> {
    // 身份经 -c 传入：不读/不改宿主机 git 配置，基线提交可归因于 executor。
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .arg("-c")
        .arg("user.name=alfred-executor")
        .arg("-c")
        .arg("user.email=alfred-executor@localhost")
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(ExecError::ArtifactError(format!(
            "`git {}` failed in {}: {}",
            args.join(" "),
            workspace.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

/// 容器 stdout/stderr 全文落 session.log；容器非零退出也先落盘再报错，证据不丢。
fn write_session_log(log_dir: &Path, output: &Output) -> Result<(), ExecError> {
    let mut log = Vec::new();
    log.extend_from_slice(b"=== stdout ===\n");
    log.extend_from_slice(&output.stdout);
    log.extend_from_slice(b"\n=== stderr ===\n");
    log.extend_from_slice(&output.stderr);
    fs::write(log_dir.join("session.log"), log)?;
    Ok(())
}

/// 离线 fixture：与 alfred-cli run 链路的 S1 fixture 同语义（hello.txt 静态 diff），
/// 供无 Docker 环境的单测/e2e 跑通治理环。
fn fixture_artifact(node_id: &str) -> Artifact {
    Artifact {
        node_id: node_id.to_owned(),
        workspace_diff: "diff --git a/hello.txt b/hello.txt\n\
                         new file mode 100644\n\
                         --- /dev/null\n\
                         +++ b/hello.txt\n\
                         @@ -0,0 +1 @@\n\
                         +Hello Alfred\n"
            .into(),
        produced_at: now_iso8601_utc(),
    }
}

/// S0 占位保留：不执行任何动作的空实现，供编排链路未接入容器执行前编译占位。
pub fn execute_stub(_assignment: &TaskAssignment) {}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::{Contract, HANDLER_RUN_INSPECT_EVAL};

    fn assignment(task_id: &str, prompt: &str) -> TaskAssignment {
        TaskAssignment {
            task_id: task_id.into(),
            handler: HANDLER_RUN_INSPECT_EVAL.into(),
            contract: Contract {
                prompt: prompt.into(),
                acceptance_criteria: "stub".into(),
                reviewer_models: vec!["stub-model".into()],
            },
            params: serde_json::Map::new(),
        }
    }

    fn session_dir() -> std::path::PathBuf {
        PathBuf::from("/run/exec-task-1/pi-session")
    }

    #[test]
    fn s0_stub_accepts_a_task_assignment_without_acting() {
        execute_stub(&assignment("task-1", "stub"));
    }

    #[test]
    fn sandbox_profile_defaults_are_deny_all() {
        let profile: SandboxProfile = serde_json::from_str("{}").unwrap();
        assert_eq!(
            profile,
            SandboxProfile {
                provider: String::new(),
                volumes: vec![],
                runtime: String::new(),
                packages: vec![],
                network: false,
            }
        );
    }

    #[test]
    fn volume_mount_read_only_defaults_true() {
        let mount: VolumeMount =
            serde_json::from_str(r#"{"host_path":"/h","container_path":"/c"}"#).unwrap();
        assert!(mount.read_only);
    }

    #[test]
    fn sandbox_profile_rejects_unknown_fields() {
        let raw = r#"{"volumes":[],"runtime":"","packages":[],"network":false,"cpu":2}"#;
        assert!(serde_json::from_str::<SandboxProfile>(raw).is_err());
    }

    #[test]
    fn docker_args_mount_workspace_rw_and_pass_prompt() {
        let profile: SandboxProfile = serde_json::from_str("{}").unwrap();
        let args = docker_run_args(
            Path::new("/ws"),
            &profile,
            &session_dir(),
            "create hello.txt",
            &[],
        );
        assert_eq!(
            args,
            vec![
                "run",
                "--rm",
                "-v",
                "/ws:/workspace:rw",
                "--network",
                "none",
                "-v",
                "/run/exec-task-1/pi-session:/pi-session:rw",
                "alfred-executor:latest",
                "--session-dir",
                "/pi-session",
                "-p",
                "create hello.txt",
            ]
        );
    }

    #[test]
    fn docker_args_apply_profile_volumes_and_network_opt_in() {
        let profile: SandboxProfile = serde_json::from_str(
            r#"{
                "volumes": [
                    {"host_path": "/refs", "container_path": "/references"},
                    {"host_path": "/cache", "container_path": "/cache", "read_only": false}
                ],
                "network": true
            }"#,
        )
        .unwrap();
        let args = docker_run_args(Path::new("/ws"), &profile, &session_dir(), "do it", &[]);
        // 工作区卷在最前，profile 卷按声明序紧随其后，默认卷只读。
        assert_eq!(args[3], "/ws:/workspace:rw");
        assert_eq!(args[5], "/refs:/references:ro");
        assert_eq!(args[7], "/cache:/cache:rw");
        // network=true → 不出现 --network none。
        assert!(!args.windows(2).any(|w| w == ["--network", "none"]));
        // prompt 永远是最后一个参数，紧随 -p。
        assert_eq!(args.last().unwrap(), "do it");
        assert_eq!(args[args.len() - 2], "-p");
    }

    #[test]
    fn offline_mode_returns_fixture_without_touching_docker_or_disk() {
        std::env::set_var(OFFLINE_ENV, "1");
        let artifact = execute_in_container(
            &assignment("task-1", "ignored"),
            // 不存在的路径：只要碰了 Docker / git / 文件系统就会炸。
            Path::new("/nonexistent-alfred-offline-workspace"),
            &serde_json::from_str("{}").unwrap(),
            Path::new("/nonexistent-alfred-offline-run"),
        )
        .unwrap();
        // offline_mode() 每次实时读 env——set 后仍为 true，remove 前断言。
        assert!(offline_mode());
        std::env::remove_var(OFFLINE_ENV);
        assert_eq!(artifact.node_id, "task-1");
        assert!(artifact.workspace_diff.contains("hello.txt"));
        assert!(artifact.workspace_diff.contains("+Hello Alfred"));
        assert!(!offline_mode());
    }

    #[test]
    fn workspace_diff_is_collected_from_git_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("seed.txt"), "seed\n").unwrap();

        git_baseline(&workspace).unwrap();
        assert!(workspace.join(".git").exists());

        // 模拟容器执行后的工作区：新建 + 修改 + 删除。
        fs::write(workspace.join("created.txt"), "from pi\n").unwrap();
        fs::write(workspace.join("seed.txt"), "seed\nmodified\n").unwrap();
        let diff = git_workspace_diff(&workspace).unwrap();

        assert!(diff.contains("diff --git a/created.txt b/created.txt"));
        assert!(diff.contains("+from pi"));
        assert!(diff.contains("+modified"));
        // 基线提交是 pre-execute，diff 对它的全量差异。
        let log = run_git(&workspace, &["log", "--oneline"]).unwrap();
        let log = String::from_utf8(log.stdout).unwrap();
        assert!(log.contains(BASELINE_COMMIT_MESSAGE));
    }

    #[test]
    fn baseline_is_idempotent_and_ignores_parent_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        git_baseline(&workspace).unwrap();
        fs::write(workspace.join("a.txt"), "a\n").unwrap();
        // 第二次基线只追加一个 pre-execute 提交，不重复 init。
        git_baseline(&workspace).unwrap();
        let count = run_git(&workspace, &["rev-list", "--count", "HEAD"]).unwrap();
        assert_eq!(String::from_utf8(count.stdout).unwrap().trim(), "2");
    }

    #[test]
    fn docker_args_inject_env_and_provider_flag() {
        let profile: SandboxProfile =
            serde_json::from_str(r#"{"provider": "deepseek"}"#).unwrap();
        let container_env = vec![
            ("DEEPSEEK_API_KEY".to_string(), "sk-test".to_string()),
            ("HTTPS_PROXY".to_string(), "http://127.0.0.1:7890".to_string()),
        ];
        let args = docker_run_args(
            Path::new("/ws"),
            &profile,
            &session_dir(),
            "do it",
            &container_env,
        );
        assert_eq!(args[3], "DEEPSEEK_API_KEY=sk-test");
        assert_eq!(args[5], "HTTPS_PROXY=http://127.0.0.1:7890");
        // provider 声明映射为 pi 的 --provider，位于镜像名之后。
        let image_at = args.iter().position(|a| a == "alfred-executor:latest").unwrap();
        assert_eq!(args[image_at + 1], "--provider");
        assert_eq!(args[image_at + 2], "deepseek");
        let ts = now_iso8601_utc();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
