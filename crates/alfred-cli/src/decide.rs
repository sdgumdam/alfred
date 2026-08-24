//! decide 命令：属主对挂起态（plan_rejected / escalated）的裁决入口。
//! 从 state.json 恢复 Orchestrator，校验挂起态，处理 OwnerDecided，写回。

use crate::store;
use crate::{EXIT_OK, EXIT_USAGE};
use alfred_core::{Event, OwnerDecision, State};
use std::path::Path;
use std::process::ExitCode;

pub fn run(run_dir: &Path, decision: &str) -> ExitCode {
    ExitCode::from(decide_code(run_dir, decision))
}

fn decide_code(run_dir: &Path, decision: &str) -> u8 {
    match execute(run_dir, decision) {
        Ok(new_state) => {
            println!("state: {}", store::state_name(new_state));
            EXIT_OK
        }
        Err(err) => {
            eprintln!("{err}");
            EXIT_USAGE
        }
    }
}

fn execute(run_dir: &Path, decision: &str) -> Result<State, String> {
    let decision = parse_decision(decision)?;
    let mut orch = store::load_orchestrator(run_dir)?;
    match orch.state() {
        State::PlanRejected | State::Escalated => {}
        state => {
            return Err(format!(
                "run is in state `{}`; owner decisions are only accepted in `plan_rejected` or `escalated`",
                store::state_name(state),
            ));
        }
    }
    let outcome = store::apply_event(&mut orch, run_dir, Event::OwnerDecided(decision))?;
    Ok(outcome.new_state)
}

/// CLI 用 kebab-case（revise-contract），同时兼容 serde 的 snake_case。
fn parse_decision(raw: &str) -> Result<OwnerDecision, String> {
    match raw {
        "retry" => Ok(OwnerDecision::Retry),
        "revise-contract" | "revise_contract" => Ok(OwnerDecision::ReviseContract),
        "abandon" => Ok(OwnerDecision::Abandon),
        other => Err(format!(
            "unknown decision `{other}`; expected one of: retry, revise-contract, abandon"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AUDIT_FILE;
    use alfred_core::{
        Artifact, Confidence, Contract, DagSpec, ExecVerdict, FailureClass, NodeSpec, NodeType,
        Orchestrator, PlanVerdict, VerdictValue,
    };
    use serde_json::{Map, Value};
    use std::collections::BTreeMap;
    use std::fs;

    fn dag_spec() -> DagSpec {
        DagSpec {
            name: "decide-test".into(),
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

    fn artifact() -> Artifact {
        Artifact {
            node_id: "n1".into(),
            workspace_diff: "diff".into(),
            produced_at: "2026-08-24T00:00:00Z".into(),
        }
    }

    fn plan_rejected_orchestrator() -> Orchestrator {
        let mut orch = Orchestrator::new(2);
        orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
        orch.transition(Event::PlanReviewed(PlanVerdict {
            pass: false,
            reason: "off topic".into(),
        }))
        .unwrap();
        orch
    }

    fn escalated_orchestrator() -> Orchestrator {
        let mut orch = Orchestrator::new(2);
        orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
        orch.transition(Event::PlanReviewed(PlanVerdict { pass: true, reason: "ok".into() }))
            .unwrap();
        orch.transition(Event::ExecutionDone(artifact())).unwrap();
        orch.transition(Event::ExecReviewed(ExecVerdict::failed(
            VerdictValue::P,
            FailureClass::ContractAmbiguity,
            Confidence::Medium,
            vec![],
            "ambiguous".into(),
        )))
        .unwrap();
        orch
    }

    fn completed_orchestrator() -> Orchestrator {
        let mut orch = Orchestrator::new(2);
        orch.transition(Event::PlanSubmitted(dag_spec())).unwrap();
        orch.transition(Event::PlanReviewed(PlanVerdict { pass: true, reason: "ok".into() }))
            .unwrap();
        orch.transition(Event::ExecutionDone(artifact())).unwrap();
        orch.transition(Event::ExecReviewed(ExecVerdict::correct(
            Confidence::High,
            vec![],
            "correct".into(),
        )))
        .unwrap();
        orch
    }

    fn save(dir: &Path, orch: &Orchestrator) {
        store::save_orchestrator(dir, orch).unwrap();
    }

    fn audit_events(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join(AUDIT_FILE))
            .unwrap()
            .lines()
            .map(|line| {
                let value: Value = serde_json::from_str(line).unwrap();
                value["event"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    #[test]
    fn decide_retry_on_plan_rejected_returns_to_planning() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &plan_rejected_orchestrator());
        assert_eq!(decide_code(dir.path(), "retry"), EXIT_OK);
        let orch = store::load_orchestrator(dir.path()).unwrap();
        assert_eq!(orch.state(), State::Planning);
        assert_eq!(orch.attempts(), 0);
        assert_eq!(audit_events(dir.path()), vec!["owner_decided"]);
    }

    #[test]
    fn decide_revise_contract_kebab_accepted() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &plan_rejected_orchestrator());
        assert_eq!(decide_code(dir.path(), "revise-contract"), EXIT_OK);
        assert_eq!(store::load_orchestrator(dir.path()).unwrap().state(), State::Planning);
    }

    #[test]
    fn decide_abandon_on_escalated_completes_run() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &escalated_orchestrator());
        assert_eq!(decide_code(dir.path(), "abandon"), EXIT_OK);
        assert_eq!(store::load_orchestrator(dir.path()).unwrap().state(), State::Completed);
    }

    #[test]
    fn decide_retry_on_escalated_resumes_executing() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &escalated_orchestrator());
        assert_eq!(decide_code(dir.path(), "retry"), EXIT_OK);
        let orch = store::load_orchestrator(dir.path()).unwrap();
        assert_eq!(orch.state(), State::Executing);
        assert_eq!(orch.attempts(), 0);
    }

    #[test]
    fn decide_on_completed_run_is_usage_error_and_leaves_state_untouched() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &completed_orchestrator());
        assert_eq!(decide_code(dir.path(), "retry"), EXIT_USAGE);
        assert_eq!(store::load_orchestrator(dir.path()).unwrap().state(), State::Completed);
        assert!(!dir.path().join(AUDIT_FILE).exists());
    }

    #[test]
    fn decide_rejects_unknown_decision() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &plan_rejected_orchestrator());
        assert_eq!(decide_code(dir.path(), "do-whatever"), EXIT_USAGE);
        assert_eq!(store::load_orchestrator(dir.path()).unwrap().state(), State::PlanRejected);
    }

    #[test]
    fn decide_missing_state_file_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(decide_code(dir.path(), "retry"), EXIT_USAGE);
    }
}
