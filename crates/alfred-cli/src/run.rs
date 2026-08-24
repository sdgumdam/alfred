//! run 命令：驱动治理环走完一轮——计划 → 计划审查 → 执行（S2 真容器执行）
//! → 执行审查 → 路由（推进 / 重跑 / 升级）。全程无状态：Orchestrator 落 state.json，
//! 审查结论落 verdicts.jsonl，每次状态转移落 audit.jsonl。
//!
//! 计划入口是 plan_owner_request：真路径内部走 converse 双 agent（Reply 映射为
//! PlanError::OwnerReply，此处打印后正常退出）；ALFRED_OFFLINE=1 走结构化意图直通，
//! 与 plan 命令共用同一条单一链路。converse 本身无离线分支，不能直接调。
//!
//! 执行入口是 alfred_executor::execute_in_container：离线模式返回确定性 fixture
//! Artifact，不碰 Docker；默认走真 Docker 容器（pi 黑盒执行者只拿 contract.prompt）。
//! SandboxProfile 从 DagSpec 入口节点的 params["sandbox"] 读，没有则用默认 deny-all。

use crate::store;
use crate::ask_panel;
use crate::{EXIT_ESCALATED, EXIT_OK, EXIT_USAGE};
use alfred_core::{
    now_millis, offline_mode, Artifact, Confidence, Event, ExecVerdict, FailureClass, NodeSpec,
    Orchestrator, OwnerRequest, RouteAction, SessionDoc, State, TaskAssignment,
    VerdictValue, HANDLER_RUN_INSPECT_EVAL,
};
use alfred_executor::{execute_in_container, SandboxProfile};
use alfred_planner::{plan_owner_request, rejection_report, PlanError};
use alfred_reviewer::{review_execution, review_plan};

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// mechanical 重试预算上限；随 Orchestrator 持久化进 state.json，run 可复现。
const RETRY_BUDGET: u32 = 2;
/// 离线注入环境变量：在 ALFRED_OFFLINE=1 下强制执行审查返回指定 failure_class 的
/// 失败裁决，使负路径在无 LLM/无 Docker 环境下可证伪。值取 failure_class 的
/// snake_case 名（mechanical / contract_fault / contract_ambiguity / fidelity_dispute /
/// disagreement）；留空或不设则返回正确裁决。
const INJECT_EXEC_VERDICT_ENV: &str = "ALFRED_INJECT_EXEC_VERDICT";
/// 离线注入环境变量：在 ALFRED_OFFLINE=1 下强制计划审查返回 pass=false，使计划打回
/// 负路径在无 LLM 环境下可证伪。值设为 "reject" 则打回；其他值或不设则放行。
const INJECT_PLAN_VERDICT_ENV: &str = "ALFRED_INJECT_PLAN_VERDICT";

pub fn run(request_path: &Path, out_dir: Option<&Path>) -> ExitCode {
    ExitCode::from(run_code(request_path, out_dir))
}

fn run_code(request_path: &Path, out_dir: Option<&Path>) -> u8 {
    match execute(request_path, out_dir) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            EXIT_USAGE
        }
    }
}

fn execute(request_path: &Path, out_dir: Option<&Path>) -> Result<u8, String> {
    let request = read_owner_request(request_path)?;
    let run_dir = out_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("run-{}", run_id(&request))));
    fs::create_dir_all(&run_dir)
        .map_err(|err| format!("cannot create run directory {}: {err}", run_dir.display()))?;

    // 计划：真路径走 converse 双 agent；离线走结构化意图直通（plan_owner_request 内部判定）。
    let spec = match plan_owner_request(&request) {
        Ok(spec) => spec,
        Err(PlanError::OwnerReply(text)) => {
            // 对话 agent 选择答复而非产出计划（需求未收敛）：原样打印，正常退出。
            println!("{text}");
            return Ok(EXIT_OK);
        }
        Err(err) => {
            let report = rejection_report(request_path, &err);
            return Err(serde_json::to_string(&report)
                .expect("rejection report serialization cannot fail"));
        }
    };

    let mut orch = Orchestrator::new(RETRY_BUDGET);
    store::save_orchestrator(&run_dir, &orch)?;
    store::apply_event(&mut orch, &run_dir, Event::PlanSubmitted(spec.clone()))?;

    // 计划审查：审查者全可见；S1 单轮，会话文档为空、对话记录为空。
    // 离线注入（ALFRED_INJECT_PLAN_VERDICT=reject）使计划打回负路径可证伪。
    let session_doc = SessionDoc::default();
    let plan_verdict = review_plan_with_injection(&request, &spec, &session_doc, &run_dir)?;
    store::append_verdict(&run_dir, "plan", &plan_verdict)?;
    store::apply_event(&mut orch, &run_dir, Event::PlanReviewed(plan_verdict))?;

    if orch.state() == State::PlanRejected {
        // 计划被拒：挂起等待属主 decide，dagspec 不落地。
        // ask panel 触发时 spawn pi 交互决策；不触发（离线/CI）直接 exit 1。
        print_summary(&run_dir, &orch);
        return handle_suspension(&orch, &run_dir);
    }

    let dagspec_path = run_dir.join("dagspec.json");
    let dagspec_json = serde_json::to_string_pretty(&spec)
        .map_err(|err| format!("cannot serialize dagspec: {err}"))?;
    fs::write(&dagspec_path, format!("{dagspec_json}\n"))
        .map_err(|err| format!("cannot write {}: {err}", dagspec_path.display()))?;

    // 单节点骨架：执行对象是入口节点，验收以其契约为标准。
    let node = spec
        .nodes
        .iter()
        .find(|n| n.node_id == spec.entrypoint)
        .ok_or_else(|| format!("dagspec entrypoint `{}` matches no node", spec.entrypoint))?;
    let contract = node.contract.clone();

    // SandboxProfile 从节点 params["sandbox"] 读，没有则用默认 deny-all
    // （network=false、volumes 空）；executor 照档案拼装 docker run 参数。
    let profile = sandbox_profile(node);

    // TaskAssignment 是 executor 的唯一输入：只拿契约 prompt，不知道任务图。
    let assignment = TaskAssignment {
        task_id: spec.entrypoint.clone(),
        handler: HANDLER_RUN_INSPECT_EVAL.into(),
        contract: contract.clone(),
        params: node.params.clone(),
    };

    // 工作区：run_dir 下的 workspace 子目录，容器内 /workspace 挂载点。
    // executor 在工作区建 git 基线、采集 diff；离线模式不碰工作区。
    let workspace = run_dir.join("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|err| format!("cannot create workspace {}: {err}", workspace.display()))?;

    // 执行 + 执行审查 + 路由循环：execute_in_container 跑真容器（离线返回
    // fixture），Retry 在预算内重跑，Advance / Escalate 收束。
    loop {
        let artifact = execute_in_container(&assignment, &workspace, &profile, &run_dir)
            .map_err(|err| format!("execution failed: {err}"))?;
        store::apply_event(&mut orch, &run_dir, Event::ExecutionDone(artifact.clone()))?;
        let exec_verdict = review_execution_with_injection(&contract, &artifact, &run_dir)?;
        store::append_verdict(&run_dir, "exec", &exec_verdict)?;
        let outcome = store::apply_event(&mut orch, &run_dir, Event::ExecReviewed(exec_verdict))?;
        if !matches!(outcome.action, Some(RouteAction::Retry { .. })) {
            break;
        }
    }

    match orch.state() {
        State::Completed => Ok(EXIT_OK),
        State::Escalated => handle_suspension(&orch, &run_dir),
        state => Err(format!("run ended in unexpected state {}", store::state_name(state))),
    }
}
/// 挂起态（PlanRejected / Escalated）的收尾：触发 ask panel 时 spawn pi 交互
/// 决策并复用 decide 逻辑（apply OwnerDecided + 写回 state.json + audit.jsonl）；
/// 不触发（离线模式 / ALFRED_NO_ASK_PANEL=1）时保持现有行为直接 EXIT_ESCALATED。
/// ask panel 失败时打印错误 + 提示手动 alfred decide，exit 1。
fn handle_suspension(orch: &Orchestrator, run_dir: &Path) -> Result<u8, String> {
    if !ask_panel::should_trigger(orch.state()) {
        return Ok(EXIT_ESCALATED);
    }
    // 读 verdicts.jsonl 最后一条，作为 ask panel 的升级上下文。
    let verdict = read_last_verdict(run_dir)?;
    match ask_panel::spawn_ask_panel(orch.state(), &verdict, run_dir, orch.attempts(), orch.retry_budget()) {
        Ok(decision) => {
            let mut orch = orch.clone();
            let outcome = store::apply_event(&mut orch, run_dir, Event::OwnerDecided(decision))?;
            println!("state: {}", store::state_name(outcome.new_state));
            match outcome.new_state {
                State::Completed => Ok(EXIT_OK),
                // 重跑 / 改契约后回到执行或计划：本 run 闭环到此，后续由下一轮 run 继续。
                _ => Ok(EXIT_ESCALATED),
            }
        }
        Err(err) => {
            eprintln!("ask panel failed: {err}");
            eprintln!(
                "  手动决策请运行: alfred decide {} <retry|revise-contract|abandon>",
                run_dir.display()
            );
            Ok(EXIT_ESCALATED)
        }
    }
}

/// 读 verdicts.jsonl 最后一条记录（含 type/verdict/timestamp），用于 ask panel 上下文。
/// 文件缺失或为空时返回错误，使 ask panel 不会在无裁决时触发。
fn read_last_verdict(run_dir: &Path) -> Result<serde_json::Value, String> {
    let path = run_dir.join(store::VERDICTS_FILE);
    let raw = fs::read_to_string(&path).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.is_empty())
        .last()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .ok_or_else(|| format!("no verdicts found in {}", path.display()))
}

fn read_owner_request(request_path: &Path) -> Result<OwnerRequest, String> {
    let raw = fs::read_to_string(request_path).map_err(|err| {
        format!("cannot read request file {}: {err}", request_path.display())
    })?;
    serde_json::from_str::<OwnerRequest>(&raw).map_err(|err| {
        format!(
            "OwnerRequest schema rejected {}: {err}",
            request_path.display()
        )
    })
}

/// run 目录名：优先 request_id（净化路径分隔符等），为空则退到时间戳。
fn run_id(request: &OwnerRequest) -> String {
    let id = request.request_id.trim();
    if id.is_empty() {
        return now_millis().to_string();
    }
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// 从 NodeSpec.params["sandbox"] 反序列化 SandboxProfile；缺字段或无则默认 deny-all
/// （network=false、volumes 空）。executor 照档案拼装 docker run 参数——
/// 知识只在 executor crate，CLI 不感知容器细节。
fn sandbox_profile(node: &NodeSpec) -> SandboxProfile {
    node.params
        .get("sandbox")
        .and_then(|v| serde_json::from_value::<SandboxProfile>(v.clone()).ok())
        .unwrap_or(SandboxProfile {
            provider: String::new(),
            volumes: vec![],
            runtime: String::new(),
            packages: vec![],
            network: false,
        })
}

/// 计划审查：真路径走 review_plan（真 LLM）；离线模式走 stub 或注入打回。
/// ALFRED_OFFLINE=1 + ALFRED_INJECT_PLAN_VERDICT=reject → pass=false，计划打回挂起。
fn review_plan_with_injection(
    request: &OwnerRequest,
    spec: &alfred_core::DagSpec,
    session_doc: &SessionDoc,
    run_dir: &Path,
) -> Result<alfred_core::PlanVerdict, String> {
    if offline_mode() && inject_plan_reject() {
        return Ok(alfred_core::PlanVerdict {
            pass: false,
            reason: "offline injection: plan rejected".into(),
        });
    }
    review_plan(request, spec, session_doc, "", run_dir).map_err(|err| err.to_string())
}

/// 执行审查：真路径走 review_execution（真 LLM）；离线模式走 stub 或注入失败。
/// ALFRED_OFFLINE=1 + ALFRED_INJECT_EXEC_VERDICT=<class> → 指定 failure_class 的失败裁决。
fn review_execution_with_injection(
    contract: &alfred_core::Contract,
    artifact: &Artifact,
    run_dir: &Path,
) -> Result<ExecVerdict, String> {
    if offline_mode() {
        if let Some(class) = inject_exec_failure_class() {
            return Ok(ExecVerdict::failed(
                VerdictValue::I,
                class,
                Confidence::Medium,
                vec![],
                "offline injection: exec failure".into(),
            ));
        }
    }
    review_execution(contract, artifact, run_dir).map_err(|err| err.to_string())
}

fn inject_plan_reject() -> bool {
    std::env::var(INJECT_PLAN_VERDICT_ENV)
        .map(|v| v == "reject")
        .unwrap_or(false)
}

fn inject_exec_failure_class() -> Option<FailureClass> {
    let raw = std::env::var(INJECT_EXEC_VERDICT_ENV).ok()?;
    match raw.as_str() {
        "mechanical" => Some(FailureClass::Mechanical),
        "contract_fault" => Some(FailureClass::ContractFault),
        "contract_ambiguity" => Some(FailureClass::ContractAmbiguity),
        "fidelity_dispute" => Some(FailureClass::FidelityDispute),
        "disagreement" => Some(FailureClass::Disagreement),
        _ => None,
    }
}

fn print_summary(run_dir: &Path, orch: &Orchestrator) {
    println!("state: {}", store::state_name(orch.state()));
    println!("attempts: {}/{}", orch.attempts(), orch.retry_budget());
    println!("run_dir: {}", run_dir.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AUDIT_FILE, STATE_FILE, VERDICTS_FILE};
    use serde_json::Value;

    fn request_json(with_intent: bool) -> String {
        let intent = if with_intent {
            r#",
  "dag_spec": {
    "name": "greeting",
    "version": 1,
    "entrypoint": "write-greeting",
    "nodes": [
      {
        "node_id": "write-greeting",
        "node_type": "step",
        "contract": {
          "prompt": "Write Hello Alfred into hello.txt",
          "acceptance_criteria": "hello.txt exists with content Hello Alfred",
          "reviewer_models": ["judge-a"]
        }
      }
    ],
    "edges": []
  }"#
        } else {
            ""
        };
        format!(
            r#"{{
  "request_id": "req-cli-1",
  "requirement": "Create hello.txt containing Hello Alfred",
  "acceptance_criteria": "hello.txt exists with content Hello Alfred"{intent}
}}"#
        )
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn offline_run_completes_and_persists_three_files() {
        std::env::set_var("ALFRED_OFFLINE", "1");
        let tmp = tempfile::tempdir().unwrap();
        let request_path = tmp.path().join("request.json");
        fs::write(&request_path, request_json(true)).unwrap();
        let run_dir = tmp.path().join("run-req-cli-1");

        assert_eq!(run_code(&request_path, Some(&run_dir)), EXIT_OK);

        // state.json：Completed，retry_budget 持久化。
        let orch = store::load_orchestrator(&run_dir).unwrap();
        assert_eq!(orch.state(), State::Completed);
        assert_eq!(orch.retry_budget(), RETRY_BUDGET);
        assert!(run_dir.join(STATE_FILE).is_file());
        assert!(run_dir.join("dagspec.json").is_file());

        // verdicts.jsonl：先 plan 后 exec，均离线 stub 结论。
        let verdicts = read_jsonl(&run_dir.join(VERDICTS_FILE));
        assert_eq!(verdicts.len(), 2);
        assert_eq!(verdicts[0]["type"], "plan");
        assert_eq!(verdicts[0]["verdict"]["pass"], true);
        assert_eq!(verdicts[1]["type"], "exec");
        assert_eq!(verdicts[1]["verdict"]["value"], "C");

        // audit.jsonl：四次转移，事件序与状态序锁死。
        let audit = read_jsonl(&run_dir.join(AUDIT_FILE));
        let events: Vec<&str> =
            audit.iter().map(|line| line["event"].as_str().unwrap()).collect();
        assert_eq!(
            events,
            vec!["plan_submitted", "plan_reviewed", "execution_done", "exec_reviewed"]
        );
        assert_eq!(audit[0]["from_state"], "planning");
        assert_eq!(audit[0]["to_state"], "plan_reviewing");
        assert_eq!(audit[3]["to_state"], "completed");
        assert_eq!(audit[3]["action"], "advance");
    }

    #[test]
    fn offline_run_without_structured_intent_is_usage_error() {
        std::env::set_var("ALFRED_OFFLINE", "1");
        let tmp = tempfile::tempdir().unwrap();
        let request_path = tmp.path().join("request.json");
        fs::write(&request_path, request_json(false)).unwrap();
        let run_dir = tmp.path().join("run-no-intent");
        assert_eq!(run_code(&request_path, Some(&run_dir)), EXIT_USAGE);
        assert!(!run_dir.join(STATE_FILE).exists());
    }

    #[test]
    fn run_rejects_malformed_request_file() {
        std::env::set_var("ALFRED_OFFLINE", "1");
        let tmp = tempfile::tempdir().unwrap();
        let request_path = tmp.path().join("bad.json");
        fs::write(&request_path, "{ not json").unwrap();
        assert_eq!(
            run_code(&request_path, Some(&tmp.path().join("run-bad"))),
            EXIT_USAGE
        );
    }
    fn dag_spec_for_test() -> alfred_core::DagSpec {
        use alfred_core::{Contract, NodeType};
        use serde_json::{Map, Value};
        use std::collections::BTreeMap;
        alfred_core::DagSpec {
            name: "test".into(),
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
        let mut orch = Orchestrator::new(RETRY_BUDGET);
        orch.transition(Event::PlanSubmitted(dag_spec_for_test())).unwrap();
        orch.transition(Event::PlanReviewed(alfred_core::PlanVerdict {
            pass: true,
            reason: "ok".into(),
        }))
        .unwrap();
        orch.transition(Event::ExecutionDone(Artifact {
            node_id: "n1".into(),
            workspace_diff: "diff".into(),
            produced_at: "2026-08-24T00:00:00Z".into(),
        }))
        .unwrap();
        orch.transition(Event::ExecReviewed(ExecVerdict::failed(
            VerdictValue::P,
            FailureClass::ContractFault,
            Confidence::Medium,
            vec![],
            "contract fault".into(),
        )))
        .unwrap();
        orch
    }

    #[test]
    fn handle_suspension_offline_skips_pi_and_returns_escalated() {
        // 挂起态在离线模式（或显式 ALFRED_NO_ASK_PANEL=1）下不触发 ask panel：
        // handle_suspension 直接返回 EXIT_ESCALATED，不 spawn pi，不修改 state.json。
        // 同时设两个逃生舱，使本测试对 ALFRED_OFFLINE 的 env 竞态免疫（即使
        // 并行测试临时 unset ALFRED_OFFLINE，no-ask-panel 仍保证 should_trigger=false）。
        std::env::set_var("ALFRED_OFFLINE", "1");
        std::env::set_var("ALFRED_NO_ASK_PANEL", "1");
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let orch = escalated_orchestrator();
        store::save_orchestrator(dir, &orch).unwrap();
        assert!(!ask_panel::should_trigger(orch.state()));
        let result = handle_suspension(&orch, dir);
        assert_eq!(result, Ok(EXIT_ESCALATED));
        // state.json 未被修改（仍为 escalated）。
        let restored = store::load_orchestrator(dir).unwrap();
        assert_eq!(restored.state(), State::Escalated);
        std::env::remove_var("ALFRED_NO_ASK_PANEL");
    }

    #[test]
    fn no_ask_panel_flag_skips_panel() {
        // ALFRED_NO_ASK_PANEL=1 显式跳过 pi 卡片，与 offline 并存都返回 false。
        std::env::set_var("ALFRED_OFFLINE", "1");
        std::env::set_var("ALFRED_NO_ASK_PANEL", "1");
        assert!(!ask_panel::should_trigger(State::Escalated));
        assert!(!ask_panel::should_trigger(State::PlanRejected));
        std::env::remove_var("ALFRED_NO_ASK_PANEL");
    }

    #[test]
    fn ask_panel_does_not_trigger_on_completed_run() {
        // 正常完成（exit 0）和其他非挂起态不触发 ask panel。
        std::env::set_var("ALFRED_OFFLINE", "1");
        assert!(!ask_panel::should_trigger(State::Completed));
        assert!(!ask_panel::should_trigger(State::Planning));
        assert!(!ask_panel::should_trigger(State::Executing));
    }
    #[test]
    fn read_last_verdict_returns_last_jsonl_line() {
        // read_last_verdict 读 verdicts.jsonl 最后一条，含 type/verdict/timestamp。
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join(VERDICTS_FILE),
            r#"{"type":"plan","verdict":{"pass":true},"timestamp":1}
{"type":"exec","verdict":{"value":"I"},"timestamp":2}
"#,
        )
        .unwrap();
        let last = read_last_verdict(dir).unwrap();
        assert_eq!(last["type"], "exec");
        assert_eq!(last["verdict"]["value"], "I");
    }

    #[test]
    fn read_last_verdict_errors_on_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(VERDICTS_FILE), "").unwrap();
        assert!(read_last_verdict(dir).is_err());
    }
}
