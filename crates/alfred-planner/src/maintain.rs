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
    #[serde(default)]
    key_file_paths: Vec<String>,
    #[serde(default)]
    key_conclusions: Vec<String>,
    #[serde(default, rename = "owner_feedback", alias = "review_summary")]
    review_summary: Vec<String>,
}

impl MaintainedProjection {
    fn into_session_doc(self) -> SessionDoc {
        SessionDoc {
            key_file_paths: self.key_file_paths,
            key_conclusions: self.key_conclusions,
            review_summary: self.review_summary,
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
/// **离线**（`ALFRED_OFFLINE=1` 或 `ALFRED_MAINTAIN_OFFLINE=1`）：确定性直通——
/// 不跑 pi，收割注入文件 `ALFRED_MAINTAIN_OFFLINE_FILE`（更新后的完整会话文档
/// JSON）作为维护产出（e2e 离线回归；llm-calls 照落 transport=offline 记录，
/// prompt/触发载荷断言面与真跑同构）。不设注入文件 → 显式报错（不静默）。
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
        let system_prompt = format!(
            "{}{}",
            MAINTAIN_SYSTEM_PROMPT,
            maintain_output_section(&outputs_dir)
        );
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
        model: model.inspect_model_id(),
        offline,
        transport: transport.to_string(),
        // system prompt 不进审计（converse 同范式：记录 user 载荷即可——
        // system 是静态 schema 文本，真跑路径已含产出规则）。
        messages: vec![ChatMessage::system(String::new()), ChatMessage::user(prompt)],
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
}
