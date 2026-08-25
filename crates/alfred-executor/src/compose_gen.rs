//! 沙箱 compose 生成（per-run）。
//!
//! 环境事实（实施计划 §三 E1/E3）：
//! - `docker run -v` 相对路径会被静默解析为 named volume → 挂载路径必须先
//!   canonicalize 成绝对路径。
//! - macOS `/tmp` 是软链、colima 只共享 `~` → run 目录必须位于 `~` 之下，
//!   canonicalize 后路径才与 docker/报错信息一致。

use std::path::Path;

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

/// 生成沙箱 compose：network none（P2）+ 工作区卷（绝对路径）。
pub fn generate_compose(workspace_host_dir: &Path, image: &str) -> Result<String> {
    let abs = canonicalize_workspace(workspace_host_dir)?;
    let compose = ComposeFile {
        services: Services {
            default: Service {
                image: image.to_string(),
                command: "tail -f /dev/null".to_string(),
                init: true,
                network_mode: "none".to_string(),
                stop_grace_period: "1s".to_string(),
                volumes: vec![format!("{}:{}", abs.display(), CONTAINER_WORKSPACE_DIR)],
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
    fn compose_has_network_none_and_workspace_volume() {
        // 测试目录须位于 HOME 下（E3：colima 只共享 ~）
        let home = std::env::var("HOME").unwrap();
        let dir = Path::new(&home).join(".local/state/alfred/test-compose");
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = generate_compose(&dir, "alfred-executor:latest").unwrap();
        assert!(yaml.contains("network_mode: none"), "got:\n{yaml}");
        assert!(yaml.contains("alfred-executor:latest"));
        // 绝对路径必须出现在卷里（E1：相对路径被 docker 静默变 named volume）
        let abs = dir.canonicalize().unwrap();
        assert!(
            yaml.contains(&format!("{}:/workspace", abs.display())),
            "got:\n{yaml}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn canonicalize_rejects_missing_dir() {
        let home = std::env::var("HOME").unwrap();
        let missing = Path::new(&home)
            .join(".local/state/alfred/does-not-exist-xyz");
        assert!(canonicalize_workspace(&missing).is_err());
    }
}
