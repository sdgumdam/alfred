//! `alfred exec-review` 子命令（R6c）。
//!
//! 读取 OwnerRequest JSON + Contract JSON + ws 目录 → 独立 reviewer 容器
//! （挂 ws 全量 ro + 契约全字段 + 对话记录）判产物 vs 验收标准 → ExecVerdict
//! 落盘 run 目录（verdict.json + state.json）。
//!
//! 这是执行审查容器路径的独立入口（治理环内嵌 scorer 的容器化替代，R6d 接入）。

use std::path::PathBuf;

use anyhow::{Context, Result};
use alfred_core::conversation::{load_conversation, ConversationLog};
use alfred_core::contract::Contract;
use alfred_core::request::OwnerRequest;
use alfred_executor::config::{load_executor_model, load_reviewer_model};
use alfred_reviewer::exec_review::{default_exec_review_dir, execute_exec_review, ExecReviewOptions};
use alfred_reviewer::ReviewerContainerOptions;
use clap::Args;

#[derive(Args, Debug)]
pub struct ExecReviewArgs {
    /// OwnerRequest JSON 文件路径。
    #[arg(long)]
    pub request: PathBuf,

    /// Contract JSON 文件路径（prompt + acceptance_criteria）。
    #[arg(long)]
    pub contract: PathBuf,

    /// 执行者挂载语义：workspace_subdirs（逗号分隔，如 "output" 或 "src,tests"）；
    /// 首个子目录 = 执行者工作区根 /workspace，契约"根目录"按此翻译。缺省空。
    #[arg(long, default_value = "")]
    pub workspace_subdirs: String,

    /// ws 全量目录（执行者产物；挂载到容器 /workspace ro）。
    #[arg(long)]
    pub ws_dir: PathBuf,

    /// 运行目录（缺省自动创建于 $ALFRED_STATE_DIR 或 ~/.local/state/alfred/runs）。
    #[arg(long)]
    pub run_dir: Option<PathBuf>,

    /// owner↔planner 对话记录（conversation.json）；缺省读 run_dir 父目录。
    #[arg(long)]
    pub conversation: Option<PathBuf>,

    /// 单样本时间上限（秒）。
    #[arg(long, default_value_t = 300)]
    pub time_limit: u32,

    /// 沙箱镜像。
    #[arg(long, default_value = "alfred-executor:latest")]
    pub image: String,

    /// 关闭 `inspect ctl` 观测轮询。
    #[arg(long)]
    pub no_ctl: bool,
}

pub fn exec_review(args: ExecReviewArgs) -> Result<()> {
    let req_text = std::fs::read_to_string(&args.request)
        .with_context(|| format!("read request {}", args.request.display()))?;
    let request: OwnerRequest = serde_json::from_str(&req_text)
        .with_context(|| format!("parse OwnerRequest {}", args.request.display()))?;

    let contract_text = std::fs::read_to_string(&args.contract)
        .with_context(|| format!("read contract {}", args.contract.display()))?;
    let contract: Contract = serde_json::from_str(&contract_text)
        .with_context(|| format!("parse Contract {}", args.contract.display()))?;
    // R6f：执行者挂载语义——workspace_subdirs[0] 即执行者 /workspace 根（契约
    // "根目录"落点）。逗号分隔解析；空串 = 空声明（审查者按字面路径判）。
    let workspace_subdirs: Vec<String> = args
        .workspace_subdirs
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let reviewer = load_reviewer_model()?;
    if let Ok(executor) = load_executor_model() {
        if reviewer.provider == executor.provider {
            eprintln!(
                "[alfred] warn: reviewer 与 executor 同 provider '{}'——异构审查降级",
                reviewer.provider
            );
        }
    }

    let run_dir = args.run_dir.clone().unwrap_or_else(default_exec_review_dir);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;

    // 对话记录（可选）：显式路径优先，否则 run_dir 父目录（治理 run 目录）。
    let conversation: Option<ConversationLog> = match &args.conversation {
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("read conversation {}", p.display()))?;
            Some(serde_json::from_str(&text).with_context(|| format!("parse conversation {}", p.display()))?)
        }
        None => load_conversation(run_dir.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(anyhow::Error::msg)
            .ok()
            .flatten(),
    };

    let opts = ExecReviewOptions {
        container: ReviewerContainerOptions {
            run_dir: run_dir.clone(),
            image: args.image.clone(),
            port_base: 13100,
            time_limit_secs: args.time_limit,
            settle_grace_seconds: 20.0,
            ctl_enabled: !args.no_ctl,
            agt_dir: alfred_reviewer::container::resolve_agt_dir(),
        },
        ws_dir: args.ws_dir.clone(),
    };

    println!(
        "alfred exec-review: 产物 vs 验收标准（reviewer model: {}）",
        reviewer.inspect_model_id()
    );
    println!("  run_dir  : {}", run_dir.display());
    println!("  ws_dir   : {}", args.ws_dir.display());
    println!("  request  : {}", request.id);
    println!("  subdirs  : {:?}", workspace_subdirs);

    let outcome = execute_exec_review(&opts, &reviewer, &request, &contract, &workspace_subdirs, conversation.as_ref())?;

    println!();
    println!("eval status      : {}", outcome.eval_status);
    match &outcome.verdict {
        Some(v) => {
            println!(
                "exec verdict     : {:?} (failure_class={:?})",
                v.value, v.failure_class
            );
            println!("  rationale     : {}", truncate(&v.explanation, 300));
        }
        None => println!(
            "exec verdict     : unscored ({})",
            outcome.unscored_reason.as_deref().unwrap_or("none")
        ),
    }
    println!("verdict.json     : {}/verdict.json", run_dir.display());
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
