//! TUI 状态看板数据聚合器（施工方案-TUI界面.md S2A，右列数据源）。
//!
//! 纯逻辑组件（零 UI 依赖，不 import ratatui/crossterm）：[`snapshot`] 一次
//! 全量读 run 目录聚合成 [`DashboardSnapshot`]，供 TUI 轮询渲染——不做增量、
//! 不做 watch，每次调用即一次完整快照。
//!
//! 数据源与降级（ground truth = 真实 run 目录落盘形态，如
//! `~/.local/state/alfred/runs/run-18d34d885198d38001`）：
//!
//! | 快照字段 | 首选源 | 降级源（缺失/坏形态时） |
//! |---|---|---|
//! | run_id / state / updated_at / verdicts / 维护者 | state.json | run_id←目录名；state/updated_at←audit.jsonl；verdicts←plan-verdicts.json / exec-verdicts.json；维护者←planner/outputs/session.json |
//! | nodes / references | dagspec.json | state.json 内嵌 dagspec |
//! | artifacts | ws/ 文件树 | 空 Vec |
//!
//! 一切缺源静默降级（空串/空 Vec/None）：run 早期 state.json 可能尚未落盘、
//! dagspec 可能还是 null（如 run-18d353f6fc70347801）、文件可能正被写入
//! （半截 JSON）——轮询看板宁可短暂缺数据也不报错。只有 run 目录本身不存在
//! 才返回 Err。
//!
//! 源优先级依据：`persist_governance_run` 每次转移整体重写 state.json（verdict
//! 历史单一真源）；dagspec.json 由 commit_intent 通道先于 state.json 落盘，故
//! 两者共存时恒为同值或 dagspec.json 更新一拍——优先读 dagspec.json。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use alfred_core::{DagSpec, ExecVerdict, PlanVerdict};
use anyhow::{ensure, Result};
use serde::Deserialize;

const STATE_FILE: &str = "state.json";
const DAGSPEC_FILE: &str = "dagspec.json";
const AUDIT_FILE: &str = "audit.jsonl";
const PLAN_VERDICTS_FILE: &str = "plan-verdicts.json";
const EXEC_VERDICTS_FILE: &str = "exec-verdicts.json";
/// 维护者产出文件（`<run>/planner/outputs/session.json`，
/// alfred_planner::maintain::MAINTAIN_OUTPUT_FILE）。
const PLANNER_SESSION_FILE: &str = "planner/outputs/session.json";
/// 持久工作区（产物树根；内含 .git 仓库，列举时排除）。
const WS_DIR: &str = "ws";

/// 状态看板快照（右列渲染数据源，一次全量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardSnapshot {
    pub run_id: String,
    /// 治理态（state.json `state_machine.state`；缺 → audit.jsonl 最后一条
    /// `state_entered`；再缺 → 空串）。
    pub state: String,
    /// 计划节点（dagspec）。
    pub nodes: Vec<NodeView>,
    /// 维护者状态（会话文档三段）。
    pub maintainer: MaintainerView,
    /// 参考卷（dagspec volumes host_path 文件名，跨节点去重保序）。
    pub references: Vec<String>,
    /// 产物（ws 树文件 + 行数，.git 除外，按路径序）。
    pub artifacts: Vec<ArtifactView>,
    /// 计划审查结论（历史最新一条）。
    pub plan_verdict: Option<VerdictView>,
    /// 执行审查结论（全部历史）。
    pub exec_verdicts: Vec<VerdictView>,
    pub updated_at: String,
}

/// dagspec 节点视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeView {
    pub id: String,
    pub summary: String,
}

/// 维护者状态视图（会话文档三段真源字段的直读投影）。
///
/// 双名兼容：磁盘真源（state.json `session_doc`）用 `review_summary`；
/// planner 投影空间产出（planner/outputs/session.json）用 `owner_feedback`
/// （同 alfred_planner::maintain::MaintainedProjection 的 rename+alias 口径）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MaintainerView {
    #[serde(default)]
    pub key_file_paths: Vec<String>,
    #[serde(default)]
    pub key_conclusions: Vec<String>,
    #[serde(default, alias = "owner_feedback")]
    pub review_summary: Vec<String>,
}

impl MaintainerView {
    fn is_empty(&self) -> bool {
        self.key_file_paths.is_empty()
            && self.key_conclusions.is_empty()
            && self.review_summary.is_empty()
    }
}

/// 产物视图（ws 树文件；行数按 wc -l 口径 = 换行符计数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactView {
    /// 相对 ws 根的路径（`/` 分隔）。
    pub path: String,
    pub lines: usize,
}

/// 审查结论视图（计划/执行统一形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictView {
    /// 结论徽标：计划审查 `pass`/`reject`；执行审查 `C`/`I`/`P`。
    pub outcome: String,
    /// 置信度（执行审查有：high/medium/low；计划审查无）。
    pub confidence: Option<String>,
    /// 结论理由/解释全文。
    pub detail: String,
}

/// 聚合 run 目录 → 看板快照（一次全量读；缺源降级，见模块注释）。
pub fn snapshot(run_dir: &Path) -> Result<DashboardSnapshot> {
    ensure!(run_dir.is_dir(), "run 目录不存在: {}", run_dir.display());

    let state_file: Option<StateFile> = read_json(&run_dir.join(STATE_FILE));
    let audit = AuditFallback::read(&run_dir.join(AUDIT_FILE));

    let run_id = state_file
        .as_ref()
        .filter(|s| !s.run_id.is_empty())
        .map(|s| s.run_id.clone())
        .unwrap_or_else(|| dir_name(run_dir));
    let state = state_file
        .as_ref()
        .filter(|s| !s.state_machine.state.is_empty())
        .map(|s| s.state_machine.state.clone())
        .or(audit.last_state)
        .unwrap_or_default();
    let updated_at = state_file
        .as_ref()
        .filter(|s| !s.updated_at.is_empty())
        .map(|s| s.updated_at.clone())
        .or(audit.last_ts)
        .unwrap_or_default();

    // nodes/references：优先 dagspec.json（每周期先落盘、恒为最新拍），
    // 缺/坏 → state.json 内嵌副本。
    let dagspec = read_json::<DagSpec>(&run_dir.join(DAGSPEC_FILE))
        .or_else(|| state_file.as_ref().and_then(|s| s.dagspec.clone()));
    let nodes: Vec<NodeView> = dagspec
        .as_ref()
        .map(|spec| {
            spec.nodes
                .iter()
                .map(|n| NodeView {
                    id: n.id.clone(),
                    summary: n.summary.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    let references = dagspec
        .as_ref()
        .map(reference_names)
        .unwrap_or_default();

    // 维护者：state.json session_doc（磁盘真源，已过禁词封堵）；空文档 →
    // planner 投影产出兜底（维护刚落盘、state.json 未 persist 的窗口）。
    let mut maintainer = state_file
        .as_ref()
        .map(|s| s.session_doc.clone())
        .unwrap_or_default();
    if maintainer.is_empty() {
        maintainer = read_json(&run_dir.join(PLANNER_SESSION_FILE)).unwrap_or_default();
    }

    // verdicts：单一真源 state.json；state.json 缺/坏 → 独立投影文件兜底。
    let (plan_verdicts, exec_verdicts) = match &state_file {
        Some(s) => (s.plan_verdicts.clone(), s.exec_verdicts.clone()),
        None => (
            read_json(&run_dir.join(PLAN_VERDICTS_FILE)).unwrap_or_default(),
            read_json(&run_dir.join(EXEC_VERDICTS_FILE)).unwrap_or_default(),
        ),
    };
    let plan_verdict = plan_verdicts.last().map(|v| VerdictView {
        outcome: if v.pass { "pass" } else { "reject" }.to_string(),
        confidence: None,
        detail: v.reason.clone(),
    });
    let exec_verdicts = exec_verdicts
        .iter()
        .map(|v| VerdictView {
            outcome: json_label(&v.value),
            confidence: Some(json_label(&v.confidence)),
            detail: v.explanation.clone(),
        })
        .collect();

    let ws_root = run_dir.join(WS_DIR);
    let mut artifacts = Vec::new();
    collect_ws_files(&ws_root, &ws_root, &mut artifacts);
    artifacts.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(DashboardSnapshot {
        run_id,
        state,
        nodes,
        maintainer,
        references,
        artifacts,
        plan_verdict,
        exec_verdicts,
        updated_at,
    })
}

/// state.json 宽松投影：只取看板字段，未知字段忽略、缺失字段缺省——旧版
/// state.json（少字段）与新版（多字段）都能读，坏形态整文件降级。
#[derive(Deserialize)]
struct StateFile {
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    state_machine: StateMachineFile,
    #[serde(default)]
    dagspec: Option<DagSpec>,
    #[serde(default)]
    session_doc: MaintainerView,
    #[serde(default)]
    plan_verdicts: Vec<PlanVerdict>,
    #[serde(default)]
    exec_verdicts: Vec<ExecVerdict>,
    #[serde(default)]
    updated_at: String,
}

#[derive(Deserialize, Default)]
struct StateMachineFile {
    #[serde(default)]
    state: String,
}

/// audit.jsonl 降级信息（state.json 缺/坏时）：最后一条 `state_entered` 的
/// 状态 + 最后一条事件时间戳。逐行容错——追加日志的半截尾行直接跳过。
#[derive(Default)]
struct AuditFallback {
    last_state: Option<String>,
    last_ts: Option<String>,
}

impl AuditFallback {
    fn read(path: &Path) -> Self {
        let mut fallback = Self::default();
        let Ok(text) = fs::read_to_string(path) else {
            return fallback;
        };
        for line in text.lines() {
            let Ok(ev): Result<AuditEventFile, _> = serde_json::from_str(line) else {
                continue;
            };
            if !ev.ts.is_empty() {
                fallback.last_ts = Some(ev.ts);
            }
            if ev.event == "state_entered" && !ev.data.state.is_empty() {
                fallback.last_state = Some(ev.data.state);
            }
        }
        fallback
    }
}

#[derive(Deserialize, Default)]
struct AuditEventFile {
    #[serde(default)]
    event: String,
    #[serde(default)]
    data: AuditDataFile,
    #[serde(default)]
    ts: String,
}

#[derive(Deserialize, Default)]
struct AuditDataFile {
    #[serde(default)]
    state: String,
}

/// 读 JSON 文件并解析为 T；任何读/解析失败 → None（缺源降级，不报错）。
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 目录名（run 目录名即 run_id 形态，如 run-18d34d885198d38001）。
fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 参考卷名集合：全节点 sandbox volumes 的 host_path 文件名，去重保序。
fn reference_names(dagspec: &DagSpec) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for node in &dagspec.nodes {
        for volume in &node.sandbox.volumes {
            let Some(file_name) = Path::new(&volume.host_path).file_name() else {
                continue;
            };
            let name = file_name.to_string_lossy().into_owned();
            if !name.is_empty() && seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    names
}

/// 枚举 ws 文件树（跳过 `.git`；行数 = 换行符计数，wc -l 口径）。
fn collect_ws_files(root: &Path, dir: &Path, out: &mut Vec<ArtifactView>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            collect_ws_files(root, &entry.path(), out);
        } else if file_type.is_file() {
            let lines = fs::read(entry.path())
                .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count())
                .unwrap_or(0);
            if let Ok(rel) = entry.path().strip_prefix(root) {
                out.push(ArtifactView {
                    path: rel.to_string_lossy().into_owned(),
                    lines,
                });
            }
        }
    }
}

/// 枚举 → 徽标串（复用 serde 序列化真源：VerdictGrade→"C"/"I"/"P"，
/// Confidence→"high"/"medium"/"low"）。
fn json_label<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use alfred_core::{Contract, PlanNode, VolumeMount};
    use serde_json::json;

    /// 合成 run 目录（临时目录，同 tag 复用时先清空）。
    fn temp_run_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "alfred-dashboard-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// ws 内写一个产物文件（自动建父目录）。
    fn ws_file(run_dir: &Path, rel: &str, content: &str) {
        let path = run_dir.join(WS_DIR).join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// 带参考卷的节点。
    fn node_with_volumes(id: &str, summary: &str, host_paths: &[&str]) -> PlanNode {
        let contract = Contract {
            prompt: "p".into(),
            acceptance_criteria: "a".into(),
            reviewer_models: vec![],
        };
        let mut node = PlanNode::new(id, summary, contract);
        node.sandbox.volumes = host_paths
            .iter()
            .map(|host| VolumeMount {
                host_path: (*host).into(),
                container_path: format!("/references/{host}"),
                mode: "ro".into(),
            })
            .collect();
        node
    }

    #[test]
    fn full_run_snapshot_aggregates_all_sources() {
        let dir = temp_run_dir("full");

        // 两节点；第二节点复用第一节点参考卷 + 新卷 → references 去重保序。
        let dagspec = DagSpec::new(
            "req-1",
            vec![
                node_with_volumes("task-1", "通读转录", &["/refs/omp-session.jsonl"]),
                node_with_volumes(
                    "task-2",
                    "提炼文档",
                    &["/refs/omp-session.jsonl", "/data/ledger.jsonl"],
                ),
            ],
        );
        fs::write(
            dir.join(DAGSPEC_FILE),
            serde_json::to_string(&dagspec).unwrap(),
        )
        .unwrap();

        let state = json!({
            "run_id": "run-test01",
            "state_machine": {"state": "executing"},
            "dagspec": serde_json::to_value(&dagspec).unwrap(),
            "session_doc": {
                "key_file_paths": ["/a/b.md"],
                "key_conclusions": ["选型已定"],
                "review_summary": ["属主反馈"],
            },
            "plan_verdicts": [
                {"pass": false, "reason": "第一轮打回"},
                {"pass": true, "reason": "第二轮通过"},
            ],
            "exec_verdicts": [
                {"value": "C", "confidence": "high", "evidence": [], "explanation": "产物达标"},
            ],
            "updated_at": "2026-09-09T01:02:03Z",
        });
        fs::write(dir.join(STATE_FILE), state.to_string()).unwrap();

        // audit 尾随 completed，但 state.json 在场为权威 → state 不被覆盖。
        fs::write(
            dir.join(AUDIT_FILE),
            concat!(
                r#"{"data":{"state":"planning"},"event":"state_entered","ts":"t0"}"#,
                '\n',
                r#"{"data":{"state":"completed"},"event":"state_entered","ts":"t9"}"#,
                '\n',
            ),
        )
        .unwrap();

        // ws 树：常规多行 / 无尾换行 / 空文件 / .git 排除。
        ws_file(&dir, "docs/report.md", "l1\nl2\nl3\n");
        ws_file(&dir, "docs/notes/n.txt", "a\nb");
        ws_file(&dir, "tools/x.js", "");
        ws_file(&dir, ".git/config", "[core]\n");

        let snap = snapshot(&dir).unwrap();

        assert_eq!(snap.run_id, "run-test01");
        assert_eq!(snap.state, "executing");
        assert_eq!(snap.updated_at, "2026-09-09T01:02:03Z");
        assert_eq!(snap.nodes.len(), 2);
        assert_eq!(snap.nodes[0].id, "task-1");
        assert_eq!(snap.nodes[0].summary, "通读转录");
        assert_eq!(snap.nodes[1].id, "task-2");
        assert_eq!(snap.references, vec!["omp-session.jsonl", "ledger.jsonl"]);
        assert_eq!(snap.maintainer.key_file_paths, vec!["/a/b.md"]);
        assert_eq!(snap.maintainer.key_conclusions, vec!["选型已定"]);
        assert_eq!(snap.maintainer.review_summary, vec!["属主反馈"]);
        let plan = snap.plan_verdict.expect("计划结论取历史最新一条");
        assert_eq!(plan.outcome, "pass");
        assert_eq!(plan.detail, "第二轮通过");
        assert_eq!(plan.confidence, None);
        assert_eq!(snap.exec_verdicts.len(), 1);
        assert_eq!(snap.exec_verdicts[0].outcome, "C");
        assert_eq!(snap.exec_verdicts[0].confidence.as_deref(), Some("high"));
        assert_eq!(snap.exec_verdicts[0].detail, "产物达标");
        assert_eq!(
            snap.artifacts,
            vec![
                ArtifactView {
                    path: "docs/notes/n.txt".into(),
                    lines: 1, // 无尾换行按 wc -l 口径
                },
                ArtifactView {
                    path: "docs/report.md".into(),
                    lines: 3,
                },
                ArtifactView {
                    path: "tools/x.js".into(),
                    lines: 0,
                },
            ]
        );
    }

    #[test]
    fn missing_state_json_degrades_to_audit_and_verdict_files() {
        let dir = temp_run_dir("nostate");

        fs::write(
            dir.join(AUDIT_FILE),
            concat!(
                r#"{"data":{"request_id":"req-9"},"event":"governance_started","ts":"t0"}"#,
                '\n',
                r#"{"data":{"state":"planning"},"event":"state_entered","ts":"t1"}"#,
                '\n',
            ),
        )
        .unwrap();
        fs::write(
            dir.join(PLAN_VERDICTS_FILE),
            r#"[{"pass": true, "reason": "通过"}]"#,
        )
        .unwrap();
        fs::write(dir.join(EXEC_VERDICTS_FILE), "[]").unwrap();
        // 维护者降级源：planner 投影形态（owner_feedback 字段名）。
        fs::create_dir_all(dir.join("planner/outputs")).unwrap();
        fs::write(
            dir.join(PLANNER_SESSION_FILE),
            r#"{"key_file_paths": [], "key_conclusions": ["记忆一条"], "owner_feedback": []}"#,
        )
        .unwrap();
        ws_file(&dir, "out.md", "x\n");

        let snap = snapshot(&dir).unwrap();

        // state.json 缺 → run_id 取目录名、state/updated_at 取 audit 降级。
        assert_eq!(
            snap.run_id,
            dir.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(snap.state, "planning");
        assert_eq!(snap.updated_at, "t1");
        assert!(snap.nodes.is_empty());
        assert!(snap.references.is_empty());
        let plan = snap.plan_verdict.expect("verdict 文件兜底");
        assert_eq!(plan.outcome, "pass");
        assert!(snap.exec_verdicts.is_empty());
        assert_eq!(snap.maintainer.key_conclusions, vec!["记忆一条"]);
        assert_eq!(snap.maintainer.review_summary, Vec::<String>::new());
        assert_eq!(snap.artifacts.len(), 1);
        assert_eq!(snap.artifacts[0].path, "out.md");
    }

    #[test]
    fn early_run_without_dagspec_degrades_to_empty() {
        let dir = temp_run_dir("early");

        // run 早期形态：dagspec 还是 null、三段/verdicts 全空（对齐
        // run-18d353f6fc70347801 的 escalated 早停形态）。
        fs::write(
            dir.join(STATE_FILE),
            json!({
                "run_id": "run-early",
                "state_machine": {"state": "planning"},
                "dagspec": null,
                "session_doc": {"key_file_paths": [], "key_conclusions": [], "review_summary": []},
                "plan_verdicts": [],
                "exec_verdicts": [],
                "updated_at": "t-early",
            })
            .to_string(),
        )
        .unwrap();
        // audit 尾行半截 JSON（写入中途被轮询撞上）：跳过不炸。
        fs::write(
            dir.join(AUDIT_FILE),
            concat!(
                r#"{"data":{"state":"planning"},"event":"state_entered","ts":"t0"}"#,
                '\n',
                r#"{"data":{"state":"com"#,
            ),
        )
        .unwrap();

        let snap = snapshot(&dir).unwrap();

        assert_eq!(snap.run_id, "run-early");
        assert_eq!(snap.state, "planning");
        assert_eq!(snap.updated_at, "t-early");
        assert!(snap.nodes.is_empty());
        assert!(snap.references.is_empty());
        assert!(snap.plan_verdict.is_none());
        assert!(snap.exec_verdicts.is_empty());
        assert!(snap.maintainer.is_empty());
        assert!(snap.artifacts.is_empty());
    }

    #[test]
    fn corrupt_state_json_degrades_gracefully() {
        let dir = temp_run_dir("corrupt");

        // 半截 state.json（写入中途）→ 整文件降级。
        fs::write(dir.join(STATE_FILE), r#"{"run_id": "run-corrupt""#).unwrap();
        fs::write(
            dir.join(AUDIT_FILE),
            concat!(
                r#"{"data":{"state":"plan_rejected"},"event":"state_entered","ts":"t5"}"#,
                '\n',
            ),
        )
        .unwrap();
        fs::write(
            dir.join(PLAN_VERDICTS_FILE),
            r#"[{"pass": false, "reason": "打回"}]"#,
        )
        .unwrap();
        fs::write(dir.join(EXEC_VERDICTS_FILE), "[]").unwrap();

        let snap = snapshot(&dir).unwrap();

        assert_eq!(snap.run_id, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(snap.state, "plan_rejected");
        assert_eq!(snap.updated_at, "t5");
        let plan = snap.plan_verdict.expect("verdict 文件兜底");
        assert_eq!(plan.outcome, "reject");
        assert!(snap.exec_verdicts.is_empty());
    }

    #[test]
    fn corrupt_dagspec_file_falls_back_to_embedded() {
        let dir = temp_run_dir("dagspec-corrupt");

        let dagspec = DagSpec::new(
            "req-1",
            vec![node_with_volumes("task-1", "单节点", &["/refs/a.jsonl"])],
        );
        // dagspec.json 半截；state.json 内嵌完好 → 内嵌兜底。
        fs::write(
            dir.join(DAGSPEC_FILE),
            r#"{"request_id": "req-1", "nodes": [{"#,
        )
        .unwrap();
        fs::write(
            dir.join(STATE_FILE),
            json!({
                "run_id": "run-embed",
                "state_machine": {"state": "exec_reviewing"},
                "dagspec": serde_json::to_value(&dagspec).unwrap(),
                "session_doc": {},
                "plan_verdicts": [],
                "exec_verdicts": [],
                "updated_at": "t6",
            })
            .to_string(),
        )
        .unwrap();

        let snap = snapshot(&dir).unwrap();

        assert_eq!(snap.state, "exec_reviewing");
        assert_eq!(snap.nodes.len(), 1);
        assert_eq!(snap.nodes[0].id, "task-1");
        assert_eq!(snap.references, vec!["a.jsonl"]);
    }

    #[test]
    fn snapshot_errors_when_run_dir_missing() {
        let missing = std::env::temp_dir().join(format!(
            "alfred-dashboard-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&missing);
        assert!(snapshot(&missing).is_err());
    }
}
