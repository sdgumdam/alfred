//! 产物采集：容器 workspace 卷（R6e：挂 run 级单一持久 ws `<run>/ws`，绝对路径）的
//! 文件比对。
//!
//! 执行前后对工作区做快照（相对路径 + 大小 + SHA-256），差异即执行者产物。
//! 跳过符号链接（防产物逃逸工作区）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use alfred_core::artifact::{Artifact, ChangeKind, FileChange, FileEntry};
use sha2::{Digest, Sha256};

/// 对工作区目录做快照。
pub fn snapshot_workspace(dir: &Path) -> Result<BTreeMap<String, FileEntry>> {
    let mut map = BTreeMap::new();
    walk(dir, dir, &mut map)?;
    Ok(map)
}

fn walk(root: &Path, dir: &Path, map: &mut BTreeMap<String, FileEntry>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            // 跳过符号链接：产物采集只认普通文件，防逃逸
            continue;
        }
        if ft.is_dir() {
            // VCS internals and installed dependency trees are environment
            // state, not executor source artifacts (same boundary as inputs).
            if matches!(path.file_name().and_then(|s| s.to_str()),
                Some(".git" | ".venv" | "node_modules")) {
                continue;
            }
            walk(root, &path, map)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .context("path outside workspace root")?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read file {}", path.display()))?;
            let mut h = Sha256::new();
            h.update(&bytes);
            let sha256 = hex::encode(h.finalize());
            map.insert(
                rel.clone(),
                FileEntry {
                    path: rel,
                    size: bytes.len() as u64,
                    sha256,
                },
            );
        }
    }
    Ok(())
}

/// 前后快照比对 → 差异列表。
pub fn diff_workspace(
    before: &BTreeMap<String, FileEntry>,
    after: &BTreeMap<String, FileEntry>,
) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for (path, after_entry) in after {
        match before.get(path) {
            Some(before_entry) if before_entry != after_entry => {
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                    before: Some(before_entry.clone()),
                    after: Some(after_entry.clone()),
                });
            }
            None => {
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Created,
                    before: None,
                    after: Some(after_entry.clone()),
                });
            }
            _ => {}
        }
    }
    for (path, before_entry) in before {
        if !after.contains_key(path) {
            changes.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Deleted,
                before: Some(before_entry.clone()),
                after: None,
            });
        }
    }
    changes
}

/// 组装 Artifact。
pub fn collect_artifact(
    task_id: &str,
    workspace_dir: &Path,
    before: &BTreeMap<String, FileEntry>,
) -> Result<Artifact> {
    let after = snapshot_workspace(workspace_dir)?;
    let changes = diff_workspace(before, &after);
    Ok(Artifact {
        task_id: task_id.to_string(),
        changes,
        files: after.into_values().collect(),
    })
}
