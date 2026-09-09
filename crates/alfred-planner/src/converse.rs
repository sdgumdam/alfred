//! 对话 agent（converse）：会话文档 + 属主消息 → §2.4 两分支。
//!
//! 施工清单 §2.4：规划器不是单个长对话 agent，而是"会话文档 + 短会话"。
//! 对话 agent 每轮都是短会话、无状态，喂给它两样东西：会话文档 + 属主这轮
//! 说的话；它吐出两种东西之一：
//! - **建图指令序列**（§五 S0 迭代"builder API 模式"），alfred 逐条驱动
//!   GraphBuilder 拼装出 DagSpec，交给编排核心（→ 计划审查/执行）。
//! - **给属主的答复**（纯文本；对话性轮次，不强制产 DagSpec——多轮对话的
//!   答复侧，对话产出建图指令后交编排器接管）。
//!
//! 两种模式：
//! - 真 LLM（默认）：宿主 pi 驱动（`host::run_converse_on_host`：宿主 `pi -p`
//!   单次短会话，cwd=治理对象项目根，按两分支规则产 outputs/instructions.json 或
//!   outputs/reply.txt），宿主按产出文件分派两分支 → GraphBuilder → DagSpec 或
//!   答复；每次调用落盘 llm-calls/（P9 证据）。
//! - 离线（`ALFRED_OFFLINE=1` 或 `ALFRED_PLANNER_OFFLINE=1`）：确定性直通，
//!   两分支由注入文件二选一——`ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json | 建图指令序列>` → 建图指令分支；
//!   `ALFRED_OFFLINE_REPLY_FILE=<reply.txt>` → 答复分支；仍把 would-be 请求
//!   落盘 llm-calls/（e2e 从记录断言会话文档/伪装消息）。

use std::path::{Path, PathBuf};

use alfred_core::builder::{BuildInstruction, GraphBuilder};
use alfred_core::dagspec::DagSpec;
use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_executor::config::ExecutorModel;
use anyhow::{bail, Context, Result};

use crate::disguise::sanitize_review_summary;
use crate::llm::{log_llm_call, ChatMessage, LlmCallRecord};

/// 会话文档对规划器的投影（方案B：第三段 review_summary → owner_feedback，内容中性化）。
/// 磁盘上 state.json 的会话文档保持原名 review_summary（审计真源不变），只改喂给
/// 规划器的投影。
#[derive(serde::Serialize)]
pub(crate) struct SessionDocProjection {
    key_file_paths: Vec<String>,
    key_conclusions: Vec<String>,
    #[serde(rename = "owner_feedback")]
    review_summary: Vec<String>,
}

/// 把会话文档投影为规划器可见形态：第三段改名 owner_feedback，且任一条目含禁词
/// 时回退中性模板。
pub(crate) fn project_session_doc(doc: &SessionDoc) -> SessionDocProjection {
    let mut review_summary = doc.review_summary.clone();
    sanitize_review_summary(&mut review_summary);
    SessionDocProjection {
        key_file_paths: doc.key_file_paths.clone(),
        key_conclusions: doc.key_conclusions.clone(),
        review_summary,
    }
}

/// 规划器建图 schema 提示词（唯一真源）：converse 的 system prompt 基础段。
/// 宿主 pi（与离线审计面）按这份规则产建图指令序列。
pub(crate) const CONVERSE_SYSTEM_PROMPT: &str = r#"你是治理系统的规划器，是对话 agent。把属主需求拆成一个任务 DAG（每个节点 = 契约 + 沙箱档案）。你只与属主对话。

你的输入：会话文档（记忆）+ 属主本轮消息。
输出二选一：
- 当属主消息是要制定/修改计划（或你判断需要产出建图指令）时：输出建图指令序列（JSON 数组）。每条指令是：
- {"op":"begin","request_id":"<需求id>"}
- {"op":"add_node","id":"task-1","summary":"<一句话摘要>","contract":{"prompt":"<任务描述>","acceptance_criteria":"<验收标准>"},"sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}}
- {"op":"add_edge","from":"...","to":"..."}
- {"op":"set_routes","start":["task-1"]}
- {"op":"commit"}
- 当属主消息是对话性提问/澄清/讨论、暂不需要改计划时：用自然语言直接答复属主，不输出建图指令 JSON。

规则：
- 建图指令 JSON 与自然语言答复只能二选一，不要混合；产建图指令时只输出 JSON 数组，不要任何多余文字。
- begin 必须最先，commit 必须最后，且至少一个节点。
- 节点粒度 = 一个可独立验收的工作单元（有自己的任务描述与验收标准）。需求包含多个不同验收物或多阶段产物（如先整理数据、再基于数据写报告），或后一部分必须以前一部分的产物为基础时，拆成多节点；单一交付物内部的步骤不要拆（如"写一个 hello.txt"是一个节点，不要拆成"起草内容"+"写入文件"两个节点）。
- 多节点之间的依赖用 add_edge 声明：from 是 to 的前置，from 完成后 to 才开工。add_edge 的 from/to 必须引用已 add_node 声明的节点 id；不允许成环（a 依赖 b、b 又依赖 a）、不允许重复声明同一依赖；相互没有依赖的节点不连线。
- 后继节点的任务描述按"前置产物已就绪"来写（如"基于 task-1 产出的 notes.md 写报告"），不要重复前置节点要做的工作。
- 每个节点的 contract.prompt 与 acceptance_criteria 必须非空。
- 每个节点必须声明非空 workspace_subdirs（sandbox.workspace_subdirs，工作区子目录列表，如 ["src"]）：声明的是该节点可见/可写的工作区范围（节点只能看到这些子目录），这是强制约束；空/缺省声明 = 计划不合格。挂载语义：首个子目录挂为该节点工作区根 /workspace，其余子目录挂为 /workspace/<子目录>。
- workspace_subdirs 必须声明具体子目录名：按任务产物位置声明（如任务写 src/ 下则声明 ["src"]）；禁止声明 "."（工作区根，挂载语义下根由系统接管，声明子目录必须是具体相对目录）；禁止声明与挂载根同名的目录名（如 "workspace"，避免嵌套歧义）；任务描述（contract.prompt）里"根目录"措辞应与声明的子目录一致（首个子目录即该节点工作区根 /workspace）。
- 契约 prompt 的产物路径措辞必须按执行者视角自锚定（执行者只看得到挂载结果，看不到宿主 ws 布局）：产物落在首个子目录（即执行者的 /workspace 根）时，表述为"在 /workspace 根下创建 <文件>"，或"在 <首子目录> 下创建"并附明确落点（如"在 src 下创建 hello.txt，落点 /workspace/hello.txt"）；禁止会产生 /workspace/<首子目录>/<文件> 之类多嵌套一层的歧义表述；产物落在其余子目录时写 /workspace/<子目录>/<文件>。
- 默认用缺省沙箱（volumes 空、runtime null、packages 空、network false），workspace_subdirs 按上条必须非空；除非任务确实需要，才声明额外权限。
- 任务需要读工作区之外的宿主参考材料（如设计文档、规格说明）时：声明只读参考卷 volumes（sandbox.volumes 数组），每项 {"host_path":"<宿主项目内路径>","container_path":"<容器内挂载点>","mode":"ro"}——host_path 必须是本会话可见的宿主项目内**绝对路径**且真实存在（容器以 ro 挂载，执行者只读）；container_path 用独立路径（建议 /references 或 /references/<名>），不得用 /workspace、/tmp/.agt 及其子路径。参考材料必须经 volumes 挂载进执行容器，契约 prompt 按 container_path 措辞（如"阅读 /references 下的设计文档"）——不要假设执行者能看到宿主任意路径，也不要把参考材料措辞成本地路径。runtime/packages/network 仍保持缺省（执行驱动不支持，声明了会被拒绝）。
- 声明参考卷前先用工具探查宿主材料体积（如 wc -c / du -sk <宿主路径>）；材料 MB 级（>5MB）时：①add_node 声明足额 time_limit_secs（可选整数秒字段，节点执行时间上限；1800 起步，每多 10MB 再加 600，向上取整——未声明时系统缺省仅 600 秒，大材料分块提炼必超）；②contract.prompt 必须明确提示执行者：参考卷超出上下文容量，用 head/tail/jq/node 等工具分块提炼所需信息，勿尝试通读。
- 计划必须忠实反映属主需求，不要做属主没要求的事。
- 答复属主时用自然语言直接、清晰，不要夹带建图指令。"#;

/// 合成 converse 的 system prompt（基础建图 schema + codux 注入的项目上下文）。
///
/// codux wrapper 每轮注入 `--append-system-prompt <memory>`（项目上下文）；P2-1
/// 方案 A 真正透传：追加到 planner pi 的 system prompt——宿主驱动
/// （`host::run_converse_on_host`）与 llm-calls 审计记录
/// （`build_messages`）共用同一份。空注入 = 原样返回基础 schema（行为不变）。
pub(crate) fn converse_system_prompt(append_system_prompt: &str) -> String {
    let append = append_system_prompt.trim();
    if append.is_empty() {
        CONVERSE_SYSTEM_PROMPT.to_string()
    } else {
        format!(
            "{}\n\n附加的项目上下文（codux 注入）：\n{}",
            CONVERSE_SYSTEM_PROMPT, append
        )
    }
}

/// converse 选项。
pub struct ConverseOptions {
    pub run_dir: PathBuf,
    pub model: ExecutorModel,
    /// 宿主 pi 化：planner 宿主驱动选项（cwd=项目根、AGT、pi-config 派生面）。
    pub host: crate::host::PlannerHostOptions,
    /// codux wrapper 注入的项目上下文（`--append-system-prompt`，经
    /// `ALFRED_APPEND_SYSTEM_PROMPT` 读入）；追加到 planner pi 的 converse
    /// system prompt（P2-1：内存注入端到端生效）。空串 = 不追加。
    pub append_system_prompt: String,
}

impl ConverseOptions {
    pub fn new(run_dir: PathBuf, model: ExecutorModel) -> Self {
        Self {
            host: crate::host::PlannerHostOptions {
                run_dir: run_dir.clone(),
                project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                time_limit_secs: 600,
                agt: alfred_executor::agt::resolve_agt_source(),
            },
            run_dir,
            model,
            append_system_prompt: String::new(),
        }
    }
}

/// converse 结果（§2.4 两分支：建图指令 或 给属主的答复）。
#[derive(Debug, Clone)]
pub enum ConverseOutcome {
    /// 建图指令序列 → DagSpec（交编排器接管：结构检查 → 计划审查 → 执行）。
    Instructions {
        dagspec: DagSpec,
        /// llm-calls/ 记录文件路径（P9 证据；e2e 从记录断言）。
        record_path: PathBuf,
    },
    /// 纯文本答复（给属主；不强制产 DagSpec——对话继续，多轮轮次可再喂入）。
    Reply {
        reply: String,
        /// llm-calls/ 记录文件路径（P9 证据；e2e 从记录断言）。
        record_path: PathBuf,
    },
}

/// 对话 agent：会话文档 + 属主消息 → §2.4 两分支。
///
/// 每轮都是短会话、无状态（喂会话文档 + 本轮属主消息）。多轮对话 = 属主消息
/// 轮次可多次喂入：对话产出建图指令（`Instructions`）后交编排器接管；否则返回
/// `Reply`（对话继续，下一轮再喂入）。
pub fn converse(
    opts: &ConverseOptions,
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
) -> Result<ConverseOutcome> {
    let messages = build_messages(request, doc, owner_message, &opts.append_system_prompt);
    let (outcome, response, offline, transport) = if std::env::var("ALFRED_OFFLINE").as_deref()
        == Ok("1")
        || std::env::var("ALFRED_PLANNER_OFFLINE").as_deref() == Ok("1")
    {
        // 离线模式保留：不经容器/宿主 pi（确定性直通）；两分支由注入文件二选一。
        let (outcome, response) = converse_offline(request)?;
        (outcome, response, true, "offline")
    } else {
        // 宿主 pi 化：宿主 pi 读 stdin prompt 跑 converse（模型经 run 级
        // models.json 单源投影），宿主读 outputs 产出（instructions.json |
        // reply.txt，收割强制恰好一个）。
        let out = crate::host::run_converse_on_host(
            &opts.host,
            &opts.model,
            request,
            doc,
            owner_message,
            &opts.append_system_prompt,
        )?;
        let outcome = match out.produced_file.as_str() {
            crate::host::CONVERSE_OUTPUT_FILE => {
                let dagspec = instructions_to_dagspec(&out.output_text, request)?;
                ConverseOutcome::Instructions {
                    dagspec,
                    record_path: PathBuf::new(),
                }
            }
            crate::host::CONVERSE_REPLY_FILE => ConverseOutcome::Reply {
                reply: out.output_text.trim().to_string(),
                record_path: PathBuf::new(),
            },
            other => bail!("converse host pi produced unexpected output file {other}"),
        };
        (outcome, out.output_text, false, "host_pi")
    };

    let record = LlmCallRecord {
        ts: alfred_core::util::now_rfc3339(),
        role: "converse".into(),
        model: opts.model.inspect_model_id(),
        offline,
        transport: transport.to_string(),
        messages,
        response,
        ok: true,
        error: None,
    };
    let record_path = log_llm_call(&opts.run_dir, &record)?;
    Ok(match outcome {
        ConverseOutcome::Instructions { dagspec, .. } => ConverseOutcome::Instructions {
            dagspec,
            record_path,
        },
        ConverseOutcome::Reply { reply, .. } => ConverseOutcome::Reply { reply, record_path },
    })
}

/// 构建提示词（会话文档 + 属主本轮消息 + codux 注入的项目上下文）。
pub fn build_messages(
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
    append_system_prompt: &str,
) -> Vec<ChatMessage> {
    // 方案B：喂给规划器的是投影（第三段 owner_feedback + 内容中性化），磁盘真源不变。
    let session = serde_json::to_string_pretty(&project_session_doc(doc)).unwrap_or_default();
    let system = converse_system_prompt(append_system_prompt);
    let user = format!(
        "需求 id：{}\n\n会话文档（记忆）：\n{session}\n\n属主本轮消息：\n{owner_message}",
        request.id
    );
    vec![ChatMessage::system(system), ChatMessage::user(user)]
}

/// 解析 LLM 输出 → 指令序列 → GraphBuilder → DagSpec。
pub fn instructions_to_dagspec(text: &str, request: &OwnerRequest) -> Result<DagSpec> {
    let cleaned = strip_fences(text);
    let v: serde_json::Value = serde_json::from_str(&cleaned)
        .with_context(|| format!("build instructions not JSON: {cleaned}"))?;
    let insts: Vec<BuildInstruction> =
        serde_json::from_value(v).context("build instruction schema mismatch")?;
    if insts.is_empty() {
        bail!("build instruction sequence is empty");
    }
    let mut builder = GraphBuilder::new();
    for inst in insts {
        builder
            .apply(inst)
            .map_err(|e| anyhow::anyhow!("builder error: {e}"))?;
    }
    let mut dagspec = builder
        .build()
        .map_err(|e| anyhow::anyhow!("builder error: {e}"))?;
    validate_dagspec(&dagspec, request)?;
    // B：大文件感知兜底——契约 time_limit_secs 字段在此写入（planner 产
    // instructions.json 的 dagspec 构造处）。提示词指导 planner 主动写对，
    // 这里是 LLM 漏写时的确定性兜底（>5MB 必有足额预算 + 分块提炼提示）。
    apply_large_volume_budget(&mut dagspec);
    Ok(dagspec)
}

// ---- B：planner 大文件感知（大参考卷预算兜底的确定性真源） ----

/// 大参考卷判定阈值：节点声明的只读参考卷宿主材料总量 >5MB 即超上下文容量。
pub const LARGE_REF_VOLUME_BYTES: u64 = 5 * 1024 * 1024;
/// 大参考卷 time_limit_secs 下限基数（秒）。
const LARGE_REF_BASE_SECS: u32 = 1800;
/// 超出阈值后每开始一个 10MB 块追加的秒数。
const LARGE_REF_PER_10MB_SECS: u32 = 600;
/// 10MB（字节）。
const TEN_MB_BYTES: u64 = 10 * 1024 * 1024;
/// 执行者分块提炼提示查重标记（planner 已按指导词写对则不重复注入）。
const LARGE_VOLUME_HINT_MARKER: &str = "勿尝试通读";

/// 大参考卷预算兜底：逐节点看 `sandbox.volumes` 宿主材料总量，>5MB 时——
///
/// ① `time_limit_secs` 未达规模下限则抬到下限（`1800 + 600×ceil((总量-5MB)/10MB)`；
///    已声明更高值不动——planner/属主的显式声明优先）；
/// ② `contract.prompt` 缺分块提炼提示则补系统提示（执行者勿通读，分块提炼）。
///
/// 根因对位：28.6MB omp 会话参考卷在 600s 硬死线内分块提炼不可能完成（三连
/// timed_out 机械重跑耗尽升级）——大材料任务必须在计划层就带足时间预算与
/// 正确的执行策略。
pub fn apply_large_volume_budget(dagspec: &mut DagSpec) {
    for node in &mut dagspec.nodes {
        let total = ref_volume_total_bytes(&node.sandbox.volumes);
        if total <= LARGE_REF_VOLUME_BYTES {
            continue;
        }
        let floor = scaled_time_limit_secs(total);
        if floor > node.time_limit_secs.unwrap_or(0) {
            node.time_limit_secs = Some(floor);
        }
        if !node.contract.prompt.contains(LARGE_VOLUME_HINT_MARKER) {
            node.contract
                .prompt
                .push_str(&large_volume_prompt_hint(total));
        }
    }
}

/// 节点只读参考卷宿主材料总量（字节；不存在/不可读路径计 0——放行校验在
/// executor validate_ref_volume，这里不重复拒绝）。
fn ref_volume_total_bytes(volumes: &[alfred_core::VolumeMount]) -> u64 {
    volumes.iter().map(|v| host_path_size(&v.host_path)).sum()
}

/// 宿主路径体积：文件 = 长度；目录 = 递归求和（symlink 跟随目标计实体）。
fn host_path_size(path: &str) -> u64 {
    match std::fs::metadata(Path::new(path)) {
        Ok(md) if md.is_file() => md.len(),
        Ok(md) if md.is_dir() => dir_size(Path::new(path)),
        _ => 0,
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(md) if md.is_dir() => total += dir_size(&entry.path()),
            Ok(md) => total += md.len(),
            Err(_) => {}
        }
    }
    total
}

/// 大参考卷 time_limit_secs 规模下限：1800 起步，超出 5MB 部分每开始一个
/// 10MB 块再 +600（向上取整——块内不欠账；28.6MB → 1800+600×3 = 3600）。
fn scaled_time_limit_secs(total_bytes: u64) -> u32 {
    let over = total_bytes.saturating_sub(LARGE_REF_VOLUME_BYTES);
    let blocks = (over + TEN_MB_BYTES - 1) / TEN_MB_BYTES;
    LARGE_REF_BASE_SECS + LARGE_REF_PER_10MB_SECS * blocks as u32
}

/// 执行者分块提炼系统提示（append 到 contract.prompt 尾部）。
fn large_volume_prompt_hint(total_bytes: u64) -> String {
    format!(
        "\n\n【系统提示（执行者必读）】本任务只读参考卷总量约 {:.1} MB，远超模型上下文容量——勿尝试通读；用 head/tail/jq/node 等工具分块检索、提炼所需信息。",
        total_bytes as f64 / 1_048_576.0
    )
}

/// 校验 DagSpec 与请求对齐（request_id 匹配、节点非空、依赖图完整）。
///
/// M2 多节点放开：不再限制单节点——依赖图完整性（悬空边/重复边/环）走
/// [`DagSpec::topological_order`] 单一真源检出。错误消息中性化（照
/// disguise.rs 范式）：不携带内部实体名与英文诊断词形——消息会进
/// planning_error_escalated 审计面，经属主转述给规划器也不泄漏治理词形。
fn validate_dagspec(dagspec: &DagSpec, request: &OwnerRequest) -> Result<()> {
    if dagspec.request_id != request.id {
        bail!(
            "计划所属的需求 id（{}）与当前需求（{}）不一致",
            dagspec.request_id,
            request.id
        );
    }
    if dagspec.nodes.is_empty() {
        bail!("计划里没有任何任务");
    }
    if let Err(e) = dagspec.topological_order() {
        bail!("{}", neutralize_graph_error(&e));
    }
    Ok(())
}

/// 依赖图诊断（`topological_order` 错误文案）→ 中性措辞。
///
/// 错误文案前缀/句式是 alfred-core M1 契约（entities.rs 逐条断言），按稳定
/// 前缀分类转写：去内部实体名（dagspec）与英文诊断词形，保留节点 id/路径
/// 细节（属主可读、可转述给规划器定位问题）。未识别的原文透传——宁可保持
/// 原样也不吞掉诊断信息（无静默出口）。
fn neutralize_graph_error(err: &str) -> String {
    let detail = err.strip_prefix("dagspec: ").unwrap_or(err);
    if let Some(path) = detail.strip_prefix("cycle detected: ") {
        return format!("计划里有些任务的先后关系成了环：{path}");
    }
    if let Some(edge) = detail.strip_prefix("duplicate edge ") {
        return format!("计划里同样的依赖 {edge} 声明了两次");
    }
    if let Some(rest) = detail.strip_prefix("edge ") {
        // "<from> -> <to>' references unknown node '<node>'"
        if let Some((pair, node)) = rest.split_once("' references unknown node '") {
            return format!(
                "计划里的依赖 {} 指向了不存在的任务 {}",
                pair.trim_matches('\''),
                node.trim_end_matches('\'')
            );
        }
    }
    err.to_string()
}

/// 离线确定性直通（`ALFRED_OFFLINE=1` 或 `ALFRED_PLANNER_OFFLINE=1`）：§2.4 两分支由注入文件二选一。
///
/// - `ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json | instructions.json>` → 建图
///   指令分支（validate → DagSpec）。DagSpec JSON（对象）直通；建图指令序列
///   （数组，与宿主 pi 产出同构）走 `instructions_to_dagspec` 同一条解析路径。
/// - `ALFRED_OFFLINE_REPLY_FILE=<reply.txt>` → 答复分支（纯文本）。
/// 两者同时/都不设 → 显式报错（不静默）。返回 (outcome, response 文本)，
/// response 供 llm-calls 记录（与宿主 pi 路径同构）。
fn converse_offline(request: &OwnerRequest) -> Result<(ConverseOutcome, String)> {
    let plan_file = std::env::var("ALFRED_OFFLINE_PLAN_FILE").ok();
    let reply_file = std::env::var("ALFRED_OFFLINE_REPLY_FILE").ok();
    match (plan_file, reply_file) {
        (Some(_), Some(_)) => bail!(
            "ALFRED_OFFLINE_PLAN_FILE 与 ALFRED_OFFLINE_REPLY_FILE 同时设置（二选一）"
        ),
        (Some(path), None) => {
            let plan = read_offline_plan(&path, request)?;
            let resp = serde_json::to_string_pretty(&plan).context("serialize offline plan")?;
            Ok((
                ConverseOutcome::Instructions {
                    dagspec: plan,
                    record_path: PathBuf::new(),
                },
                resp,
            ))
        }
        (None, Some(path)) => {
            let reply = std::fs::read_to_string(&path)
                .with_context(|| format!("read offline reply {}", path))?;
            let reply = reply.trim().to_string();
            if reply.is_empty() {
                bail!("ALFRED_OFFLINE_REPLY_FILE 为空");
            }
            Ok((
                ConverseOutcome::Reply {
                    reply: reply.clone(),
                    record_path: PathBuf::new(),
                },
                reply,
            ))
        }
        (None, None) => bail!(
            "ALFRED_OFFLINE/ALFRED_PLANNER_OFFLINE=1 requires ALFRED_OFFLINE_PLAN_FILE=<DagSpec.json|instructions.json> or ALFRED_OFFLINE_REPLY_FILE=<reply.txt>"
        ),
    }
}

/// 离线模式读注入的计划文件：DagSpec JSON（对象）或建图指令序列（数组）。
///
/// 数组形态与宿主 pi 产出的 instructions.json 同构，走同一条解析路径
/// （`instructions_to_dagspec` 单一真源——含 validate 与大参考卷兜底，与
/// 真跑路径完全同构）；对象形态是既有注入契约（终态 DagSpec 直通 +
/// validate）。两种形态都显式校验（无静默出口）。
fn read_offline_plan(path: &str, request: &OwnerRequest) -> Result<DagSpec> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read offline plan {}", path))?;
    if text.trim_start().starts_with('[') {
        return instructions_to_dagspec(&text, request)
            .with_context(|| format!("parse offline plan {}", path));
    }
    let plan: DagSpec =
        serde_json::from_str(&text).with_context(|| format!("parse offline plan {}", path))?;
    validate_dagspec(&plan, request)?;
    Ok(plan)
}

/// 剥 markdown 代码围栏 / 只取首个平衡 JSON 数组。
pub fn strip_fences(text: &str) -> String {
    let trimmed = text.trim();
    // 先找 ```json ... ``` 围栏块
    if let Some(start) = trimmed.find("```") {
        if let Some(rel) = trimmed[start..].find('[') {
            let abs = start + rel;
            if let Some(end) = trimmed.rfind(']') {
                if end > abs {
                    return trimmed[abs..=end].to_string();
                }
            }
        }
    }
    // 直接取首个 [ ... ] 平衡块
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            if end > start {
                return trimmed[start..=end].to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alfred_core::contract::Contract;
    use alfred_core::dagspec::{Edge, PlanNode};

    /// 测试参考材料目录：一次性建、drop 时整目录清理（含 >5MB 大文件）。
    struct RefFixture(PathBuf);

    impl RefFixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "alfred-large-ref-{tag}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, bytes: u64) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, vec![0u8; bytes as usize]).unwrap();
            p
        }
    }

    impl Drop for RefFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn owner_request() -> OwnerRequest {
        OwnerRequest {
            id: "req-1".into(),
            title: "t".into(),
            description: "d".into(),
            acceptance_criteria: "a".into(),
            created_at: "2026-09-08T00:00:00Z".into(),
        }
    }

    /// 构造带单参考卷的建图指令（instructions.json 形态）。
    fn instructions_with_volume(
        host_path: &str,
        time_limit_secs: Option<u32>,
        prompt_suffix: &str,
    ) -> String {
        let tl = time_limit_secs
            .map(|v| format!(",\"time_limit_secs\":{v}"))
            .unwrap_or_default();
        format!(
            r#"[{{"op":"begin","request_id":"req-1"}},
{{"op":"add_node","id":"task-1","summary":"提炼参考材料",
"contract":{{"prompt":"阅读 /references 参考材料并完成任务。{prompt_suffix}","acceptance_criteria":"a"}},
"sandbox":{{"volumes":[{{"host_path":"{host_path}","container_path":"/references","mode":"ro"}}],"workspace_subdirs":["src"]}}{tl}}},
{{"op":"commit"}}]"#
        )
    }

    #[test]
    fn large_volume_declares_floor_time_limit_and_hint() {
        // >5MB 触发：未声明 → 抬到规模下限（6MB：1800+600×1=2400）+ prompt 补提示。
        let fx = RefFixture::new("6mb");
        let big = fx.write("big.bin", 6 * 1024 * 1024);
        let text = instructions_with_volume(big.to_str().unwrap(), None, "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        let node = &dag.nodes[0];
        assert_eq!(node.time_limit_secs, Some(2400));
        assert!(node.contract.prompt.contains(LARGE_VOLUME_HINT_MARKER));
        assert!(node.contract.prompt.contains("head/tail/jq/node"));
    }

    #[test]
    fn large_volume_scales_with_size() {
        // 35MB：超出 30MB = 3 个整 10MB 块 → 1800+1800=3600；28.6MB 原始事故
        // 体积（超出 23.6MB → 3 块）同为 3600。
        let fx = RefFixture::new("scale");
        let big = fx.write("big.bin", 35 * 1024 * 1024);
        let text = instructions_with_volume(big.to_str().unwrap(), None, "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        assert_eq!(dag.nodes[0].time_limit_secs, Some(3600));

        let incident = fx.write("omp-session.json", 30_000_000); // ≈28.6MB
        let text = instructions_with_volume(incident.to_str().unwrap(), None, "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        assert_eq!(dag.nodes[0].time_limit_secs, Some(3600));
    }

    #[test]
    fn small_volume_untouched() {
        // ≤5MB 不触发：无声明保持 None、prompt 原样（无系统提示注入）。
        let fx = RefFixture::new("small");
        let small = fx.write("small.md", 1024);
        let text = instructions_with_volume(small.to_str().unwrap(), None, "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        assert_eq!(dag.nodes[0].time_limit_secs, None);
        assert_eq!(
            dag.nodes[0].contract.prompt,
            "阅读 /references 参考材料并完成任务。"
        );
    }

    #[test]
    fn declared_limit_higher_kept_lower_raised() {
        // 显式声明优先于下限（更高不动）；低于下限被抬（planner 漏算兜底）。
        let fx = RefFixture::new("declared");
        let big = fx.write("big.bin", 6 * 1024 * 1024);

        let text = instructions_with_volume(big.to_str().unwrap(), Some(7200), "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        assert_eq!(dag.nodes[0].time_limit_secs, Some(7200));

        let text = instructions_with_volume(big.to_str().unwrap(), Some(600), "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        assert_eq!(dag.nodes[0].time_limit_secs, Some(2400));
    }

    #[test]
    fn existing_hint_not_duplicated() {
        // planner 已按指导词写对提示（含查重标记）→ 系统不重复注入。
        let fx = RefFixture::new("dup");
        let big = fx.write("big.bin", 6 * 1024 * 1024);
        let text = instructions_with_volume(
            big.to_str().unwrap(),
            None,
            "参考卷超出上下文容量，用 head/tail/jq/node 分块提炼，勿尝试通读。",
        );
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        let prompt = &dag.nodes[0].contract.prompt;
        assert!(!prompt.contains("【系统提示"));
        assert_eq!(prompt.matches(LARGE_VOLUME_HINT_MARKER).count(), 1);
    }

    #[test]
    fn directory_volume_sums_files() {
        // 目录卷递归求和：3MB + 3MB = 6MB > 5MB → 触发（2400）。
        let fx = RefFixture::new("dir");
        let sub = fx.0.join("refs");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        std::fs::write(sub.join("b.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let text = instructions_with_volume(sub.to_str().unwrap(), None, "");
        let dag = instructions_to_dagspec(&text, &owner_request()).unwrap();
        assert_eq!(dag.nodes[0].time_limit_secs, Some(2400));
        assert!(dag.nodes[0].contract.prompt.contains(LARGE_VOLUME_HINT_MARKER));
    }

    // ---------- M2 多节点：validate 放开 + 依赖图校验（错误中性化） ----------

    /// 最小合法节点（契约非空 + 声明工作区子目录）。
    fn mn_node(id: &str) -> PlanNode {
        PlanNode::new(
            id,
            format!("{id} 摘要"),
            Contract {
                prompt: format!("做 {id}"),
                acceptance_criteria: format!("{id} 完成"),
                reviewer_models: vec![],
            },
        )
    }

    #[test]
    fn multinode_instructions_edges_and_topological_order() {
        // M2：多节点指令（add_node×2 + add_edge）放行；故意先声明后继再声明
        // 前置——dagspec 按依赖拓扑序重排（前置在前），边原样随图。
        let text = r#"[
{"op":"begin","request_id":"req-1"},
{"op":"add_node","id":"task-2","summary":"后继：基于前置产物写报告",
"contract":{"prompt":"基于 task-1 产出的 notes.md 写报告","acceptance_criteria":"报告覆盖要点"},
"sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}},
{"op":"add_node","id":"task-1","summary":"前置：整理要点",
"contract":{"prompt":"整理要点写入 notes.md","acceptance_criteria":"notes.md 存在"},
"sandbox":{"volumes":[],"runtime":null,"packages":[],"network":false,"workspace_subdirs":["src"]}},
{"op":"add_edge","from":"task-1","to":"task-2"},
{"op":"commit"}]"#;
        let dag = instructions_to_dagspec(text, &owner_request()).unwrap();
        assert_eq!(
            dag.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["task-1", "task-2"],
            "节点应按依赖拓扑序排列（前置在前，即使后声明）"
        );
        assert_eq!(
            dag.edges,
            vec![Edge {
                from: "task-1".into(),
                to: "task-2".into()
            }]
        );
    }

    #[test]
    fn offline_plan_file_accepts_both_forms_multinode() {
        // 离线注入两形态（M2）：对象 = 终态 DagSpec 直通放行；数组 = 建图指令
        // 序列走 instructions_to_dagspec 同一条解析路径。
        let dir = std::env::temp_dir().join(format!("alfred-offline-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let dag = DagSpec {
            request_id: "req-1".into(),
            nodes: vec![mn_node("task-1"), mn_node("task-2")],
            edges: vec![Edge {
                from: "task-1".into(),
                to: "task-2".into(),
            }],
        };
        let obj = dir.join("plan-object.json");
        std::fs::write(&obj, serde_json::to_string(&dag).unwrap()).unwrap();
        assert_eq!(
            read_offline_plan(obj.to_str().unwrap(), &owner_request()).unwrap(),
            dag,
            "对象形态：多节点 DagSpec 应直通放行（validate 不再限单节点）"
        );

        let inst = dir.join("instructions.json");
        std::fs::write(
            &inst,
            r#"[{"op":"begin","request_id":"req-1"},
{"op":"add_node","id":"task-1","summary":"前置","contract":{"prompt":"p","acceptance_criteria":"a"},"sandbox":{"workspace_subdirs":["src"]}},
{"op":"add_node","id":"task-2","summary":"后继","contract":{"prompt":"p","acceptance_criteria":"a"},"sandbox":{"workspace_subdirs":["src"]}},
{"op":"add_edge","from":"task-1","to":"task-2"},
{"op":"commit"}]"#,
        )
        .unwrap();
        let got = read_offline_plan(inst.to_str().unwrap(), &owner_request()).unwrap();
        assert_eq!(got.nodes.len(), 2);
        assert_eq!(
            got.edges,
            vec![Edge {
                from: "task-1".into(),
                to: "task-2".into()
            }]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_graph_errors_are_neutral() {
        // M2：依赖图校验错误中性化（照 disguise.rs 范式）——不携带内部实体名
        // 与英文诊断词形（经属主转述也不泄漏治理词形），保留节点 id 定位细节。
        let cases: Vec<(&str, DagSpec, &[&str])> = vec![
            (
                "环",
                DagSpec {
                    request_id: "req-1".into(),
                    nodes: vec![mn_node("task-1"), mn_node("task-2")],
                    edges: vec![
                        Edge {
                            from: "task-1".into(),
                            to: "task-2".into(),
                        },
                        Edge {
                            from: "task-2".into(),
                            to: "task-1".into(),
                        },
                    ],
                },
                &["先后关系成了环", "task-1", "task-2"],
            ),
            (
                "悬空边",
                DagSpec {
                    request_id: "req-1".into(),
                    nodes: vec![mn_node("task-1")],
                    edges: vec![Edge {
                        from: "task-1".into(),
                        to: "ghost".into(),
                    }],
                },
                &["指向了不存在的任务", "ghost"],
            ),
            (
                "重复边",
                DagSpec {
                    request_id: "req-1".into(),
                    nodes: vec![mn_node("task-1"), mn_node("task-2")],
                    edges: vec![
                        Edge {
                            from: "task-1".into(),
                            to: "task-2".into(),
                        },
                        Edge {
                            from: "task-1".into(),
                            to: "task-2".into(),
                        },
                    ],
                },
                &["声明了两次", "task-1 -> task-2"],
            ),
        ];
        for (name, dag, expected) in &cases {
            let err = validate_dagspec(dag, &owner_request())
                .unwrap_err()
                .to_string();
            for word in *expected {
                assert!(err.contains(word), "{name}: err = {err}");
            }
            for tech in [
                "dagspec",
                "cycle",
                "unknown node",
                "duplicate edge",
                "references",
            ] {
                assert!(!err.contains(tech), "{name}: err 含技术词 {tech}: {err}");
            }
        }
    }

    #[test]
    fn validate_alignment_errors_are_neutral() {
        // 对齐错误同样中性化：不携带内部实体名（dagspec/request.id 词形）。
        let wrong_id = DagSpec {
            request_id: "req-other".into(),
            nodes: vec![mn_node("task-1")],
            edges: vec![],
        };
        let err = validate_dagspec(&wrong_id, &owner_request())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("req-other") && err.contains("req-1"),
            "err = {err}"
        );
        assert!(!err.contains("dagspec"), "err = {err}");

        let empty = DagSpec {
            request_id: "req-1".into(),
            nodes: vec![],
            edges: vec![],
        };
        let err = validate_dagspec(&empty, &owner_request())
            .unwrap_err()
            .to_string();
        assert!(err.contains("没有任何任务"), "err = {err}");
    }

    #[test]
    fn converse_prompt_teaches_multinode_dependency() {
        // M2 prompt 契约：教 add_edge 依赖声明 + 节点粒度（何时拆/何时不拆）。
        // 防回归锚点（离线 e2e 绕过 prompt，真跑契约只能靠这里防守）。
        assert!(
            CONVERSE_SYSTEM_PROMPT.contains("add_edge"),
            "缺 add_edge 教学"
        );
        assert!(
            CONVERSE_SYSTEM_PROMPT.contains("from 完成后 to 才开工"),
            "缺依赖方向语义教学"
        );
        assert!(
            CONVERSE_SYSTEM_PROMPT.contains("可独立验收的工作单元"),
            "缺节点粒度教学"
        );
        assert!(
            CONVERSE_SYSTEM_PROMPT.contains("单一交付物内部的步骤不要拆"),
            "缺何时不拆的粒度指导"
        );
    }
}
