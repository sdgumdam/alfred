//! 会话文档维护者（③重做：宿主 pi + 滚动维护 + key_file_paths 真实数据源）。
//!
//! 属主 08-20："planner 分两个 agent——一个负责对话，上下文由另外一个维护者
//! **定期**维护，保证有计划需要读的关键的文件路径、关键结论、reviewer 的审查
//! 意见"。
//!
//! **维护者 = 宿主 pi agent**（照 converse 的 host.rs 范式）：
//! `pi -p --no-session -nc --system-prompt <MAINTAIN_SYSTEM_PROMPT> -e <planner AGT>`，
//! cwd=项目根，模型 config.yml 单一真源（run 级 pi-config），AGT 拦写+拦读 run
//! 治理产物——维护者是 planner 侧组件，同受不可知约束（它维护 planner 记忆，
//! 不能见 reviewer 痕迹）。产出（更新后的会话文档 JSON）写
//! `<run>/planner/outputs/session.json`，宿主收割。
//!
//! **触发（"定期" = 滚动）**：
//! - [`MaintainTrigger::ConverseDone`]：每轮 converse 落定后（pi 答复/建图产出
//!   后立即）——滚动维护，下一轮 converse 就能用上新记忆；
//! - [`MaintainTrigger::PlanReviewed`]：计划审查结论落定后——审查意见进记忆
//!   （**经 disguise 投影**：审查理由先经 `disguise_rejection` 转写为属主口吻
//!   中性文本再喂维护者，review_summary 落盘的是中性转写）。
//!
//! **key_file_paths 真实数据源**：planner 宿主 pi 每轮真实读过的宿主路径——
//! 编排器 converse 前快照 AGT 审计行数、converse 后增量提取 allow read 路径
//! （`host::snapshot_audit_lines` / `host::extract_allow_read_paths`，确定性
//! 提取+去重）喂给维护者 prompt；**deny 记录不喂**（deny 路径本身泄露治理面）。
//! 维护者 LLM 判关键性——非全部路径都关键，这是它的智力职责。
//!
//! 不可知：维护者 prompt 零 reviewer 痕迹（审查意见经 disguise 投影、审计 deny
//! 不提取）；llm-calls/ 落盘（P9 证据链，role=maintain），e2e 从记录断言。

use crate::converse::project_session_doc;
use std::path::Path;

use alfred_core::session::SessionDoc;
use alfred_executor::config::ExecutorModel;
use anyhow::{bail, Context, Result};

use crate::host::{
    prepare_host_agt, spawn_planner_pi, write_pi_config, PlannerHostOptions, OUTPUTS_DIR,
    PI_CONFIG_DIR, PLANNER_WORK_DIR,
};
use crate::llm::{log_llm_call, ChatMessage, LlmCallRecord};

/// 维护产出文件名（`<run>/planner/outputs/session.json`）。
pub const MAINTAIN_OUTPUT_FILE: &str = "session.json";

/// 维护触发（滚动维护的两个事件点，方案 §二）。
#[derive(Debug, Clone)]
pub enum MaintainTrigger {
    /// 每轮 converse 落定后（滚动维护——下轮 converse 即用上新记忆）。
    ConverseDone {
        /// 本轮 pi 真实读过的宿主路径（AGT 审计 allow read 增量，确定性提取）。
        read_paths: Vec<String>,
        /// 本轮对话产出摘要（答复文本或计划摘要）——维护者判 key_conclusions。
        reply_summary: String,
    },
    PlanReviewed {
        /// 审查理由的**伪装转写**（属主口吻中性文本，`disguise_rejection` 产物）。
        disguised_review: String,
    },
}

/// 维护者在**投影空间**工作（同 converse 投影：第三段命名 owner_feedback + 内容
/// 中性化）——维护者 prompt/产出字面零禁词（不可知），收割时映射回磁盘真源
/// review_summary（审计真源字段名不变）。alias 兼容维护者回写原字段名的形态。
#[derive(serde::Deserialize)]
struct MaintainedProjection {
    // 无 default（MaintainerAudit P2）：错形态 JSON（缺字段/误名）解析必须报错，
    // 不许静默清空整份记忆——收割是唯一写入口，坏产出要可见。rename+alias 双名
    // 兼容维护者回写原字段名 review_summary 的形态。
    key_file_paths: Vec<String>,
    key_conclusions: Vec<String>,
    #[serde(rename = "owner_feedback", alias = "review_summary")]
    review_summary: Vec<String>,
}

impl MaintainedProjection {
    fn into_session_doc(self) -> SessionDoc {
        // 禁词回流封堵（MaintainerAudit P2，真跑实证"否决"进 key_conclusions）：
        // 收割是会话文档唯一写入口——维护者 LLM 产出可能夹带结构化审查词，逐段
        // 复用 disguise 既有管线（neutralize → 残留禁词回退中性模板）后落盘，
        // 保证磁盘真源三段零禁词（converse 投影层 sanitize 只是第二道，不是
        // 唯一防线）。key_file_paths 同查：路径含禁词（如 …/review/x.md）同样
        // 回退中性模板——回流封堵不设免检面。
        let mut key_file_paths = self.key_file_paths;
        crate::disguise::sanitize_review_summary(&mut key_file_paths);
        let mut key_conclusions = self.key_conclusions;
        for c in key_conclusions.iter_mut() {
            *c = crate::disguise::neutralize_review_language(c);
        }
        crate::disguise::sanitize_review_summary(&mut key_conclusions);
        let mut review_summary = self.review_summary;
        crate::disguise::sanitize_review_summary(&mut review_summary);
        SessionDoc {
            key_file_paths,
            key_conclusions,
            review_summary,
        }
    }
}

/// 维护者 system prompt（唯一真源）：判关键性、滚动累积、只写会话文档。
///
/// 不可知：字段名用投影形态 owner_feedback、不出现审查者/否决/打分等治理禁词；
/// 第三段用中性描述——磁盘真源字段名（review_summary）只在收割映射处出现，
/// 不进维护者 prompt/产出。
const MAINTAIN_SYSTEM_PROMPT: &str = r#"你是治理系统的会话文档维护者。你的唯一任务：根据本轮新增信息更新规划器的记忆文档（会话文档），并把更新后的完整文档写入指定文件。你只做记忆维护。

会话文档固定三段（JSON 对象）：
- key_file_paths：规划器后续需要参考的关键文件路径（只收宿主上真实存在且对规划有价值的路径；本轮探查读过的路径里只有关键的才收，不是全部照抄）；
- key_conclusions：已经确立的关键结论（如技术选型结果、属主补充的新要求；本轮新确立的才新增，已有的不重复）；
- owner_feedback：属主对之前方案的反馈摘要（属主口吻的简短转写）。

纪律：
- 只输出三段 JSON 对象；条目用简短中文；
- 滚动累积：保留原文档中仍然有效的条目，不是每轮清空重写；
- 空段保持空数组；
- 产出文件只能用 write 工具写入；严禁用 bash 重定向（> / >> / tee / dd 等）创建或改写任何文件。"#;

/// 维护者产出路径规则（追加到 system prompt；照 converse `output_paths_section` 范式）。
fn maintain_output_section(outputs_dir: &Path) -> String {
    let out = outputs_dir.display();
    format!(
        r#"

产出方式（强制）：
- 更新后的完整会话文档（三段 JSON 对象）用 write 工具写入：{out}/{MAINTAIN_OUTPUT_FILE}
- 只写这一个文件；不要写产出目录之外的任何文件（会被拒绝）。
- 产出只用 write 工具；禁止用 bash 重定向（> >> tee 等）写任何文件。
写完即结束。"#
    )
}

/// 触发载荷 → 维护者 prompt 的载荷段（prompt 构造唯一真源，测试同源断言）。
fn trigger_payload_section(trigger: &MaintainTrigger) -> String {
    match trigger {
        MaintainTrigger::ConverseDone {
            read_paths,
            reply_summary,
        } => {
            let paths = if read_paths.is_empty() {
                "（本轮无探查读取记录）".to_string()
            } else {
                read_paths
                    .iter()
                    .map(|p| format!("- {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "本轮探查读过（宿主路径，判关键性收 key_file_paths）：\n{paths}\n\n本轮对话产出摘要（判 key_conclusions）：\n{reply_summary}"
            )
        }
        MaintainTrigger::PlanReviewed { disguised_review } => {
            format!("属主对本轮方案的反馈（转写进 owner_feedback）：\n{disguised_review}")
        }
    }
}

/// 运行维护者一轮：喂当前会话文档 + 触发载荷 → 收割更新后的 SessionDoc。
///
/// **离线**（`ALFRED_OFFLINE=1` / `ALFRED_PLANNER_OFFLINE=1` /
/// `ALFRED_MAINTAIN_OFFLINE=1` 任一）：确定性直通——不跑 pi，收割注入文件
/// `ALFRED_MAINTAIN_OFFLINE_FILE`
/// （更新后的完整会话文档 JSON）作为维护产出（e2e 离线回归；llm-calls 照落
/// transport=offline 记录，prompt/触发载荷断言面与真跑同构）。不设注入文件 →
/// 恒等直通（文档不变；与 converse 离线直通同构）。
///
/// 真 LLM：宿主 pi 短会话（AGT 拦写+拦读同 converse——维护者同受不可知约束）；
/// 收割 `session.json` 必须存在且为合法会话文档（无静默出口）；llm-calls 落盘
/// （role=maintain）。维护失败显式报错——记忆坏了要可见，不悄悄放行。
pub fn run_maintain(
    opts: &PlannerHostOptions,
    model: &ExecutorModel,
    doc: &SessionDoc,
    trigger: &MaintainTrigger,
) -> Result<SessionDoc> {
    let offline = std::env::var("ALFRED_OFFLINE").as_deref() == Ok("1")
        || std::env::var("ALFRED_PLANNER_OFFLINE").as_deref() == Ok("1")
        || std::env::var("ALFRED_MAINTAIN_OFFLINE").as_deref() == Ok("1");
    if !offline && model.raw_id {
        bail!(
            "maintainer host pi requires an openai-compatible provider model, got raw builtin model '{}'",
            model.model
        );
    }

    let work = opts.run_dir.join(PLANNER_WORK_DIR);
    let outputs_dir = work.join(OUTPUTS_DIR);
    std::fs::create_dir_all(&outputs_dir)
        .with_context(|| format!("create planner outputs dir {}", outputs_dir.display()))?;

    // 跨轮残留清理：本轮收割唯一候选 session.json。
    let out_path = outputs_dir.join(MAINTAIN_OUTPUT_FILE);
    match std::fs::remove_file(&out_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove stale {}", MAINTAIN_OUTPUT_FILE)),
    }

    // prompt（stdin）= 当前会话文档（**投影空间**：owner_feedback 命名+净化——
    // 维护者字面零禁词，同 converse 投影）+ 本轮触发载荷。
    let projection = project_session_doc(doc);
    let session =
        serde_json::to_string_pretty(&projection).context("serialize SessionDoc projection")?;
    let payload = trigger_payload_section(trigger);
    let prompt = format!("当前会话文档：\n{session}\n\n{payload}");
    // system prompt（MaintainerAudit P3 审计真值）：静态 schema + 产出路径规则，
    // 提升到离线分支之前构造——离线 llm-calls 记录与真跑同值（converse 落全量
    // messages 同范式），llm-calls 不再有"system 恒空串"的记录失真。
    let system_prompt = format!(
        "{}{}",
        MAINTAIN_SYSTEM_PROMPT,
        maintain_output_section(&outputs_dir)
    );

    let (text, offline, transport) = if offline {
        match std::env::var("ALFRED_MAINTAIN_OFFLINE_FILE") {
            // 离线注入：收割注入文件（e2e 从记录断言滚动/disguise 语义）。
            Ok(path) => (
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read offline maintain output {path}"))?,
                true,
                "offline",
            ),
            // 离线无注入：恒等直通（文档不变；与 converse 离线直通同构——
            // llm-calls 记录喂=产，审计链完整，无静默失败）。
            Err(_) => (
                serde_json::to_string_pretty(&projection).context("serialize identity maintain")?,
                true,
                "offline",
            ),
        }
    } else {
        // AGT 拦截层 + run 级 pi 配置（converse 同款单一真源）。
        let agt_ext = prepare_host_agt(&work, &opts.agt, &outputs_dir, &opts.project_root)?;
        let pi_config_dir = work.join(PI_CONFIG_DIR);
        write_pi_config(&pi_config_dir, model)?;
        let pi_stdout = spawn_planner_pi(
            opts,
            model,
            &work,
            &pi_config_dir,
            agt_ext.as_deref(),
            &system_prompt,
            &prompt,
        )?;
        // 收割：session.json 必须存在且解析为合法会话文档（无静默出口）。
        let text = std::fs::read_to_string(&out_path).with_context(|| {
            format!("maintainer produced no {MAINTAIN_OUTPUT_FILE} (pi stdout: {pi_stdout})")
        })?;
        (text, false, "host_pi")
    };
    if text.trim().is_empty() {
        bail!("maintainer produced empty {MAINTAIN_OUTPUT_FILE}");
    }
    let updated: SessionDoc = serde_json::from_str::<MaintainedProjection>(text.trim())
        .with_context(|| format!("maintainer output not valid SessionDoc: {text}"))?
        .into_session_doc();

    // llm-calls 审计（P9 证据链；role=maintain）。prompt 零 reviewer 痕迹：
    // 审查意见已经 disguise 投影、审计 deny 路径未进 prompt。
    let record = LlmCallRecord {
        ts: alfred_core::util::now_rfc3339(),
        role: "maintain".into(),
        offline,
        model: model.inspect_model_id(),
        transport: transport.to_string(),
        // system prompt 审计真值（MaintainerAudit P3）：记录提升后的构造真值，
        // 离线/真跑同值（与 converse 落全量 messages 同范式），不再恒空串失真。
        messages: vec![ChatMessage::system(system_prompt), ChatMessage::user(prompt)],
        response: text.trim().to_string(),
        ok: true,
        error: None,
    };
    log_llm_call(&opts.run_dir, &record)?;

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintain_output_section_names_session_json() {
        let s = maintain_output_section(Path::new("/run/planner/outputs"));
        assert!(s.contains("/run/planner/outputs/session.json"));
    }

    /// 不可知防线（静态层）：维护者 system prompt 零 reviewer 痕迹。
    #[test]
    fn maintain_system_prompt_has_no_reviewer_signals() {
        for w in crate::disguise::FORBIDDEN_SIGNALS {
            assert!(
                !MAINTAIN_SYSTEM_PROMPT.to_lowercase().contains(w),
                "维护者 system prompt 泄露禁词 {w}"
            );
        }
    }

    /// ConverseDone 载荷：喂路径清单 + 产出摘要（prompt 构造同源）。
    #[test]
    fn converse_done_payload_lists_read_paths() {
        let trigger = MaintainTrigger::ConverseDone {
            read_paths: vec!["/ws/src/main.rs".into()],
            reply_summary: "答复：确认用 Rust".into(),
        };
        let prompt = trigger_payload_section(&trigger);
        assert!(prompt.contains("/ws/src/main.rs"));
        assert!(prompt.contains("答复：确认用 Rust"));
        assert!(prompt.contains("key_file_paths"));
    }

    /// PlanReviewed 载荷：只喂伪装转写（属主口吻），无审查语义词。
    #[test]
    fn plan_reviewed_payload_is_disguised() {
        let trigger = MaintainTrigger::PlanReviewed {
            disguised_review:
                "我重新看了下需求，你给的方案跟我要的不太对。你再按我原来的需求重新弄一版。".into(),
        };
        let prompt = trigger_payload_section(&trigger);
        assert!(prompt.contains("属主对本轮方案的反馈"));
        assert!(prompt.contains("重新弄一版"));
        assert!(crate::disguise::contains_forbidden_signal(&prompt).is_none());
    }

    /// 离线闸门三 env 任一命中（MaintainerAudit P1：r1-r4/r6c/r6d/agt-default
    /// 套件只用 ALFRED_PLANNER_OFFLINE=1——此前维护者闸门不认它，会真跑 LLM）。
    /// 断言面 = 离线路径真身：raw_id 模型在离线分支不 bail（在线会 bail），
    /// 且产出 llm-calls role=maintain、transport=offline 记录。
    #[test]
    fn offline_gate_honors_planner_offline_var() {
        use crate::host::PlannerHostOptions;
        use alfred_core::session::SessionDoc;
        let keys = ["ALFRED_OFFLINE", "ALFRED_PLANNER_OFFLINE", "ALFRED_MAINTAIN_OFFLINE"];
        for key in keys {
            let prev = std::env::var_os(key);
            std::env::set_var(key, "1");
            let base = std::env::temp_dir().join(format!("alfred-maintain-gate-{}-{}", key.to_lowercase(), std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let opts = PlannerHostOptions {
                run_dir: base.clone(),
                project_root: base.clone(),
                time_limit_secs: 5,
                agt: alfred_executor::agt::AgtSource::Builtin,
            };
            let model = ExecutorModel {
                provider: "mockllm".into(),
                model: "mockllm/model".into(),
                base_url: String::new(),
                api_key: String::new(),
                max_tokens: 1024,
                raw_id: true,
            };
            let doc = SessionDoc::default();
            let trigger = MaintainTrigger::ConverseDone {
                read_paths: vec![],
                reply_summary: "答复：确认用 Rust".into(),
            };
            let updated = run_maintain(&opts, &model, &doc, &trigger)
                .unwrap_or_else(|e| panic!("{key}=1 应走离线直通，却报错：{e}"));
            assert_eq!(updated, doc, "{key}=1 无注入文件 → 恒等直通（文档不变）");
            // P3 审计真值：记录里的 system prompt = 真值（静态 schema + 产出
            // 路径规则），离线与真跑同值，不再是恒空串。
            let rec_path = base
                .join("llm-calls")
                .read_dir()
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .next()
                .expect("{key}=1 应落 llm-calls role=maintain 记录");
            let rec: LlmCallRecord = serde_json::from_str(
                &std::fs::read_to_string(&rec_path).unwrap(),
            )
            .unwrap();
            assert_eq!(rec.role, "maintain", "role=maintain");
            assert!(rec.offline && rec.transport == "offline");
            assert_eq!(rec.messages.len(), 2, "system + user 两条");
            assert_eq!(rec.messages[0].role, "system");
            assert!(
                rec.messages[0].content.starts_with("你是治理系统的会话文档维护者")
                    && rec.messages[0].content.contains("session.json"),
                "system prompt 记录真值（含 schema 与产出路径规则）"
            );
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// 收割 schema 严格（MaintainerAudit P2）：错形态 JSON 缺任一字段 → 解析报错
    /// 不静默清空；三字段齐（含 alias review_summary 双名）→ 正常解析。
    #[test]
    fn maintained_projection_rejects_missing_fields() {
        // 缺 key_file_paths → 错（此前 default 静默清空整份记忆）
        let missing = serde_json::from_str::<MaintainedProjection>(
            r#"{"key_conclusions": [], "owner_feedback": []}"#,
        );
        assert!(missing.is_err(), "缺 key_file_paths 必须报错（不静默清空）");
        // 缺 key_conclusions → 错
        let missing2 = serde_json::from_str::<MaintainedProjection>(
            r#"{"key_file_paths": [], "owner_feedback": []}"#,
        );
        assert!(missing2.is_err(), "缺 key_conclusions 必须报错");
        // 三字段齐（owner_feedback 命名）→ 正常
        let ok = serde_json::from_str::<MaintainedProjection>(
            r#"{"key_file_paths": ["/a.rs"], "key_conclusions": ["用 Rust"], "owner_feedback": []}"#,
        )
        .expect("三字段齐应解析成功");
        assert_eq!(ok.key_file_paths, vec!["/a.rs".to_string()]);
        // alias：维护者回写磁盘真源字段名 review_summary 同样合法
        let aliased = serde_json::from_str::<MaintainedProjection>(
            r#"{"key_file_paths": [], "key_conclusions": [], "review_summary": ["属主反馈"]}"#,
        )
        .expect("alias review_summary 应解析成功");
        assert_eq!(aliased.review_summary, vec!["属主反馈".to_string()]);
    }

    /// 禁词回流封堵（MaintainerAudit P2）：维护者产出夹带结构化审查词 → 收割
    /// 时 neutralize，残留禁词回退中性模板——key_conclusions/key_file_paths/
    /// review_summary 三段都查，落盘真源零禁词。
    #[test]
    fn harvest_scrubs_forbidden_language_all_sections() {
        let p = MaintainedProjection {
            key_file_paths: vec!["/ws/src/main.rs".into(), "/ws/评审记录/x.md".into()],
            key_conclusions: vec![
                "技术选型用 Rust".into(),
                "方案被否决：规划器要重做".into(),
            ],
            review_summary: vec!["审查未通过".into()],
        };
        let doc = p.into_session_doc();
        for (seg, entries) in [
            ("key_file_paths", &doc.key_file_paths),
            ("key_conclusions", &doc.key_conclusions),
            ("review_summary", &doc.review_summary),
        ] {
            for e in entries {
                assert!(
                    crate::disguise::contains_forbidden_signal(e).is_none(),
                    "{seg} 段禁词回流：{e}"
                );
            }
        }
        // neutralize 保语义（非全量替换）："否决"→"不行" 留在原句
        assert!(
            doc.key_conclusions.iter().any(|c| c.contains("不行")),
            "中和后语义保留：{:?}",
            doc.key_conclusions
        );
        // 干净条目原样透传（不误伤）
        assert!(doc.key_file_paths.contains(&"/ws/src/main.rs".to_string()));
    }
}
