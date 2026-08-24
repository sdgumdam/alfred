//! run 目录持久化三件套的唯一真源：state.json / verdicts.jsonl / audit.jsonl。
//! CLI 无状态——每条命令从这里恢复 Orchestrator，处理事件后经 apply_event 写回。
//! state.json 的 schema 由 alfred-core 的 Orchestrator serde 定义（deny_unknown_fields），
//! 本模块只负责读写，不另造状态表示。

use alfred_core::{now_millis, Event, Orchestrator, RouteAction, State, TransitionOutcome};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

pub const STATE_FILE: &str = "state.json";
pub const VERDICTS_FILE: &str = "verdicts.jsonl";
pub const AUDIT_FILE: &str = "audit.jsonl";

/// 从 run 目录恢复 Orchestrator。state.json 缺失或损坏都是 usage 级错误。
pub fn load_orchestrator(run_dir: &Path) -> Result<Orchestrator, String> {
    let path = run_dir.join(STATE_FILE);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("{} is not a valid orchestrator state: {err}", path.display()))
}

/// 把 Orchestrator 当前状态写回 state.json（原子写：先写 .tmp 再 rename）。
/// 同目录 tmp 文件保证同文件系统 rename 原子性——崩溃时不会留下半截 state。
pub fn save_orchestrator(run_dir: &Path, orch: &Orchestrator) -> Result<(), String> {
    let path = run_dir.join(STATE_FILE);
    let tmp = run_dir.join(format!("{STATE_FILE}.tmp"));
    let json = serde_json::to_string_pretty(orch)
        .map_err(|err| format!("cannot serialize orchestrator state: {err}"))?;
    fs::write(&tmp, format!("{json}\n"))
        .map_err(|err| format!("cannot write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .map_err(|err| format!("cannot rename {} to {}: {err}", tmp.display(), path.display()))
}

/// 处理一个事件并落盘全套痕迹：audit.jsonl 追加转移记录，state.json 写回新状态。
pub fn apply_event(
    orch: &mut Orchestrator,
    run_dir: &Path,
    event: Event,
) -> Result<TransitionOutcome, String> {
    let from_state = orch.state();
    let event_kind = event_kind(&event);
    let outcome = orch.transition(event).map_err(|err| err.to_string())?;
    append_audit(run_dir, &event_kind, from_state, outcome.new_state, outcome.action)?;
    save_orchestrator(run_dir, orch)?;
    Ok(outcome)
}

/// 审查结论追加写：一行一个 JSON，{type, verdict, timestamp}。
pub fn append_verdict(
    run_dir: &Path,
    verdict_type: &str,
    verdict: &impl Serialize,
) -> Result<(), String> {
    append_jsonl(
        &run_dir.join(VERDICTS_FILE),
        &serde_json::json!({
            "type": verdict_type,
            "verdict": verdict,
            "timestamp": now_millis(),
        }),
    )
}

/// 审计日志追加写：一行一个 JSON，{event, from_state, to_state, action, timestamp}。
pub fn append_audit(
    run_dir: &Path,
    event_kind: &str,
    from_state: State,
    to_state: State,
    action: Option<RouteAction>,
) -> Result<(), String> {
    append_jsonl(
        &run_dir.join(AUDIT_FILE),
        &serde_json::json!({
            "event": event_kind,
            "from_state": from_state,
            "to_state": to_state,
            "action": action_name(action),
            "timestamp": now_millis(),
        }),
    )
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let line = serde_json::to_string(value)
        .map_err(|err| format!("cannot serialize {} record: {err}", path.display()))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
    writeln!(file, "{line}").map_err(|err| format!("cannot append {}: {err}", path.display()))
}

/// Event 的 serde tag 是事件名的唯一真源，audit 复用它而不是另写字面量。
fn event_kind(event: &Event) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| value.get("kind")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// State 的 serde 字面量是状态名的唯一真源，纯文本输出复用它。
pub fn state_name(state: State) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{state:?}"))
}

fn action_name(action: Option<RouteAction>) -> Option<&'static str> {
    match action {
        None => None,
        Some(RouteAction::Advance) => Some("advance"),
        Some(RouteAction::Retry { .. }) => Some("retry"),
        Some(RouteAction::Escalate { .. }) => Some("escalate"),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::{Contract, DagSpec, Event, NodeSpec, NodeType};
    use serde_json::{Map, Value};
    use std::collections::BTreeMap;

    fn dag_spec() -> DagSpec {
        DagSpec {
            name: "store-test".into(),
            version: 1,
            entrypoint: "n1".into(),
            nodes: vec![NodeSpec {
                node_id: "n1".into(),
                node_type: NodeType::Step,
                contract: Contract {
                    prompt: "do it".into(),
                    acceptance_criteria: "done".into(),
                    reviewer_models: vec![],
                },
                params: Map::<String, Value>::new(),
                input_schema: BTreeMap::new(),
                routes: None,
            }],
            edges: vec![],
        }
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn save_then_load_roundtrips_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let orch = Orchestrator::new(2);
        save_orchestrator(dir.path(), &orch).unwrap();
        let loaded = load_orchestrator(dir.path()).unwrap();
        assert_eq!(loaded, orch);
    }

    #[test]
    fn load_rejects_missing_and_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_orchestrator(dir.path()).is_err());
        // 未知字段被 deny_unknown_fields 硬拒。
        fs::write(
            dir.path().join(STATE_FILE),
            r#"{"state":"planning","attempts":0,"retry_budget":2,"dag_spec":null,"plan_verdict":null,"exec_verdict":null,"artifact":null,"surprise":1}"#,
        )
        .unwrap();
        assert!(load_orchestrator(dir.path()).is_err());
    }

    #[test]
    fn apply_event_appends_audit_and_saves_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut orch = Orchestrator::new(2);
        let outcome = apply_event(&mut orch, dir.path(), Event::PlanSubmitted(dag_spec())).unwrap();
        assert_eq!(outcome.new_state, State::PlanReviewing);

        let audit = read_jsonl(&dir.path().join(AUDIT_FILE));
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0]["event"], "plan_submitted");
        assert_eq!(audit[0]["from_state"], "planning");
        assert_eq!(audit[0]["to_state"], "plan_reviewing");
        assert_eq!(audit[0]["action"], Value::Null);
        assert!(audit[0]["timestamp"].is_u64());

        let persisted = load_orchestrator(dir.path()).unwrap();
        assert_eq!(persisted.state(), State::PlanReviewing);
    }

    #[test]
    fn append_verdict_writes_one_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        append_verdict(dir.path(), "plan", &serde_json::json!({"pass": true})).unwrap();
        append_verdict(dir.path(), "exec", &serde_json::json!({"value": "C"})).unwrap();
        let verdicts = read_jsonl(&dir.path().join(VERDICTS_FILE));
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[0]["type"], "plan");
        assert_eq!(verdicts[0]["verdict"]["pass"], true);
        assert_eq!(verdicts[1]["type"], "exec");
        assert!(verdicts[0]["timestamp"].is_u64());
    }

}
