//! alfred CLI driver bin（codux 可调度的 run/feed/status）。
//!
//! 属主 08-31：删/回滚 alfred CLI 六命令（run/decide/status/plan-review/exec-review/
//! panel 不再作为 owner 交互入口）；alfred 保留编排器状态机为库，并出真实 `alfred`
//! bin 作为 codux 终端 wrapper 可调度的 CLI driver（照 omp.rs 范式，不发明面板）。
//! owner 在 codux 终端与 pi 对话；wrapper 调度本 bin 驱动治理环。
//!
//! 子命令：
//! - `chat`   [--run-dir <dir>]
//!     owner 持续会话入口（REPL，codux 调度的常驻会话进程）：需求收集态（两行
//!     确定性转写 OwnerRequest → 建 run → 提交 planner）/ Planning 对话路由
//!     （[pi] 答复）/ 挂起升级包呈现与决策精确解析（"重试"/"放弃"/其余 Revise）/
//!     [orchestrator] 自主流转状态行 / 断点恢复（state.json）。详见 chat.rs。
//! - `run`    --request <request.json> --run-dir <dir>
//!     初始化 GovernanceRun（request.submit 首轮 + ws 基线）→ `run_governance_loop`
//!     推进到挂起/终态。
//! - `feed`   --run-dir <dir> --decision revise|retry|abandon
//!             [--message <文本|文件路径>]
//!     喂属主消息 → `feed_owner_message`（续跑治理环，返回新状态 + 规划器答复给调用方显示）。
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
    audit, build_governance_context, default_governance_dir, feed_owner_message,
    init_governance_run, load_governance_run, persist_governance_run, run_governance_loop,
    state_label,
};
use alfred_core::governance::{GovernanceAblation, GovernanceOptions, OwnerDecision};
use alfred_core::request::OwnerRequest;
use anyhow::{bail, Context, Result};

mod chat;
mod chat_tui;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = strip_append_system_prompt(args);
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "run" => cmd_run(&args[1..]),
        "feed" => cmd_feed(&args[1..]),
        "status" => cmd_status(&args[1..]),
        "chat" => chat::cmd_chat(&args[1..]),
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "-V" | "--version" => {
            println!("alfred {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("alfred: 未知子命令 {other:?}（run|feed|status|chat；--help 查看用法）"),
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
    println!(
        "alfred {} — codux 可调度的治理环 CLI driver",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("用法: alfred [--append-system-prompt <value>] <子命令> [参数]");
    println!();
    println!("子命令:");
    println!(
        "  chat    owner 持续会话入口（REPL：需求收集/对话/拍板/断点恢复；[--run-dir <dir>]）"
    );
    println!("  feed    喂属主决策（revise|retry|abandon）并从挂起态续跑");
    println!("  run     初始化治理环（request → 规划 → 计划审查 → 执行 → 执行审查 → 路由；[--ablation a1|a2|a3] 方案A消融档位，缺省完整链；[--env-compose <绝对路径>] [--env-metadata <JSON文件>] 任务真实环境接线（原任务 compose + SAMPLE_METADATA 插值键，缺省内置 network none 单容器））");
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
    let mut ablation: Option<GovernanceAblation> = None;
    let mut env_compose: Option<String> = None;
    let mut env_metadata: Option<std::collections::BTreeMap<String, String>> = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--request"
            | "--run-dir"
            | "--time-limit"
            | "--review-time-limit"
            | "--planner-time-limit"
            | "--image"
            | "--ablation"
            | "--env-compose"
            | "--env-metadata" => {
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
                    "--ablation" => {
                        ablation = Some(parse_ablation(&val)?);
                    }
                    "--env-compose" => env_compose = Some(val),
                    "--env-metadata" => {
                        let path = PathBuf::from(&val);
                        let text = std::fs::read_to_string(&path).with_context(|| {
                            format!("alfred run: --env-metadata 读取失败 {}", path.display())
                        })?;
                        env_metadata = Some(
                            serde_json::from_str(&text).with_context(|| {
                                format!(
                                    "alfred run: --env-metadata 需为 JSON 对象(字符串键值) {}",
                                    path.display()
                                )
                            })?,
                        );
                    }
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

    // 模型配置 + run 目录初始化走共享真源（init_governance_run /
    // build_governance_context，chat 需求收集同路径，不复制第二份）。
    // 任务环境接线（G1 native_inspect 真实环境）：--env-compose = 原任务
    // compose 绝对路径；--env-metadata = 其 `${SAMPLE_METADATA_*}` 插值键
    // (JSON 文件)。值进 GovernanceOptions → state.json（续跑/孤儿恢复绑定
    // 同一环境）。校验：metadata 无 compose = 接线错误（fail-closed）；
    // compose 必须绝对路径（相对路径经 docker 静默变 named volume）。
    let env_compose = env_compose
        .map(|p| {
            let path = PathBuf::from(&p);
            if !path.is_absolute() {
                bail!(
                    "alfred run: --env-compose 必须为绝对路径（相对路径经 docker 静默变 named volume）: {p}"
                );
            }
            if !path.is_file() {
                bail!("alfred run: --env-compose 文件不存在: {p}");
            }
            Ok(p)
        })
        .transpose()?;
    let env_metadata = match (env_metadata, env_compose.as_ref()) {
        (Some(meta), Some(_)) => meta,
        (Some(_), None) => {
            bail!("alfred run: --env-metadata 只能与 --env-compose 同用（无任务 compose 的插值键无处解析）");
        }
        (None, _) => std::collections::BTreeMap::new(),
    };
    let options = GovernanceOptions {
        image,
        exec_time_limit_secs: time_limit,
        review_time_limit_secs: review_time_limit,
        planner_time_limit_secs: planner_time_limit,
        port_base: 13100,
        settle_grace_seconds: 20.0,
        ctl_enabled: !no_ctl,
        ablation,
        env_compose,
        env_metadata,
    };
    let run_dir = run_dir.unwrap_or_else(default_governance_dir);
    let mut run = init_governance_run(&run_dir, request, options)?;
    // 任务环境绑定进原生审计轨迹（run 身份绑定：续跑/孤儿恢复从 state.json
    // 读同一环境；audit 只记非秘密引用——插值键名与 compose 路径）。
    if let Some(compose) = &run.options.env_compose {
        audit(
            &run_dir,
            "task_environment_bound",
            &serde_json::json!({
                "env_compose": compose,
                "env_metadata_keys": run.options.env_metadata.keys().collect::<Vec<_>>(),
            }),
        )?;
    }
    if let Some(abl) = run.options.ablation {
        // 消融档位进原生审计轨迹（run 身份绑定；A3 语义下 audit 照写的组成部分）。
        audit(
            &run_dir,
            "governance_ablation",
            &serde_json::json!({
                "arm": format!("{:?}", abl),
                "ablation": ablation_label(abl),
            }),
        )?;
    }
    let ctx = build_governance_context(&run_dir)?;
    let pending_reply = run_governance_loop(&mut run, &ctx)?;
    persist_governance_run(&run_dir, &run)?;
    audit(
        &run_dir,
        "governance_paused",
        &serde_json::json!({ "state": state_label(run.state()) }),
    )?;
    if let Some(reply) = &pending_reply {
        // P2-2：规划器答复 surface 给 owner 终端（Reply 分支不产计划，对话继续）。
        println!("[pi] {reply}");
        println!(
            "[orchestrator] 规划器已答复属主（state=Planning，对话继续）。run_dir: {}；等待属主界面喂入下一轮消息。",
            run_dir.display()
        );
    }
    println!(
        "[driver] 当前状态 : {}（attempts={}/{}）",
        state_label(run.state()),
        run.attempts_used,
        run.mechanical_budget
    );
    Ok(())
}

/// 方案A消融档位 CLI 解析（`--ablation a1|a2|a3`；不新增第四种）。
fn parse_ablation(value: &str) -> Result<GovernanceAblation> {
    match value {
        "a1" => Ok(GovernanceAblation::NoProcessEvidence),
        "a2" => Ok(GovernanceAblation::NoActiveVerification),
        "a3" => Ok(GovernanceAblation::AuditOnly),
        other => bail!("alfred run: --ablation 未知档位 {other:?}（a1|a2|a3；缺省 = 完整治理链）"),
    }
}

/// 消融档位短标签（审计/记录用）。
fn ablation_label(abl: GovernanceAblation) -> &'static str {
    match abl {
        GovernanceAblation::NoProcessEvidence => "a1",
        GovernanceAblation::NoActiveVerification => "a2",
        GovernanceAblation::AuditOnly => "a3",
    }
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
    let ctx = build_governance_context(&run_dir)?;

    let outcome = feed_owner_message(&mut run, &ctx, &message, decision)?;
    if let Some(reply) = &outcome.reply {
        // P2-2：规划器答复 surface 给 owner 终端（Planning 态续入对话后 planner 再答复）。
        println!("[pi] {reply}");
        println!(
            "[orchestrator] 规划器已答复属主（state=Planning，对话继续）。run_dir: {}；等待属主界面喂入下一轮消息。",
            run_dir.display()
        );
    }
    println!(
        "[driver] 当前状态 : {}（attempts={}/{}）",
        state_label(outcome.state),
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
