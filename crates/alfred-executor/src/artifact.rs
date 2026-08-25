//! 产物采集：容器 workspace 卷（挂 run_dir/workspace，绝对路径）的文件比对。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn diff_detects_create_modify_delete() {
        let dir = std::env::temp_dir().join(format!("alfred-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_file(&dir, "keep.txt", "same");
        write_file(&dir, "del.txt", "bye");
        write_file(&dir, "mod.txt", "v1");
        let before = snapshot_workspace(&dir).unwrap();

        write_file(&dir, "keep.txt", "same");
        write_file(&dir, "new.txt", "hello");
        std::fs::remove_file(dir.join("del.txt")).unwrap();
        write_file(&dir, "mod.txt", "v2");
        let after = snapshot_workspace(&dir).unwrap();

        let changes = diff_workspace(&before, &after);
        let kinds: Vec<(String, ChangeKind)> = changes
            .iter()
            .map(|c| (c.path.clone(), c.kind))
            .collect();
        assert!(kinds.contains(&("new.txt".into(), ChangeKind::Created)));
        assert!(kinds.contains(&("del.txt".into(), ChangeKind::Deleted)));
        assert!(kinds.contains(&("mod.txt".into(), ChangeKind::Modified)));
        assert!(!kinds.contains(&("keep.txt".into(), ChangeKind::Modified)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_skips_symlinks() {
        let dir = std::env::temp_dir().join(format!("alfred-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_file(&dir, "real.txt", "x");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.join("evil")).unwrap();
        let snap = snapshot_workspace(&dir).unwrap();
        assert!(snap.contains_key("real.txt"));
        assert!(!snap.contains_key("evil"), "symlink must be skipped");
        std::fs::remove_dir_all(&dir).ok();
    }
}
