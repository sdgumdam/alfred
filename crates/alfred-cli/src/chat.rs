//! alfred chat —— owner 持续会话入口（编排器 owner 接口层）。
//!
//! 形态（架构锚点 `.plans/chat架构-orchestrator接口层.md`）：codux 调度 `alfred
//! chat` 常驻会话进程 = 协调层的 owner 接口。本模块是**确定性循环壳**——不发明
//! 治理机制、不发明面板（纯终端文本）：读一行，按当前 run 状态确定性路由到既有
//! 库入口（`run_governance_loop` / `feed_owner_message` / `init_governance_run`）；
//! LLM 只在被治理容器里（planner/reviewer pi）。
//!
//! - **需求收集态**（无 run / 终态后新需求）：一行需求 → OwnerRequest（title=
//!   首行 ≤40 字、`chars()` 截断防中文 panic；description=全文；acceptance_
//!   criteria 默认=需求原文——不追问验收标准，pi 对话中需要澄清自然会问）→
//!   `init_governance_run` 建 run（与 cmd_run 单一初始化真源）→ 提交 planner。
//!   转写呈现极简：一行"已受理"（id/title/criteria 不再四行块铺陈）。
//! - **Planning 态**（pi 答复后停驻）：整行=="放弃" → Abandon（P2a 转移表支持，
//!   属主放弃恒可选）；其余一律 Revise + 整行作 owner 消息续入对话 → `[pi]` 答复。
//! - **挂起态**（escalated/plan_rejected）：升级包呈现极简（2-3 行：run/态 +
//!   审查/升级意见 + "回复：重试 / 放弃 / 或直接说修改意见"；按态取数：
//!   PlanRejected→plan_verdicts.last()；Escalated→exec_verdicts.last()+
//!   escalation_source；缺 verdict（unscored 升级）读 audit.jsonl 最近升级事件；
//!   owner 全可见用原始 verdict——禁词净化是 planner 侧投影不适用此处）→
//!   决策**整行 trim 精确匹配**：=="重试"→Retry、=="放弃"→Abandon、其余一律
//!   Revise+整行作 owner 消息（最安全分支：进 planner 它会追问澄清；禁子串
//!   包含——"不要重试"误路由 Retry 是不可逆误动作）。
//! - **终态**（completed/abandoned）：呈现结果 + "新需求请直接说 / Ctrl-D 退出"
//!   → 回需求收集态。
//! - **断点恢复**：run 从 state.json 恢复（P3 每转移 persist）；流转中间态
//!   （plan_reviewing/executing/exec_reviewing）经 `run_governance_loop` 从断点
//!   续跑——执行审查输入（契约/挂载语义/ws/对话记录）全在磁盘，exec_reviewing
//!   照常磁盘重入（9/3 欠账修复，废"执行 outcome 不落盘无法续跑"死锁）；审查
//!   失败走治理降级（escalated 挂起属主拍板）。
//! - **错误处理（P3）**：feed/loop 的 Err catch + 打印 + 从 state.json reload 续
//!   REPL（不退进程）；reload 失败（state.json 不可读）才退出。
//! - **计划摘要呈现（P2）**：pi 建图后 owner 侧呈现计划摘要——读 conversation.json
//!   M4-a 语义轮次（ConverseReply 单一真源），按轮次快照检测本轮新增。
//!
//! 终端前缀各司其职（无第三套）：`[orchestrator]` 编排器流转/转写、`[pi]` 规划器
//! 语音、`[driver]` CLI driver 状态行（run/feed/status 保留不动）、`[chat]` 本壳
//! 提示音。

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use rustyline::error::ReadlineError;
use rustyline::history::MemHistory;
use rustyline::{Config, Editor};

use alfred_cli::governance::{
    build_governance_context, default_governance_base, default_governance_dir, feed_owner_message,
    init_governance_run, load_governance_run, persist_governance_run, run_governance_loop,
    state_label,
};
use alfred_core::conversation::{load_conversation, ConversationRole, ConversationSource};
use alfred_core::governance::{GovernanceOptions, GovernanceRun, GovernanceState, OwnerDecision};
use alfred_core::request::OwnerRequest;
use alfred_core::util::short_id;
use anyhow::{bail, Context, Result};

/// `alfred chat [--run-dir <dir>]`：owner 持续会话 REPL。
///
/// 入口定位（P3 发现规则）：显式 --run-dir 优先（有 state.json 即恢复，否则作为
/// 新 run 落点）；未指定则扫描默认基目录取 updated_at 最新的 run（任意态，同构
/// RFC3339 字典序）；挂起 run（plan_rejected/escalated）≥2 时不猜——列出清单并
/// 要求 --run-dir 消歧。
pub fn cmd_chat(args: &[String]) -> Result<()> {
    let mut run_dir_flag: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--run-dir" => {
                i += 1;
                run_dir_flag = Some(PathBuf::from(
                    args.get(i).with_context(|| "alfred chat: --run-dir 缺值")?,
                ));
            }
            other => bail!("alfred chat: 未知参数 {other:?}（--run-dir <dir>；--help 查看用法）"),
        }
        i += 1;
    }

    let (mut run_dir, mut run) = locate_run(run_dir_flag.as_deref())?;

    // REPL 横幅常显 run_dir（P3：owner 永远知道自己在哪个 run 上说话）；其余
    // 元数据极简——恢复态一行（run_id + state），需求不复述（升级包/终态呈现时
    // 仍可见）。
    match &run {
        Some(r) => {
            println!("[chat] ── alfred chat（owner 持续会话；Ctrl-D 退出）──");
            println!("[chat] run_dir: {}", run_dir.display());
            println!(
                "[chat] 恢复 run {}（state={}）",
                r.run_id,
                state_label(r.state())
            );
        }
        None => {
            println!("[chat] ── alfred chat（owner 持续会话；Ctrl-D 退出）──");
            println!("[chat] 未发现进行中的治理 run——请直接说需求。");
        }
    }

    // 断点恢复：流转中间态（进程在上次转移后被杀）无需属主输入，直接续跑。
    if let Some(r) = run.as_mut() {
        if matches!(
            r.state(),
            GovernanceState::PlanReviewing
                | GovernanceState::Executing
                | GovernanceState::ExecReviewing
        ) {
            match drive_loop(run.as_mut().expect("run"), &run_dir) {
                Ok(()) => {}
                Err(e) => reload_after_error(&mut run, &run_dir, &e)?,
            }
        }
    }

    let mut input = ChatInput::new();
    // 终态呈现一次性标记（进入循环后第一次遇到终态时呈现结果，随后是需求收集态）。
    let mut fresh_terminal = false;
    loop {
        match run.as_ref().map(GovernanceRun::state) {
            // ── 流转中间态：run_governance_loop 返回时必为挂起/终态/Reply 停驻；
            //    落到此处只可能是断点恢复/feed 出错 reload——直接续跑。 ──
            Some(GovernanceState::ExecReviewing) => {
                // 磁盘重入（9/3 欠账，用户死锁链修复②）：执行审查输入（契约/挂载
                // 语义/ws/对话记录）全在磁盘（run 目录 contract.json/dagspec/ws/
                // exec-N），重入执行审查直接从磁盘组装、不依赖进程内执行 outcome
                // （exec_review_step 已无 outcome 消费）。与 plan_reviewing/executing
                // 同构：drive_loop 推进到挂起/终态；审查失败走治理降级（escalated
                // 挂起拍板），不再"outcome 不落盘无法续跑"死锁。
                let mut r = run.take().expect("intermediate state has run");
                match drive_loop(&mut r, &run_dir) {
                    Ok(()) => run = Some(r),
                    Err(e) => reload_after_error(&mut run, &run_dir, &e)?,
                }
            }
            Some(GovernanceState::PlanReviewing) | Some(GovernanceState::Executing) => {
                let mut r = run.take().expect("intermediate state has run");
                match drive_loop(&mut r, &run_dir) {
                    Ok(()) => run = Some(r),
                    Err(e) => reload_after_error(&mut run, &run_dir, &e)?,
                }
            }
            // ── 需求收集态：无 run，或终态后的新需求 ──
            None | Some(GovernanceState::Completed) | Some(GovernanceState::Abandoned) => {
                if let Some(r) = run.as_ref() {
                    if !fresh_terminal {
                        present_terminal_result(r, &run_dir);
                        fresh_terminal = true;
                    }
                }
                let Some(requirement) = collect_requirement(&mut input)? else {
                    break;
                };
                // 确定性转写（title 按 chars() 截断 ≤40，中文安全）；验收标准
                // 默认=需求原文——不追问（pi 对话中需要澄清自然会问，Reply 分支）。
                let request = OwnerRequest::new(
                    short_id("chat"),
                    first_n_chars(&requirement, 40),
                    requirement.clone(),
                    requirement,
                );
                let new_dir = next_new_run_dir(run_dir_flag.as_deref());
                // 转写呈现极简：一行"已受理"（id/criteria 不再四行块铺陈）。
                println!("[chat] 新建 run: {}", new_dir.display());
                println!("[chat] 已受理：{}", request.title);
                let mut r =
                    match init_governance_run(&new_dir, request, GovernanceOptions::default()) {
                        Ok(r) => r,
                        Err(e) => {
                            println!("[chat] run 初始化失败：{e:#}");
                            continue;
                        }
                    };
                match drive_loop(&mut r, &new_dir) {
                    Ok(()) => {
                        run_dir = new_dir;
                        run = Some(r);
                        fresh_terminal = false;
                    }
                    Err(e) => {
                        // 尽力从 state.json 恢复；无 state.json（初始化即败）→ 回需求
                        // 收集态，残缺 run 目录留在磁盘可审计。
                        reload_after_error(&mut run, &new_dir, &e).ok();
                        if run.is_some() {
                            run_dir = new_dir;
                            fresh_terminal = false;
                        }
                    }
                }
            }
            // ── Planning 态：pi 已答复（§2.4 Reply 分支停驻），行=owner 消息续入对话 ──
            Some(GovernanceState::Planning) => {
                let Some(line) = input.read_line("[chat] 对 pi 说（一行；Ctrl-D 退出）：")?
                else {
                    break;
                };
                let line = line.trim();
                if line.is_empty() {
                    println!("[chat] 空输入已忽略。");
                    continue;
                }
                // P1-2：对话态同样暴露放弃出口（P2a 转移表支持）；其余（含"重试"——
                // Planning 无重跑语义）一律 Revise + 整行续入对话。
                let (decision, message) = parse_planning_decision(line);
                feed_and_present(&mut run, &run_dir, decision, &message)?;
            }
            // ── 挂起态：升级包呈现 + 确定性决策解析 ──
            Some(GovernanceState::PlanRejected) | Some(GovernanceState::Escalated) => {
                let r = run.as_ref().expect("suspended state has run");
                present_suspension(r, &run_dir);
                let Some(line) =
                    input.read_line("[chat] 回复：重试 / 放弃 / 或直接说修改意见：")?
                else {
                    break;
                };
                let line = line.trim();
                if line.is_empty() {
                    println!("[chat] 空输入已忽略。");
                    continue;
                }
                let (decision, message) = parse_suspended_decision(line);
                feed_and_present(&mut run, &run_dir, decision, &message)?;
            }
        }
    }
    println!("[chat] 会话结束。");
    Ok(())
}

/// 喂入 + owner 侧呈现（Planning/挂起两态共用）：feed_owner_message → `[pi]`
/// 答复/计划摘要（快照差分检测本轮新增 planner ConverseReply 轮，M4-a 单一真源）
/// → 状态行；Err → 打印 + reload 续 REPL。
fn feed_and_present(
    run: &mut Option<GovernanceRun>,
    run_dir: &Path,
    decision: OwnerDecision,
    message: &str,
) -> Result<()> {
    let ctx = match build_governance_context(run_dir) {
        Ok(ctx) => ctx,
        Err(e) => return reload_after_error(run, run_dir, &e),
    };
    let turns_before = conversation_turn_count(run_dir);
    let mut r = run.take().expect("feed state has run");
    match feed_owner_message(&mut r, &ctx, message, decision) {
        Ok(outcome) => {
            surface_planner_output(run_dir, &outcome.reply, turns_before);
            println!("[chat] 当前状态: {}", state_label(outcome.state));
            *run = Some(r);
        }
        Err(e) => reload_after_error(run, run_dir, &e)?,
    }
    Ok(())
}

/// 推进治理环到下一个挂起/终态/Reply 停驻（创建后首推与断点续跑共用）：
/// loop 返回后 persist（P3 崩溃恢复显式化）+ planner 产出呈现。
fn drive_loop(run: &mut GovernanceRun, run_dir: &Path) -> Result<()> {
    let ctx = build_governance_context(run_dir)?;
    let turns_before = conversation_turn_count(run_dir);
    let reply = run_governance_loop(run, &ctx)?;
    if let Err(e) = persist_governance_run(run_dir, run) {
        println!("[chat] state 持久化失败：{e:#}");
    }
    surface_planner_output(run_dir, &reply, turns_before);
    Ok(())
}

/// feed/loop 出错后的续会话处理（P3）：打印错误 + 从 state.json reload（磁盘单一
/// 真源——feed/loop 的转移均在其内部 persist，出错点两侧一致）继续会话不退进程；
/// reload 失败（state.json 不可读/缺失）→ 显式报错退出（run 已不可续）。
fn reload_after_error(
    run: &mut Option<GovernanceRun>,
    run_dir: &Path,
    e: &anyhow::Error,
) -> Result<()> {
    println!("[chat] 操作失败：{e:#}");
    let fresh = load_governance_run(run_dir).with_context(|| {
        format!(
            "alfred chat: run 状态重载失败（{}）——会话无法继续",
            run_dir.join("state.json").display()
        )
    })?;
    println!(
        "[chat] 已从 state.json 重载（state={}），会话继续（可重试/放弃/改口）。",
        state_label(fresh.state())
    );
    *run = Some(fresh);
    Ok(())
}

/// 入口 run 定位（P3 发现规则）。返回 (run_dir, run)——run 为 None 表示全新会话
/// （首个需求收集后建 run）。
fn locate_run(explicit: Option<&Path>) -> Result<(PathBuf, Option<GovernanceRun>)> {
    if let Some(dir) = explicit {
        if dir.join("state.json").is_file() {
            let run = load_governance_run(dir)
                .with_context(|| format!("alfred chat: 恢复 run {}", dir.display()))?;
            return Ok((dir.to_path_buf(), Some(run)));
        }
        // 显式目录无 state.json → 作为新 run 落点（全新会话）。
        return Ok((dir.to_path_buf(), None));
    }
    let base = default_governance_base();
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        // 基目录不存在 → 全新会话（新 run 落默认位）。
        Err(_) => return Ok((default_governance_dir(), None)),
    };
    // 多挂起消歧清单（P3）：挂起 run ≥2 时不猜。
    let mut suspended: Vec<(PathBuf, String, String)> = Vec::new();
    let mut latest: Option<(String, PathBuf, GovernanceRun)> = None;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join("state.json").is_file() {
            continue;
        }
        let Ok(r) = load_governance_run(&dir) else {
            continue;
        };
        if matches!(
            r.state(),
            GovernanceState::PlanRejected | GovernanceState::Escalated
        ) {
            suspended.push((
                dir.clone(),
                state_label(r.state()).to_string(),
                r.updated_at.clone(),
            ));
        }
        let take = latest
            .as_ref()
            .map(|(ts, _, _)| r.updated_at.as_str() > ts.as_str())
            .unwrap_or(true);
        if take {
            latest = Some((r.updated_at.clone(), dir, r));
        }
    }
    if suspended.len() > 1 {
        eprintln!(
            "[chat] 发现 {} 个挂起 run（plan_rejected/escalated），需 --run-dir 指定要续的：",
            suspended.len()
        );
        for (dir, state, ts) in &suspended {
            eprintln!(
                "[chat]   {}（state={state}, updated_at={ts}）",
                dir.display()
            );
        }
        bail!("alfred chat: 多个挂起 run 并存，请用 --run-dir 消歧");
    }
    match latest {
        Some((_, dir, r)) => Ok((dir, Some(r))),
        None => Ok((default_governance_dir(), None)),
    }
}

/// 挂起态决策解析（P1 纠偏：整行 trim 精确匹配，禁子串包含）：=="重试"→Retry、
/// =="放弃"→Abandon、其余一律 Revise + 整行作属主消息（最安全分支：进 planner
/// 它会追问澄清——"不要重试，改成X"这类否定句不会被误判成重跑）。
fn parse_suspended_decision(line: &str) -> (OwnerDecision, String) {
    match line {
        "重试" => (OwnerDecision::Retry, line.to_string()),
        "放弃" => (OwnerDecision::Abandon, line.to_string()),
        _ => (OwnerDecision::Revise, line.to_string()),
    }
}

/// Planning（对话）态解析：精确 =="放弃" → Abandon（P2a 转移表支持，属主放弃恒
/// 可选）；其余（含"重试"——Planning 无重跑语义）一律 Revise + 整行续入对话。
fn parse_planning_decision(line: &str) -> (OwnerDecision, String) {
    match line {
        "放弃" => (OwnerDecision::Abandon, line.to_string()),
        _ => (OwnerDecision::Revise, line.to_string()),
    }
}

/// 需求收集（单轮直提）：一行=需求（空行重问）。不追问验收标准——默认=需求
/// 原文（pi 对话中需要澄清自然会问，Reply 分支）。EOF → None（会话结束）。
fn collect_requirement(input: &mut ChatInput) -> Result<Option<String>> {
    loop {
        let Some(line) = input.read_line("[chat] 需求（一行；Ctrl-D 退出）：")? else {
            return Ok(None);
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            println!("[chat] 需求为空——请直接说需求。");
            continue;
        }
        return Ok(Some(line));
    }
}

/// 新 run 落点：显式 --run-dir 且该目录尚无 state.json（未被既有 run 占据）→ 用它；
/// 否则默认治理目录（fresh `run-<id>`，绝不覆盖既有 run——终态 run 的显式目录保持
/// 完整可审计）。
fn next_new_run_dir(explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(dir) if !dir.join("state.json").exists() => dir.to_path_buf(),
        _ => default_governance_dir(),
    }
}

/// 升级包呈现（工单④ 极简化）：2-3 行——挂起标题 + 意见/原因 + 拍板提示。
///
/// 按态取数单一真源：PlanRejected → plan_verdicts.last()（打回 reason）；
/// Escalated → exec_verdicts.last()（等级/意见/证据一行并呈）+ escalation_source
/// 摘要。缺 verdict（unscored 升级，如离线/审查容器故障）→ 读 audit.jsonl 最近
/// 升级事件。属主全可见用原始 verdict（禁词净化是 planner 侧投影，不适用此处）。
/// 砍掉 run_dir/需求复述/计划节点/attempts 等元数据行（run_dir 横幅已有；attempts
/// 等细节在 audit.jsonl/state.json 可查）。
fn present_suspension(run: &GovernanceRun, run_dir: &Path) {
    println!(
        "[chat] ── 治理挂起，等待属主拍板（state={}）──",
        state_label(run.state())
    );
    match run.state() {
        GovernanceState::PlanRejected => match run.plan_verdicts.last() {
            Some(v) => println!("[chat] 计划审查意见（打回）：{}", v.reason),
            None => println!(
                "[chat] 打回原因: {}",
                last_escalation_reason(run_dir).unwrap_or_else(|| "未知".into())
            ),
        },
        GovernanceState::Escalated => match run.exec_verdicts.last() {
            Some(v) => {
                println!(
                    "[chat] 执行审查意见（{:?}，来源 {:?}）：{}",
                    v.value, run.escalation_source, v.explanation
                );
            }
            None => println!(
                "[chat] 升级原因: {}（来源 {:?}）",
                last_escalation_reason(run_dir).unwrap_or_else(|| "未知".into()),
                run.escalation_source
            ),
        },
        _ => {}
    }
}

/// 终态呈现 + 新需求引导（工单⑤：呈现结果 + "新需求请直接说 / Ctrl-D 退出"）。
fn present_terminal_result(run: &GovernanceRun, run_dir: &Path) {
    match run.state() {
        GovernanceState::Completed => {
            println!(
                "[chat] ── run 完成（Completed）：需求「{}」已通过执行审查（验收 C）。",
                run.request.title
            );
            println!(
                "[chat] 产物: {}/ws（执行审查已 git diff 验收）",
                run_dir.display()
            );
        }
        GovernanceState::Abandoned => {
            println!(
                "[chat] ── run 已放弃（Abandoned）：需求「{}」。",
                run.request.title
            );
        }
        _ => {}
    }
    println!("[chat] 新需求请直接说（Ctrl-D 退出）。");
}

/// planner 产出呈现（P2 纠偏，单一真源 conversation.json M4-a 语义轮次）：
/// reply 分支的答复直显；否则扫描本轮新增轮次，最后一个 planner ConverseReply 轮
/// （= 建图分支的计划摘要）以 [pi] 呈现。
fn surface_planner_output(run_dir: &Path, reply: &Option<String>, turns_before: usize) {
    if let Some(r) = reply {
        println!("[pi] {r}");
        return;
    }
    let Ok(Some(log)) = load_conversation(run_dir) else {
        return;
    };
    for turn in log.turns.iter().skip(turns_before).rev() {
        if turn.role == ConversationRole::Planner
            && turn.source == ConversationSource::ConverseReply
        {
            println!("[pi] {}", turn.content);
            return;
        }
    }
}

/// 读 audit.jsonl 最近一条升级事件的 reason/error（unscored 升级时 verdict 历史
/// 为空，升级原因只在审计里——不静默）。
fn last_escalation_reason(run_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(run_dir.join("audit.jsonl")).ok()?;
    let mut found: Option<String> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(event) = v.get("event").and_then(|e| e.as_str()) else {
            continue;
        };
        if !event.contains("escalat") {
            continue;
        }
        let data = v.get("data");
        let detail = data
            .and_then(|d| {
                d.get("reason")
                    .or_else(|| d.get("error"))
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| data.map(|d| d.to_string()).unwrap_or_default());
        found = Some(format!("{event}: {detail}"));
    }
    found
}

/// title 截断：按 chars() 取前 n 字（中文安全——byte 截断会 panic/乱码）。
fn first_n_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 行输入器：交互双 tty → rustyline 行编辑（↑↓ 历史、←→ 移动；进程内会话级
/// 历史——需求收集/对话/拍板三态共享同一实例，历史贯通；不持久化文件）；否则
/// （管道/重定向喂入）保持裸字节读（非 UTF-8 容错，见 sanitize_raw_line）——
/// 提示打 stdout 并 flush（管道喂入时提示与输出同流，便于黑盒断言，e2e 管
/// 道路径依赖）。
struct ChatInput {
    editor: Option<Editor<(), MemHistory>>,
    stdin: io::Stdin,
}

impl ChatInput {
    fn new() -> Self {
        // 行编辑仅交互双 tty 启用：stdout 非 tty（如 `alfred chat | tee`）时编辑
        // UI 无处渲染，回退裸读保持现状。
        let editor = if io::stdin().is_terminal() && io::stdout().is_terminal() {
            match Editor::with_history(Config::default(), MemHistory::new()) {
                Ok(ed) => Some(ed),
                Err(e) => {
                    eprintln!("[chat] 行编辑初始化失败（{e}），回退裸读。");
                    None
                }
            }
        } else {
            None
        };
        Self {
            editor,
            stdin: io::stdin(),
        }
    }

    /// 读一行（EOF/Ctrl-C → None，会话结束——对齐裸读时代 Ctrl-D→None、
    /// Ctrl-C→SIGINT 终止语义，不 panic）。
    fn read_line(&mut self, prompt: &str) -> Result<Option<String>> {
        if let Some(editor) = self.editor.as_mut() {
            return match editor.readline(prompt) {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        editor.add_history_entry(line.as_str()).ok();
                    }
                    Ok(Some(line))
                }
                Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => Ok(None),
                Err(e) => Err(e).context("alfred chat: 读 stdin 失败"),
            };
        }
        print!("{prompt}");
        io::stdout().flush().ok();
        // 裸读走字节级：read_line 遇非 UTF-8 字节直接 Err（属主真实使用撞到
        // `stream did not contain valid UTF-8`，整个 REPL 崩退）。改 read_until
        // 读原始字节 → sanitize_raw_line（lossy + 转义过滤 + 去 \n/\r 尾）。
        let mut raw = Vec::new();
        match self.stdin.lock().read_until(b'\n', &mut raw) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(sanitize_raw_line(&raw))),
            Err(e) => Err(e).context("alfred chat: 读 stdin 失败"),
        }
    }
}

/// 裸读行净化（UTF-8 容错 + 控制序列过滤，只作用于裸读分支——tty 走 rustyline）：
/// - **不崩**：`read_line` 对非 UTF-8 字节直接 Err；这里字节级过滤后
///   `from_utf8_lossy`（非法字节 → U+FFFD 替身），坏行以替身字符照常进治理流
///   ——属主看得见，不静默丢；
/// - **去尾**：剥尾部 `\n`/`\r`（read_until 带回换行；read_line 三处调用点均先
///   `trim()`，干净输入语义逐字节不变，且与 rustyline 分支无换行尾对齐）；
/// - **过滤**：裸读无行编辑，方向键等控制序列会以原始转义字节混进消息体变成
///   乱码"命令"。剥 CSI 序列（`ESC[` + 参数/中间字节 0x20-0x3F + 终结字节
///   0x40-0x7E 的单序列——方向键 `ESC[A/B/C/D`、Delete `ESC[3~` 等）与裸控制
///   字符（<0x20，`\t` 保留）。仅匹配 ASCII 单字节；UTF-8 保证多字节序列内不
///   出现 ASCII 字节，中文安全。
fn sanitize_raw_line(raw: &[u8]) -> String {
    let mut end = raw.len();
    while end > 0 && matches!(raw[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    let bytes = &raw[..end];
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1B && bytes.get(i + 1) == Some(&b'[') {
            // 单个 CSI 序列：吞参数/中间字节直到终结字节；遇非序列字节/越界视
            // 为残缺——只丢 ESC[ 前缀，不吞后续正文。
            let mut j = i + 2;
            let complete = loop {
                match bytes.get(j) {
                    Some(c) if (0x40..=0x7E).contains(c) => {
                        j += 1;
                        break true;
                    }
                    Some(c) if (0x20..=0x3F).contains(c) => j += 1,
                    _ => break false,
                }
            };
            i = if complete { j } else { i + 2 };
        } else if b < 0x20 && b != b'\t' {
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::sanitize_raw_line;

    /// 干净 UTF-8 输入恒等：lossy 对合法输入不改内容，只剥行尾（调用点均
    /// trim()，语义逐字节不变）。
    #[test]
    fn clean_utf8_identity() {
        assert_eq!(sanitize_raw_line(b"hello\n"), "hello");
        assert_eq!(
            sanitize_raw_line("新需求：写 hello.txt\r\n".as_bytes()),
            "新需求：写 hello.txt"
        );
        assert_eq!(sanitize_raw_line(b"eof no newline"), "eof no newline");
    }

    /// 非 UTF-8 字节不崩：→ U+FFFD 替身（坏行照常进治理流——可见，不静默丢）。
    #[test]
    fn invalid_bytes_become_replacement() {
        assert_eq!(sanitize_raw_line(b"test\xff\xe6\x96\xb0\n"), "test\u{FFFD}新");
    }

    /// 方向键等 CSI 转义序列整段剥除，不进消息体。
    #[test]
    fn csi_sequences_filtered() {
        assert_eq!(sanitize_raw_line(b"\x1b[Dtext\x1b[C\n"), "text");
        assert_eq!(sanitize_raw_line(b"\x1b[3~\n"), "");
        assert_eq!(sanitize_raw_line(b"\x1b[1;5Cgo\n"), "go");
    }

    /// 裸控制字符剥除（\t 保留）；残缺转义序列只丢 ESC[ 前缀，不吞正文。
    #[test]
    fn control_chars_and_malformed_escape() {
        assert_eq!(sanitize_raw_line(b"a\x07b\x00c\n"), "abc");
        assert_eq!(sanitize_raw_line(b"a\tb\n"), "a\tb");
        assert_eq!(sanitize_raw_line("\x1b[新需求".as_bytes()), "新需求");
    }
}

/// conversation.json 当前轮数（呈现差分快照用；读取失败按 0）。
fn conversation_turn_count(run_dir: &Path) -> usize {
    load_conversation(run_dir)
        .ok()
        .flatten()
        .map(|l| l.turns.len())
        .unwrap_or(0)
}
