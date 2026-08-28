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
//! + AGT 挂载；planner/reviewer 模板见 `docker/planner.compose.yaml.tmpl` /
//! `docker/reviewer.compose.yaml.tmpl`（静态模板，编排器渲染占位符）。

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
    for (i, sub) in mounts.workspace_subdirs.iter().enumerate() {
        // R6a：子目录必须相对且不越界（校验与 run.rs 预建子目录共用
        // `validate_workspace_subdir`，单一真源）。
        validate_workspace_subdir(sub)?;
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
        // E1 防呆：参考卷宿主路径必须绝对（相对路径被 docker 静默变 named volume）
        let host = Path::new(&vol.host_path);
        if !host.is_absolute() {
            bail!(
                "ref volume host_path must be absolute: {} (E1: relative -v silently becomes a named volume)",
                vol.host_path
            );
        }
        let host_abs = host
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_subdir_rejects_absolute_and_empty() {
        // R6e：绝对/空路径禁止（防 rw 挂载静默换基到宿主任意目录）
        assert!(validate_workspace_subdir("/etc").unwrap_err().to_string().contains("relative path"));
        assert!(validate_workspace_subdir("").unwrap_err().to_string().contains("relative path"));
    }

    #[test]
    fn validate_subdir_rejects_parent_dir_and_cur_dir() {
        // R6e：`..` / `.` 越界路径禁止
        assert!(validate_workspace_subdir("../escape").unwrap_err().to_string().contains(".."));
        assert!(validate_workspace_subdir("./x").unwrap_err().to_string().contains("'..' or '.'"));
    }

    #[test]
    fn validate_subdir_accepts_relative_simple() {
        // R6e：相对简单子目录通过
        assert!(validate_workspace_subdir("src").is_ok());
        assert!(validate_workspace_subdir("src/deep/nested").is_ok());
    }

    #[test]
    fn canonicalize_rejects_missing_dir() {
        let home = std::env::var("HOME").unwrap();
        let missing = Path::new(&home)
            .join(".local/state/alfred/does-not-exist-xyz");
        assert!(canonicalize_workspace(&missing).is_err());
    }

    // ---- R6a：executor workspace_subdirs 投影（矩阵 §1.1 executor 行） ----

    fn home_dir(tag: &str) -> PathBuf {
        let home = std::env::var("HOME").unwrap();
        let dir = Path::new(&home).join(format!(".local/state/alfred/test-exec-compose-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn executor_compose_mounts_subdirs_rw() {
        let ws = home_dir("subdirs");
        let src = ws.join("src");
        let tests = ws.join("tests");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&tests).unwrap();
        let mounts = ExecutorMounts {
            workspace_subdirs: vec!["src".to_string(), "tests".to_string()],
            ..Default::default()
        };
        let yaml = generate_executor_compose(&ws, "alfred-executor:latest", &mounts).unwrap();
        let abs = ws.canonicalize().unwrap();
        // 首个 subdir → /workspace 根（rw）；其余 → /workspace/<subdir>（rw）
        assert!(
            yaml.contains(&format!("{}:/workspace:rw", abs.join("src").display())),
            "got:\n{yaml}"
        );
        assert!(
            yaml.contains(&format!("{}:/workspace/tests:rw", abs.join("tests").display())),
            "got:\n{yaml}"
        );
        assert!(yaml.contains("network_mode: none"), "got:\n{yaml}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn executor_compose_empty_subdirs_errors() {
        // R6e 块B：executor ws 挂载非空保证——空 workspace_subdirs 防御性报错
        // （不静默跳过挂载、不静默回退挂全量）
        let ws = home_dir("empty");
        let mounts = ExecutorMounts::default();
        let err = generate_executor_compose(&ws, "alfred-executor:latest", &mounts).unwrap_err();
        assert!(
            err.to_string().contains("workspace_subdirs 为空"),
            "空 subdirs 必须报错，got: {err}"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn executor_compose_mounts_ref_volumes_ro_and_agt() {
        let ws = home_dir("refs");
        let refs = ws.join("refs");
        std::fs::create_dir_all(&refs).unwrap();
        let agt = ws.join(".agt");
        std::fs::create_dir_all(&agt).unwrap();
        let agt_audit = agt.join("audit");
        std::fs::create_dir_all(&agt_audit).unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let mounts = ExecutorMounts {
            workspace_subdirs: vec!["src".to_string()],
            ref_volumes: vec![VolumeMount {
                host_path: refs.display().to_string(),
                container_path: "/references/docs".to_string(),
            }],
            agt_dir: Some(agt.clone()),
            agt_audit_dir: Some(agt_audit.clone()),
        };
        let yaml = generate_executor_compose(&ws, "alfred-executor:latest", &mounts).unwrap();
        // 参考卷 ro
        assert!(
            yaml.contains(&format!(
                "{}:/references/docs:ro",
                refs.canonicalize().unwrap().display()
            )),
            "got:\n{yaml}"
        );
        // R6a：AGT 策略目录 ro（agent 不可改策略）
        assert!(
            yaml.contains(&format!(
                "{}:/tmp/.agt:ro",
                agt.canonicalize().unwrap().display()
            )),
            "got:\n{yaml}"
        );
        // R6a：AGT 审计输出子目录 rw（agent 可写审计）
        assert!(
            yaml.contains(&format!(
                "{}:/tmp/.agt/audit:rw",
                agt_audit.canonicalize().unwrap().display()
            )),
            "got:\n{yaml}"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn executor_compose_rejects_relative_ref_volume() {
        // E1 防呆：参考卷宿主路径必须绝对（相对路径被 docker 静默变 named volume）
        let ws = home_dir("relref");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let mounts = ExecutorMounts {
            workspace_subdirs: vec!["src".to_string()],
            ref_volumes: vec![VolumeMount {
                host_path: "relative/path".to_string(),
                container_path: "/references/x".to_string(),
            }],
            ..Default::default()
        };
        let err = generate_executor_compose(&ws, "img", &mounts).unwrap_err();
        assert!(err.to_string().contains("must be absolute"), "got: {err}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn executor_compose_rejects_missing_subdir() {
        let ws = home_dir("missing-sub");
        let mounts = ExecutorMounts {
            workspace_subdirs: vec!["does-not-exist".to_string()],
            ..Default::default()
        };
        let err = generate_executor_compose(&ws, "img", &mounts).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn executor_compose_rejects_absolute_subdir() {
        // R6a：子目录含绝对路径 → 报错（防静默换基 rw 挂载到宿主任意目录）
        let ws = home_dir("abs-sub");
        let mounts = ExecutorMounts {
            workspace_subdirs: vec!["/etc".to_string()],
            ..Default::default()
        };
        let err = generate_executor_compose(&ws, "img", &mounts).unwrap_err();
        assert!(err.to_string().contains("relative path"), "got: {err}");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn executor_compose_rejects_parent_dir_subdir() {
        // R6a：子目录含 `..` → 报错（防 rw 挂载逃逸持久 ws）
        let ws = home_dir("parent-sub");
        let mounts = ExecutorMounts {
            workspace_subdirs: vec!["../escape".to_string()],
            ..Default::default()
        };
        let err = generate_executor_compose(&ws, "img", &mounts).unwrap_err();
        assert!(err.to_string().contains(".."), "got: {err}");
        std::fs::remove_dir_all(&ws).ok();
    }

    // ---- R6a：planner/reviewer 模板 = 矩阵行落码（docker/*.compose.yaml.tmpl） ----

    fn docker_template(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docker")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    #[test]
    fn planner_template_encodes_planner_matrix_row() {
        let text = docker_template("planner.compose.yaml.tmpl");
        // ws 全量 ro + OwnerRequest + 会话文档 + 契约（自己写的）
        assert!(text.contains("{ws}:/workspace:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/request.json:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/session.json:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/contract.json:ro"), "got:\n{text}");
        // 不挂 conversation.json（审查者独有，矩阵 §1.1 planner 行 ❌）——
        // 断言挂载目标不存在（注释里允许出现该词说明设计，但不得有挂载行）
        assert!(
            !text.contains("/inputs/conversation.json"),
            "planner 不得挂 conversation 卷，got:\n{text}"
        );
        // network none + AGT 拦写层 ro + 输出卷
        assert!(text.contains("network_mode: none"), "got:\n{text}");
        assert!(text.contains("/tmp/.agt:ro"), "got:\n{text}");
        assert!(text.contains("{outputs_dir}:/outputs"), "got:\n{text}");
    }

    #[test]
    fn reviewer_template_encodes_reviewer_matrix_row() {
        let text = docker_template("reviewer.compose.yaml.tmpl");
        // ws 全量 ro + conversation（reviewer 独有）+ OwnerRequest + 会话文档 + 契约全字段
        assert!(text.contains("{ws}:/workspace:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/conversation.json:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/request.json:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/session.json:ro"), "got:\n{text}");
        assert!(text.contains("/inputs/contract.json:ro"), "got:\n{text}");
        // R6a：reviewer 挂完整会话文档真源（{session_full_path}），不绑 planner 投影（{session_path}）
        assert!(
            text.contains("{session_full_path}:/inputs/session.json:ro"),
            "reviewer 必须挂完整会话文档真源（{{session_full_path}}），got:\n{text}"
        );
        assert!(
            !text.contains("{session_path}:/inputs/session.json:ro"),
            "reviewer 不得绑 planner 的投影会话文件（{{session_path}}），got:\n{text}"
        );
        // network none + AGT 拦写层 ro + 输出卷
        assert!(text.contains("network_mode: none"), "got:\n{text}");
        assert!(text.contains("/tmp/.agt:ro"), "got:\n{text}");
        assert!(text.contains("{outputs_dir}:/outputs"), "got:\n{text}");
    }
}
