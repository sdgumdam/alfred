//! 共享小工具（id / 时间戳）。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 UTC 时间，RFC3339 格式。
pub fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    match now.format(&time::format_description::well_known::Rfc3339) {
        Ok(s) => s,
        Err(_) => format!("{now:?}"),
    }
}

/// 生成短 id：`<prefix>-<13 位纳秒戳的 hex><进程内序号>`。
pub fn short_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 进程内原子序号：同一纳秒内两次调用也保证不同（R3 修复 e2e 偶发碰撞）。
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:013x}{seq:02x}")
}
