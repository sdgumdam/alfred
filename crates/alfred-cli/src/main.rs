//! alfred CLI driver bin（codux 可调度的 run/feed/status）。
//!
//! 属主 08-31：删/回滚 alfred CLI 六命令（run/decide/status/plan-review/exec-review/
//! panel 不再作为 owner 交互入口）；alfred 保留编排器状态机为库，并出真实 `alfred`
//! bin 作为 codux 终端 wrapper 可调度的 CLI driver（照 omp.rs 范式，不发明面板）。
//! owner 在 codux 终端与 pi 对话；wrapper 调度本 bin 驱动治理环。
//!
//! 子命令：
//! - `run`    --request <request.json> --run-dir <dir>
//!             [--time-limit N] [--review-time-limit N] [--planner-time-limit N]
//!             [--image IMG] [--no-ctl]
//!     初始化 GovernanceRun（request.submit 首轮 + ws 基线）→ `run_governance_loop`
//!     推进到挂起/终态。
//! - `feed`   --run-dir <dir> --decision revise|retry|abandon
//!             [--message <文本|文件路径>]
//!     喂属主消息 → `feed_owner_message`（续跑治理环，返回新状态给调用方显示）。
//! - `status` --run-dir <dir>
//!     打印当前状态（state/attempts/owner_message）。
//!
//! `--message` 优先按文件路径读取（旧 decide --message 语义）；路径不存在时按
//! 内联文本处理（codux 终端直喂属主原话）。
//!
//! 前置消费：codux wrapper 在子命令前注入 `--append-system-prompt <value>`（项目
//! 上下文）；main() 取子命令前先剥离任意前置该 flag 并存入 `ALFRED_APPEND_SYSTEM_PROMPT`
//! env（绝不 bail）。`cmd_run`/`cmd_feed` 读该 env → `GovernanceContext` → 追加到
//! planner pi 的 converse system prompt（容器 + llm-calls 记录，见
//! alfred-planner::converse 的 `converse_system_prompt`）。P2-1：内存注入端到端生效。
//! `--help/-h` 与 `--version/-V` 打印后退出 0。输出保持 `[driver] 当前状态` 状态行。

use std::path::{Path, PathBuf};

use alfred_cli::governance::{
    audit, default_governance_dir, feed_owner_message, load_governance_run,
    persist_governance_run, run_governance_loop, state_label, GovernanceContext,
};
use alfred_core::conversation::{append_to_disk, ConversationRole, ConversationSource};
use alfred_core::governance::{GovernanceOptions, GovernanceRun, OwnerDecision};
use alfred_core::request::OwnerRequest;
use alfred_executor::config::{load_executor_model, load_planner_model, load_reviewer_model};
use alfred_executor::run::ensure_run_workspace;
use anyhow::{bail, Context, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = strip_append_system_prompt(args);
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "run" => cmd_run(&args[1..]),
        "feed" => cmd_feed(&args[1..]),
        "status" => cmd_status(&args[1..]),
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "-V" | "--version" => {
            println!("alfred {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("alfred: 未知子命令 {other:?}（run|feed|status；--help 查看用法）"),
    }
}

/// 消费任意前置的 `--append-system-prompt <value>`（codux wrapper 前置注入）。
/// 存入 `ALFRED_APPEND_SYSTEM_PROMPT` env（cmd_run/cmd_feed 读入
/// `GovernanceContext`，追加到 planner pi 的 system prompt），绝不 bail——
/// 值缺失时仅消费 flag 本身继续。
fn strip_append_system_prompt(mut args: Vec<String>) -> Vec<String> {
    let mut appended: Vec<String> = Vec::new();
    while let Some(first) = args.first().map(String::as_str) {
        if first != "--append-system-prompt" {
            break;
        }
        args.remove(0);
        if let Some(value) = args.first().cloned() {
            appended.push(value);
            args.remove(0);
        }
    }
    if !appended.is_empty() {
        std::env::set_var("ALFRED_APPEND_SYSTEM_PROMPT", appended.join("\n"));
    }
    args
}

fn print_help() {
    println!("alfred {} — codux 可调度的治理环 CLI driver", env!("CARGO_PKG_VERSION"));
    println!();
    println!("用法: alfred [--append-system-prompt <value>] <子命令> [参数]");
    println!();
    println!("子命令:");
    println!("  run     初始化治理环（request → 规划 → 计划审查 → 执行 → 执行审查 → 路由）");
    println!("  feed    喂属主决策（revise|retry|abandon）并从挂起态续跑");
    println!("  status  只读打印当前治理环状态");
    println!();
    println!("通用 flag: --append-system-prompt <value>（前置注入，追加到 planner pi 系统提示）; -h/--help; -V/--version");
}

/// `run`：初始化治理 run（request.submit 首轮 + ws 基线）→ 推进治理环。
fn cmd_run(args: &[String]) -> Result<()> {
    let mut request_path: Option<PathBuf> = None;
    let mut run_dir: Option<PathBuf> = None;
    let mut time_limit: u32 = 600;
    let mut review_time_limit: u32 = 300;
    let mut planner_time_limit: u32 = 600;
    let mut image = "alfred-executor:latest".to_string();
    let mut no_ctl = false;

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--request" | "--run-dir" | "--time-limit" | "--review-time-limit"
            | "--planner-time-limit" | "--image" => {
                i += 1;
                let val = args
                    .get(i)
                    .with_context(|| format!("alfred run: {flag} 缺值"))?
                    .clone();
                match flag {
                    "--request" => request_path = Some(PathBuf::from(val)),
                    "--run-dir" => run_dir = Some(PathBuf::from(val)),
                    "--time-limit" => time_limit = val.parse().context("--time-limit 非数字")?,
                    "--review-time-limit" => {
                        review_time_limit = val.parse().context("--review-time-limit 非数字")?
                    }
                    "--planner-time-limit" => {
                        planner_time_limit = val.parse().context("--planner-time-limit 非数字")?
                    }
                    "--image" => image = val,
                    _ => unreachable!(),
                }
            }
            "--no-ctl" => no_ctl = true,
            other => bail!("alfred run: 未知参数 {other:?}"),
        }
        i += 1;
    }
    let request_path = request_path.context("alfred run: 需要 --request <request.json>")?;

    let text = std::fs::read_to_string(&request_path)
        .with_context(|| format!("read request {}", request_path.display()))?;
    let request: OwnerRequest = serde_json::from_str(&text)
        .with_context(|| format!("parse OwnerRequest {}", request_path.display()))?;

    let planner = load_planner_model()?;
    let executor = load_executor_model()?;
    let reviewer = load_reviewer_model()?;

    let run_dir = run_dir.unwrap_or_else(default_governance_dir);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("create run dir {}", run_dir.display()))?;
    // R6e：治理 run 初始化单一持久 ws（git init 基线快照）——三容器共享此 ws。
    ensure_run_workspace(&run_dir)?;
    std::fs::write(
        run_dir.join("request.json"),
        serde_json::to_string_pretty(&request).context("serialize OwnerRequest")?,
    )
    .context("write request.json")?;
    let run_id = run_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("run")
        .to_string();

    // R6a：对话记录（reviewer 挂载输入数据源，§二.8）——初始需求提交轮。
    append_to_disk(
        &run_dir,
        &run_id,
        ConversationRole::Owner,
        alfred_planner::format_request_message(&request),
        ConversationSource::RequestSubmit,
    )
    .map_err(anyhow::Error::msg)
    .context("append request.submit to conversation.json")?;

    let options = GovernanceOptions {
        image,
        exec_time_limit_secs: time_limit,
        review_time_limit_secs: review_time_limit,
        planner_time_limit_secs: planner_time_limit,
        port_base: 13100,
        settle_grace_seconds: 20.0,
        ctl_enabled: !no_ctl,
    };
    let mut run = GovernanceRun::new(run_id, request, options);
    let ctx = GovernanceContext {
        run_dir: run_dir.clone(),
        planner_model: planner,
        executor_model: executor,
        reviewer_model: reviewer,
        append_system_prompt: std::env::var("ALFRED_APPEND_SYSTEM_PROMPT").unwrap_or_default(),
    };

    audit(
        &run_dir,
        "governance_started",
        &serde_json::json!({ "request_id": run.request.id }),
    )?;
    run_governance_loop(&mut run, &ctx)?;
    persist_governance_run(&run_dir, &run)?;
    audit(
        &run_dir,
        "governance_paused",
        &serde_json::json!({ "state": state_label(run.state()) }),
    )?;
    println!(
        "[driver] 当前状态 : {}（attempts={}/{}）",
        state_label(run.state()),
        run.attempts_used,
        run.mechanical_budget
    );
    Ok(())
}

/// `feed`：喂属主消息 → 续跑治理环（`feed_owner_message` 库 API）。
fn cmd_feed(args: &[String]) -> Result<()> {
    let mut run_dir: Option<PathBuf> = None;
    let mut decision: Option<String> = None;
    let mut message: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--run-dir" | "--decision" | "--message" => {
                i += 1;
                let val = args
                    .get(i)
                    .with_context(|| format!("alfred feed: {flag} 缺值"))?
                    .clone();
                match flag {
                    "--run-dir" => run_dir = Some(PathBuf::from(val)),
                    "--decision" => decision = Some(val),
                    "--message" => message = Some(val),
                    _ => unreachable!(),
                }
            }
            other => bail!("alfred feed: 未知参数 {other:?}"),
        }
        i += 1;
    }
    let run_dir = run_dir.context("alfred feed: 需要 --run-dir <dir>")?;
    let decision = decision.context("alfred feed: 需要 --decision revise|retry|abandon")?;
    let decision = match decision.as_str() {
        "revise" => OwnerDecision::Revise,
        "retry" => OwnerDecision::Retry,
        "abandon" => OwnerDecision::Abandon,
        other => bail!("alfred feed: 未知决策 {other:?}（revise|retry|abandon）"),
    };
    // --message 优先按文件路径读取（旧 decide 语义）；路径不存在按内联文本。
    // P2b：Retry/Abandon 消息可选（Abandon 不需要消息；Retry 可不带新指令重跑）；Revise 必填。
    let message = match &message {
        Some(m) if Path::new(m).is_file() => {
            std::fs::read_to_string(m).with_context(|| format!("read message {}", m))?
        }
        Some(m) => m.clone(),
        None => match decision {
            OwnerDecision::Revise => {
                bail!("alfred feed: revise 决策需要 --message <文本|文件路径>")
            }
            OwnerDecision::Retry | OwnerDecision::Abandon => String::new(),
        },
    };

    let mut run = load_governance_run(&run_dir)?;
    let planner = load_planner_model()?;
    let executor = load_executor_model()?;
    let reviewer = load_reviewer_model()?;
    let ctx = GovernanceContext {
        run_dir: run_dir.clone(),
        planner_model: planner,
        executor_model: executor,
        reviewer_model: reviewer,
        append_system_prompt: std::env::var("ALFRED_APPEND_SYSTEM_PROMPT").unwrap_or_default(),
    };

    let state = feed_owner_message(&mut run, &ctx, &message, decision)?;
    println!(
        "[driver] 当前状态 : {}（attempts={}/{}）",
        state_label(state),
        run.attempts_used,
        run.mechanical_budget
    );
    Ok(())
}

/// `status`：打印当前治理环状态（只读）。
fn cmd_status(args: &[String]) -> Result<()> {
    let mut run_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--run-dir" => {
                i += 1;
                run_dir = Some(PathBuf::from(
                    args.get(i)
                        .with_context(|| "alfred status: --run-dir 缺值")?
                        .clone(),
                ));
            }
            other => bail!("alfred status: 未知参数 {other:?}"),
        }
        i += 1;
    }
    let run_dir = run_dir.context("alfred status: 需要 --run-dir <dir>")?;
    let run = load_governance_run(&run_dir)?;
    println!(
        "[driver] 当前状态 : {}（attempts={}/{}）",
        state_label(run.state()),
        run.attempts_used,
        run.mechanical_budget
    );
    if let Some(msg) = &run.owner_message {
        println!("[driver] owner_message : {msg}");
    }
    Ok(())
}
