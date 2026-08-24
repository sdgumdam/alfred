pub mod builder;
pub mod config;
pub mod entities;
pub mod llm;
pub mod orchestrator;
pub mod routing;
pub mod session_doc;
pub mod validate;
pub mod verdict;

pub use builder::{BuilderError, CommitError, Draft, DraftStatus, GraphBuilder};
pub use entities::*;
pub use llm::{chat_completion, LlmConfig, LlmError, LlmRole, Message};
pub use orchestrator::{Event, OrchError, Orchestrator, OwnerDecision, State, TransitionOutcome};
pub use routing::{route, RouteAction};
pub use session_doc::SessionDoc;
pub use validate::{validate_dagspec, ValidationError, ValidationIssue};
pub use verdict::{ExecVerdict, PlanVerdict};
/// 模型常把 JSON 裹进 ```json 围栏；剥掉再解析。
pub fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let without_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    without_open.strip_suffix("```").unwrap_or(without_open).trim()
}

// ── 单一真源常量：禁止在 planner/reviewer/executor/cli 各自重定义 ──

/// 离线模式环境变量名：置 `ALFRED_OFFLINE=1` 跳过 LLM/Docker，返回确定性 stub。
pub const OFFLINE_ENV: &str = "ALFRED_OFFLINE";

/// handler 魔法串的唯一真源：TaskAssignment.handler 取此值标识执行-审查-裁决三段。
/// S1 阶段 executor 消费 contract.prompt，handler 字段保留给后续 Inspect AI 集成。
pub const HANDLER_RUN_INSPECT_EVAL: &str = "run_inspect_eval";

/// 读 ALFRED_OFFLINE 开关：值为 "1" 即离线。每次实时读 env（不缓存），
/// 与 planner/reviewer/run 三处的逐次读取语义一致。
pub fn offline_mode() -> bool {
    std::env::var(OFFLINE_ENV)
        .ok()
        .filter(|v| v == "1")
        .is_some()
}

// ── 时间格式化单一真源（Howard Hinnant civil-from-days，无外部依赖）──

/// 当前 epoch 毫秒。约定与 llm.rs 一致。
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// 当前时间 ISO-8601 UTC 秒级（如 `2026-08-24T12:34:56Z`）。
pub fn now_iso8601_utc() -> String {
    iso8601_utc(now_millis())
}

/// epoch 毫秒 → ISO-8601 UTC 秒级（Howard Hinnant civil-from-days，无外部依赖）。
/// executor 与 cli/store 共用此唯一实现，禁止各写一份。
pub fn iso8601_utc(millis: u64) -> String {
    let secs = millis / 1000;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_converts_known_epochs() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(1_767_225_600_000), "2026-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400_000), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn now_iso8601_utc_has_canonical_shape() {
        let ts = now_iso8601_utc();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn now_millis_is_non_decreasing() {
        let a = now_millis();
        let b = now_millis();
        assert!(b >= a);
    }

    #[test]
    fn offline_mode_reflects_env_var() {
        // 清理可能残留的开关值，避免受其他测试影响。
        std::env::remove_var(OFFLINE_ENV);
        assert!(!offline_mode());
        std::env::set_var(OFFLINE_ENV, "1");
        assert!(offline_mode());
        std::env::set_var(OFFLINE_ENV, "0");
        assert!(!offline_mode());
        std::env::set_var(OFFLINE_ENV, "yes");
        assert!(!offline_mode());
        std::env::remove_var(OFFLINE_ENV);
    }

    #[test]
    fn handler_run_inspect_eval_is_stable_literal() {
        assert_eq!(HANDLER_RUN_INSPECT_EVAL, "run_inspect_eval");
    }
}
