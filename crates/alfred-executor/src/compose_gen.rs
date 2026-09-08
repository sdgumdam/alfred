//! 沙箱 compose 生成（per-run）。
//!
//! 环境事实（实施计划 §三 E1/E3）：
//! - `docker run -v` 相对路径会被静默解析为 named volume → 挂载路径必须先
//!   canonicalize 成绝对路径。
//! - macOS `/tmp` 是软链、colima 只共享 `~` → run 目录必须位于 `~` 之下，
//!   canonicalize 后路径才与 docker/报错信息一致。
//!
//! R6a（三容器挂载矩阵）：`generate_executor_compose` 落矩阵 §1.1 executor 行——
//! `workspace_subdirs` 子集投影（非空挂载，R6e 块B：空声明防御性报错）+ 参考卷 ro
//! + AGT 挂载（planner/reviewer 已宿主 pi 化，无 compose；reviewer 模板句由
//!   reviewer 线处置）。

use std::path::{Path, PathBuf};

use alfred_core::contract::VolumeMount;
use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Serialize)]
struct ComposeFile {
    services: Services,
}

#[derive(Serialize)]
struct Services {
    default: Service,
}

#[derive(Serialize)]
struct Service {
    image: String,
    command: String,
    init: bool,
    network_mode: String,
    stop_grace_period: String,
    volumes: Vec<String>,
}

/// 容器内工作区路径（固定，执行驱动专用挂载）。
pub const CONTAINER_WORKSPACE_DIR: &str = "/workspace";

/// executor 容器挂载参数（R6a：矩阵 §1.1 executor 行落码）。
///
/// - `workspace_subdirs`：契约声明的工作区子目录（相对持久 ws）。**非空必挂**
///   （R6e 块B：executor ws 挂载非空保证）；空 = 防御性报错（计划审查应打回
///   重规划，executor 不静默跳过、不静默挂全量）。
/// - `ref_volumes`：只读参考卷（`SandboxProfile.volumes`，档案声明）。
/// - `agt_dir`：AGT 策略目录（policy.json/agt-policy.ts，挂到 `/tmp/.agt`，ro）。
/// - `agt_audit_dir`：AGT 审计输出子目录（挂到 `/tmp/.agt/audit`，rw——
///   审计 JSONL 落此）。策略与审计拆开挂载：agent 可写审计但不可改策略（R6a）。
#[derive(Debug, Clone, Default)]
pub struct ExecutorMounts {
    pub workspace_subdirs: Vec<String>,
    pub ref_volumes: Vec<VolumeMount>,
    pub agt_dir: Option<PathBuf>,
    pub agt_audit_dir: Option<PathBuf>,
}

/// 校验 workspace subdir 声明：必须相对、非空、不含 `.`/`..`（防 rw 挂载逃逸持久 ws，
/// 把 rw 挂载静默换基到宿主任意目录）。`generate_executor_compose` 与 run.rs 预建
/// 子目录共用（单一真源，代码质量红线 1）。
pub fn validate_workspace_subdir(sub: &str) -> Result<()> {
    let sub_path = Path::new(sub);
    if sub.is_empty() || sub_path.is_absolute() {
        bail!("workspace subdir must be a relative path, got: '{sub}'（绝对/空路径禁止）");
    }
    if sub_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
    {
        bail!("workspace subdir must not contain '..' or '.': '{sub}'（越界路径禁止）");
    }
    Ok(())
}

/// 校验 workspace_subdirs 列表声明：逐项 [`validate_workspace_subdir`] + 首子目录名
/// 重复声明拒绝（A4 挂载锚窄边界）。
///
/// 挂载语义（task_gen::mount_anchor_prompt）：workspace_subdirs[0] 直接挂为执行者
/// /workspace 根，锚断言「/workspace 下不存在同名嵌套子目录」。首名若在其余位
/// 重复声明，compose 会把同一宿主目录双挂载（/workspace 与 /workspace/<同名>
/// 指向同一宿主目录）——锚断言与执行者所见矛盾（执行者透过挂载看见
/// /workspace/<同名>/）。显式报错防重复挂载，不静默去重（重复声明是计划缺陷，
/// 计划审查应打回重规划）。`validate_executor_sandbox` 与
/// `generate_executor_compose` 共用（单一真源，代码质量红线 1）。
pub fn validate_workspace_subdirs(subs: &[String]) -> Result<()> {
    for sub in subs {
        validate_workspace_subdir(sub)?;
    }
    if let Some(first) = subs.first() {
        if subs.iter().skip(1).any(|s| s == first) {
            bail!(
                "workspace_subdirs 首子目录名 '{first}' 在其余位重复声明：重复项与首挂载（/workspace 根）指向同一宿主目录造成双挂载，且与挂载锚「/workspace 下不存在同名嵌套子目录」矛盾（拒绝重复声明，不静默去重）"
            );
        }
    }
    Ok(())
}

/// 校验只读参考卷声明（9/3 方案②，放行条件真源——`validate_executor_sandbox`
/// 与 `generate_executor_compose` 共用，单一真源）：
///
/// - `mode` 必须为 `"ro"`：参考卷一律只读（契约 §2.5"一律只读、不 cp 进工作区"；
///   缺省 ro 由 serde default 保证，显式非 ro 值在此拒绝）。
/// - `host_path` 必须绝对且存在于宿主：相对路径被 docker 静默变 named volume
///   （E1 防呆）；宿主材料不存在 = 计划缺陷（防御性报错，不静默跳过挂载——
///   防"申请的参考材料没生效"）。
/// - `container_path` 必须绝对、非空、不含 `..`/`.`（挂载点合法性；禁 `/workspace`
///   与 `/tmp/.agt` 保留挂载点冲突——工作区 rw 面与 AGT 策略面不被参考卷遮蔽）。
pub fn validate_ref_volume(vol: &VolumeMount) -> Result<()> {
    if vol.mode != "ro" {
        bail!(
            "ref volume mode must be 'ro' (参考卷一律只读), got: '{}'",
            vol.mode
        );
    }
    let host = Path::new(&vol.host_path);
    if !host.is_absolute() {
        bail!(
            "ref volume host_path must be absolute: {} (E1: relative -v silently becomes a named volume)",
            vol.host_path
        );
    }
    if !host.exists() {
        bail!(
            "ref volume host_path does not exist on host: {}（宿主参考材料缺失 = 计划缺陷，拒绝静默跳过）",
            vol.host_path
        );
    }
    let target = Path::new(&vol.container_path);
    if vol.container_path.is_empty() || !target.is_absolute() {
        bail!(
            "ref volume container_path must be an absolute path, got: '{}'",
            vol.container_path
        );
    }
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
    {
        bail!(
            "ref volume container_path must not contain '..' or '.': '{}'",
            vol.container_path
        );
    }
    for reserved in ["/workspace", "/tmp/.agt"] {
        if target == Path::new(reserved) || target.starts_with(reserved) {
            bail!(
                "ref volume container_path '{}' conflicts with reserved mount point '{reserved}'",
                vol.container_path
            );
        }
    }
    Ok(())
}

/// 生成 executor 容器 compose：network none + `workspace_subdirs` 投影 + 参考卷 ro
/// + AGT 挂载（R6a：矩阵 §1.1 executor 行落码）。
///
/// 挂载布局（对齐方案 v2 §二.1 executor 示例）：
/// - 首个 subdir → `/workspace`（rw，契约声明的子目录即执行者工作区根）；
/// - 其余 subdir → `/workspace/<subdir>`（rw）；
/// - 参考卷 → `<container_path>`（ro，按 `VolumeMount` 声明）；
/// - AGT 策略目录 → `/tmp/.agt`（ro，agent 不可改策略）；
/// - AGT 审计子目录 → `/tmp/.agt/audit`（rw，审计 JSONL 落此）。
/// 空 subdirs → 防御性报错（R6e 块B：executor ws 挂载非空保证——空声明是计划
/// 缺陷，计划审查应打回重规划；executor 不静默跳过、不静默回退挂全量）。
/// 首名重复声明 → 显式报错（A4 挂载锚：防双挂载同宿主目录 + 锚断言矛盾）。
pub fn generate_executor_compose(
    workspace_host_dir: &Path,
    image: &str,
    mounts: &ExecutorMounts,
) -> Result<String> {
    let abs = canonicalize_workspace(workspace_host_dir)?;
    let mut volumes: Vec<String> = Vec::new();
    if mounts.workspace_subdirs.is_empty() {
        // R6e 块B：executor 挂载非空保证——空声明是计划缺陷（计划审查应打回
        // 重规划），executor 侧防御性失败：不静默跳过挂载、不静默回退挂全量。
        bail!(
            "executor 沙箱 workspace_subdirs 为空：计划审查应拦截，executor 挂载不能为空（拒绝空声明，不挂全量）"
        );
    }
    // R6a + A4：逐项相对/越界 + 首名重复声明拒绝（列表级校验与 run.rs
    // `validate_executor_sandbox` 共用 `validate_workspace_subdirs`，单一真源）。
    validate_workspace_subdirs(&mounts.workspace_subdirs)?;
    for (i, sub) in mounts.workspace_subdirs.iter().enumerate() {
        let host = abs.join(sub);
        if !host.exists() {
            bail!(
                "workspace subdir '{}' does not exist under {} (declared in workspace_subdirs)",
                sub,
                abs.display()
            );
        }
        let target = if i == 0 {
            CONTAINER_WORKSPACE_DIR.to_string()
        } else {
            format!("{}/{}", CONTAINER_WORKSPACE_DIR, sub)
        };
        volumes.push(format!("{}:{}:rw", host.display(), target));
    }
    for vol in &mounts.ref_volumes {
        // 9/3 方案②：逐卷校验真源（mode ro + host 存在 + container 合法）与
        // validate_executor_sandbox 共用（单一真源）。
        validate_ref_volume(vol)?;
        let host_abs = Path::new(&vol.host_path)
            .canonicalize()
            .with_context(|| format!("canonicalize ref volume host {}", vol.host_path))?;
        volumes.push(format!("{}:{}:ro", host_abs.display(), vol.container_path));
    }
    if let Some(agt) = &mounts.agt_dir {
        let agt_abs = agt
            .canonicalize()
            .with_context(|| format!("canonicalize agt dir {}", agt.display()))?;
        // R6a：策略目录只读——agent 不可改策略文件。
        volumes.push(format!("{}:/tmp/.agt:ro", agt_abs.display()));
    }
    if let Some(audit) = &mounts.agt_audit_dir {
        let audit_abs = audit
            .canonicalize()
            .with_context(|| format!("canonicalize agt audit dir {}", audit.display()))?;
        // R6a：审计输出子目录 rw——agent 可写审计但不可改策略（拆开挂载）。
        volumes.push(format!("{}:/tmp/.agt/audit:rw", audit_abs.display()));
    }
    let compose = ComposeFile {
        services: Services {
            default: Service {
                image: image.to_string(),
                command: "tail -f /dev/null".to_string(),
                init: true,
                network_mode: "none".to_string(),
                stop_grace_period: "1s".to_string(),
                volumes,
            },
        },
    };
    serde_yaml::to_string(&compose).context("serialize compose yaml")
}

/// canonicalize 工作区宿主目录；存在性/可访问性校验。
pub fn canonicalize_workspace(dir: &Path) -> Result<std::path::PathBuf> {
    if !dir.exists() {
        bail!(
            "workspace host dir does not exist: {} (must be created before compose)",
            dir.display()
        );
    }
    let abs = dir
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {}", dir.display()))?;
    // E1/E3 防呆：拒绝不在 HOME 下的 run 目录（colima 只共享 ~，挂不进去）
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    if !abs.starts_with(&home) {
        bail!(
            "workspace host dir {} is outside HOME ({}) — colima 只共享 ~，挂载会静默失败",
            abs.display(),
            home
        );
    }
    Ok(abs)
}
