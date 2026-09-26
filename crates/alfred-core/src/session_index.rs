//! 原生 session 身份台账（宿主 pi 调用的可定位身份记录）。
//!
//! planner（converse/maintain）与 reviewer（plan-review/exec-review）都是宿主
//! `pi -p` 单次短会话：原生 session 落盘在各自角色工作目录的 `sessions/`
//! 子目录，文件名形如 `<timestamp>_<sessionId>.jsonl`（pi SessionManager 命名，
//! 时间戳由 pi 分配，调用方不可预知）。本模块提供两条公共能力：
//!
//! 1. **身份预指派**：调用方在 spawn 前用 `--session-id <唯一 id>` 显式指派
//!    会话身份（pi 无此 id 的本地会话时创建之）——身份由调用方持有，不依赖
//!    进程输出或事后猜测。
//! 2. **台账落盘**：pi 进程结束后（成功/超时/非零退出都记录），把
//!    `{kind, session_id, file, file_exists, pi_exit, recorded_at}` 追加到
//!    `<work>/sessions/session-index.jsonl`。`file` 按 id 精确后缀
//!    `_<session_id>.jsonl` 定位（唯一 id → 恰好 0/1 个匹配；不按时间猜
//!    "最新"，不扫描目录外路径）。
//!
//! 身份与落盘分开（W08 核收语义）：pi 启动即分配身份，但 `_persist()` 需
//! 首条 assistant 消息才写盘——`file_exists=false` 是"身份已分配、文件未
//! 落盘"的如实记录，不是失败。executor 路径的身份由 driver 从 pi RPC
//! `get_state` 实测取回（driver.done.json `session` 字段），不经本模块。

use std::io::Write;
use std::path::{Path, PathBuf};

use std::io::Result;

/// 台账文件名（`<work>/sessions/session-index.jsonl`，追加式 JSONL）。
pub const SESSION_INDEX_FILE: &str = "session-index.jsonl";

/// 按 id 精确定位 pi 原生 session 文件。
///
/// pi 的文件名约定：`<fileTimestamp>_<sessionId>.jsonl`（同一 `--session-dir`
/// 之下）。id 由调用方唯一指派，所以精确后缀匹配 0/1 个文件；多匹配（id
/// 被复用的缺陷形态）不猜——返回 `None`（台账如实落 `file=null`，消费方
/// 见到的是"身份在、文件不可定位"，不是被静默挑中的一个）。
pub fn find_session_file(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let suffix = format!("_{session_id}.jsonl");
    let mut hits: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(sessions_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(&suffix) {
            hits.push(entry.path());
        }
    }
    match hits.len() {
        0 | 1 => hits.pop(),
        _ => None,
    }
}

/// 追加一条原生 session 身份记录到 `<work>/sessions/session-index.jsonl`。
///
/// `pi_exit` 如实描述本次 pi 进程结局（"success" / "exit:<code>" / "signal" /
/// "timeout"…）；台账失败显式报错（证据链丢失可见，不静默吞）。
pub fn record_session_identity(
    work: &Path,
    invocation: &str,
    session_id: &str,
    pi_exit: &str,
) -> Result<()> {
    let sessions_dir = work.join("sessions");
    let file = find_session_file(&sessions_dir, session_id);
    let file_exists = file.as_ref().map(|p| p.is_file()).unwrap_or(false);
    let entry = serde_json::json!({
        "kind": invocation,
        "session_id": session_id,
        "file": file.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "file_exists": file_exists,
        "pi_exit": pi_exit,
        "recorded_at": crate::util::now_rfc3339(),
    });
    let ledger = sessions_dir.join(SESSION_INDEX_FILE);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ledger)?;
    writeln!(f, "{entry}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_file_by_exact_id_suffix_and_rejects_ambiguity() {
        let dir = std::env::temp_dir().join(format!("sessidx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("20260908T091810.000000Z_my-id.jsonl"), b"{}").unwrap();
        std::fs::write(dir.join("other.jsonl"), b"{}").unwrap();

        let hit = find_session_file(&dir, "my-id").unwrap();
        assert_eq!(
            hit.file_name().unwrap().to_str().unwrap(),
            "20260908T091810.000000Z_my-id.jsonl"
        );
        assert_eq!(hit.parent().unwrap(), &dir);
        // 未落盘：身份在、文件不在。
        assert_eq!(find_session_file(&dir, "my-other-id"), None);

        // 同 id 两份文件 = 不可定位（不猜）。
        std::fs::write(dir.join("20260908T100000.000000Z_my-id.jsonl"), b"{}").unwrap();
        assert_eq!(find_session_file(&dir, "my-id"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ledger_records_identity_and_file_state() {
        let work = std::env::temp_dir().join(format!("sessledger-{}", std::process::id()));
        let sessions = work.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        // 未落盘会话：file=null、file_exists=false。
        record_session_identity(&work, "converse", "id-a", "success").unwrap();
        // 已落盘会话。
        std::fs::write(sessions.join("t_id-b.jsonl"), b"{}\n").unwrap();
        record_session_identity(&work, "plan-review", "id-b", "exit:1").unwrap();

        let text = std::fs::read_to_string(sessions.join(SESSION_INDEX_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"], "converse");
        assert_eq!(first["session_id"], "id-a");
        assert_eq!(first["file"], serde_json::Value::Null);
        assert_eq!(first["file_exists"], false);
        assert_eq!(first["pi_exit"], "success");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["kind"], "plan-review");
        assert_eq!(
            second["file"].as_str().unwrap(),
            sessions.join("t_id-b.jsonl").to_str().unwrap()
        );
        assert_eq!(second["file_exists"], true);
        assert_eq!(second["pi_exit"], "exit:1");
        std::fs::remove_dir_all(&work).unwrap();
    }
}
