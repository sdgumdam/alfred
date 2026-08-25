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

/// 生成短 id：`<prefix>-<13 位纳秒戳的 hex>`。
pub fn short_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{nanos:013x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_is_well_formed() {
        let s = now_rfc3339();
        assert!(s.contains('T') && s.ends_with('Z'), "got {s}");
    }

    #[test]
    fn short_id_is_unique_prefixed() {
        let a = short_id("run");
        let b = short_id("run");
        assert_ne!(a, b);
        assert!(a.starts_with("run-"), "got {a}");
    }
}
