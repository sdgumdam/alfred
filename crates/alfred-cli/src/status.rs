//! status 命令：读 run 目录的 state.json，打印当前状态、attempts、最近 verdict、
//! artifact 摘要。纯文本输出，只读不写。

use crate::store;
use crate::{EXIT_OK, EXIT_USAGE};
use alfred_core::Orchestrator;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

pub fn run(run_dir: &Path) -> ExitCode {
    ExitCode::from(status_code(run_dir))
}

fn status_code(run_dir: &Path) -> u8 {
    match render(run_dir) {
        Ok(text) => {
            print!("{text}");
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{err}");
            EXIT_USAGE
        }
    }
}

fn render(run_dir: &Path) -> Result<String, String> {
    let path = run_dir.join(store::STATE_FILE);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|err| format!("{} is not valid JSON: {err}", path.display()))?;
    // 先按 Orchestrator schema 硬校验，损坏的 state 不允许打印出误导性摘要；
    // 摘要字段从 Value 读（Orchestrator 字段私有，schema 由 serde 定义）。
    let orch: Orchestrator = serde_json::from_value(value.clone())
        .map_err(|err| format!("{} is not a valid orchestrator state: {err}", path.display()))?;

    let mut out = String::new();
    out.push_str(&format!("run_dir: {}\n", run_dir.display()));
    out.push_str(&format!("state: {}\n", store::state_name(orch.state())));
    out.push_str(&format!("attempts: {}/{}\n", orch.attempts(), orch.retry_budget()));
    // 最近 verdict：exec 优先于 plan（治理环里 exec 审查更晚发生）。
    if let Some(line) = exec_verdict_line(&value) {
        out.push_str(&format!("verdict: exec {line}\n"));
    } else if let Some(line) = plan_verdict_line(&value) {
        out.push_str(&format!("verdict: plan {line}\n"));
    } else {
        out.push_str("verdict: none\n");
    }
    if let Some(line) = artifact_line(&value) {
        out.push_str(&format!("artifact: {line}\n"));
    } else {
        out.push_str("artifact: none\n");
    }
    if let Some(line) = dag_line(&value) {
        out.push_str(&format!("dag_spec: {line}\n"));
    }
    Ok(out)
}

fn plan_verdict_line(state: &Value) -> Option<String> {
    let verdict = state.get("plan_verdict")?;
    if verdict.is_null() {
        return None;
    }
    Some(format!(
        "pass={} reason={}",
        verdict.get("pass")?,
        verdict.get("reason")?.as_str()?
    ))
}

fn exec_verdict_line(state: &Value) -> Option<String> {
    let verdict = state.get("exec_verdict")?;
    if verdict.is_null() {
        return None;
    }
    let failure_class = match verdict.get("failure_class") {
        Some(Value::String(class)) => format!(" failure_class={class}"),
        _ => String::new(),
    };
    Some(format!(
        "value={}{} confidence={} explanation={}",
        verdict.get("value")?.as_str()?,
        failure_class,
        verdict.get("confidence")?.as_str()?,
        verdict.get("explanation")?.as_str()?
    ))
}

fn artifact_line(state: &Value) -> Option<String> {
    let artifact = state.get("artifact")?;
    if artifact.is_null() {
        return None;
    }
    let diff = artifact.get("workspace_diff")?.as_str()?;
    Some(format!(
        "node={} produced_at={} diff_lines={}",
        artifact.get("node_id")?.as_str()?,
        artifact.get("produced_at")?.as_str()?,
        diff.lines().count()
    ))
}

fn dag_line(state: &Value) -> Option<String> {
    let dag = state.get("dag_spec")?;
    if dag.is_null() {
        return None;
    }
    Some(format!(
        "name={} version={} nodes={}",
        dag.get("name")?.as_str()?,
        dag.get("version")?,
        dag.get("nodes")?.as_array()?.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::{
        Artifact, Confidence, Contract, DagSpec, Event, ExecVerdict, FailureClass, NodeSpec,
        NodeType, PlanVerdict, VerdictValue,
    };
    use serde_json::Map;
    use std::collections::BTreeMap;

    fn dag_spec() -> DagSpec {
        DagSpec {
            name: "status-test".into(),
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

    fn escalated_orchestrator() -> Orchestrator {
        let mut orch = Orchestrator::new(2);
        orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
        orch.transition(Event::PlanReviewed(PlanVerdict {
            pass: true,
            reason: "plan ok".into(),
        }))
        .unwrap();
        orch.transition(Event::ExecutionDone(Artifact {
            node_id: "n1".into(),
            workspace_diff: "line-a\nline-b\n".into(),
            produced_at: "2026-08-24T00:00:00Z".into(),
        }))
        .unwrap();
        orch.transition(Event::ExecReviewed(ExecVerdict::failed(
            VerdictValue::I,
            FailureClass::ContractFault,
            Confidence::High,
            vec![],
            "contract broken".into(),
        )))
        .unwrap();
        orch
    }

    #[test]
    fn render_reports_state_attempts_latest_verdict_and_artifact() {
        let dir = tempfile::tempdir().unwrap();
        store::save_orchestrator(dir.path(), &escalated_orchestrator()).unwrap();
        let text = render(dir.path()).unwrap();
        assert!(text.contains("state: escalated"), "{text}");
        assert!(text.contains("attempts: 0/2"), "{text}");
        // 最近 verdict 是 exec（不是 plan）。
        assert!(text.contains("verdict: exec value=I failure_class=contract_fault"), "{text}");
        assert!(text.contains("explanation=contract broken"), "{text}");
        assert!(text.contains("artifact: node=n1 produced_at=2026-08-24T00:00:00Z diff_lines=2"), "{text}");
        assert!(text.contains("dag_spec: name=status-test version=1 nodes=1"), "{text}");
    }

    #[test]
    fn render_fresh_run_shows_no_verdict() {
        let dir = tempfile::tempdir().unwrap();
        store::save_orchestrator(dir.path(), &Orchestrator::new(2)).unwrap();
        let text = render(dir.path()).unwrap();
        assert!(text.contains("state: planning"), "{text}");
        assert!(text.contains("verdict: none"), "{text}");
        assert!(text.contains("artifact: none"), "{text}");
    }

    #[test]
    fn status_missing_state_file_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(status_code(dir.path()), EXIT_USAGE);
    }

    #[test]
    fn status_corrupt_state_file_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(store::STATE_FILE), "{\"state\":\"bogus\"}").unwrap();
        assert_eq!(status_code(dir.path()), EXIT_USAGE);
    }
}
