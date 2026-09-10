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
//! 语音（含 `[pi] ⋯` 过程动作行——converse 执行期间后台 tail planner AGT 审计
//! `planner/agt/audit/audit.jsonl` 实时打探查/读取/拦截行，动作真源=审计、此处
//! 仅呈现层投影）、`[driver]` CLI driver 状态行（run/feed/status 保留不动）、
//! `[chat]` 本壳提示音。
//!
//! # S2a 输出双发 + S2b 治理环通知事件化（覆盖对照表见 chat_events.rs）
//!
//! 本壳全部 owner 可见输出点位经 [`SessionSink`]：REPL 路径（管道/降级）只打印
//! ——与原 println!/eprintln! 逐字节等价（e2e chat.sh 硬底线）；TUI 路径打印
//! 保留（进程 stdout/stderr 已被 chat_tui 重定向进捕获管道，不毁界面）+
//! [`ChatEvent`] 双发（载荷=去前缀正文，契约见 chat_events.rs）。会话主体
//! [`chat_session`] 由 REPL（stdin 行）与 TUI 治理 worker（通道行，
//! [`run_tui_session`]）共用——输入抽象 [`OwnerInput`]，语义单一真源不重写。
//! governance.rs 侧 `[orchestrator]` 点位 S2b 起同契约事件化：会话侧把
//! `sink.events` 接入 `GovernanceContext`（drive_loop / feed_and_present 两处
//! 建 ctx 点），治理环通知经 governance::`orchestrator_notice` 单一出口分流
//! （REPL 打终端 / TUI 发事件），捕获管道透传路径退役。

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use parking_lot::Mutex;
use rustyline::error::ReadlineError;
use rustyline::history::MemHistory;
use rustyline::{Config, Editor};

use alfred_cli::chat_events::{ActionKind, ChatEvent, ChatEventSender, ErrorStream};
use alfred_cli::governance::{
    build_governance_context, default_governance_base, default_governance_dir, feed_owner_message,
    init_governance_run, load_governance_run, persist_governance_run, run_governance_loop,
    state_label,
};
use alfred_core::conversation::{load_conversation, ConversationRole, ConversationSource};
use alfred_core::governance::{
    EscalationSource, GovernanceOptions, GovernanceRun, GovernanceState, OwnerDecision,
};
use alfred_core::request::OwnerRequest;
use alfred_core::util::short_id;
use anyhow::{bail, Context, Result};

/// 会话输出 sink（S2a 双发接线，覆盖对照表的 chat.rs 侧点位）。
///
/// REPL（events=None）：只打印——与原点位逐字节等价（格式化在调用点完成后整体
/// 交给 sink，sink 只做 `[chat] `/`[pi] ` 前缀拼接；e2e 管道路径硬底线）。TUI
/// （events=Some）：打印保留（TUI 模式进程 stdout/stderr 已被 chat_tui 重定向进
/// 捕获管道，不毁界面）+ ChatEvent 双发，载荷=去前缀正文（契约：前缀是 sink 的
/// 渲染关注点）。`send` 吞错（接收端 Drop → false，TUI 先退不杀会话线程）。
struct SessionSink {
    events: Option<ChatEventSender>,
}

impl SessionSink {
    /// REPL sink：无事件端（打印即全部）。
    fn repl() -> Self {
        Self { events: None }
    }

    /// TUI sink：打印 + 事件双发。
    fn tui(events: ChatEventSender) -> Self {
        Self { events: Some(events) }
    }

    /// `[chat] {body}` 状态/转写行 → [`ChatEvent::OrchestratorNotice`]（纯正文）。
    fn notice(&self, body: String) {
        println!("[chat] {body}");
        if let Some(tx) = &self.events {
            tx.send(ChatEvent::OrchestratorNotice(body));
        }
    }

    /// `[pi] {body}` 答复/计划摘要 → [`ChatEvent::PiReply`]。
    fn pi_reply(&self, body: String) {
        println!("[pi] {body}");
        if let Some(tx) = &self.events {
            tx.send(ChatEvent::PiReply(body));
        }
    }

    /// 挂起意见/升级原因（present_suspension 四分支，格式化留在数据所在地）→
    /// [`ChatEvent::EscalationPrompt`]。
    fn escalation(&self, reason: String, source: EscalationSource) {
        println!("[chat] {reason}");
        if let Some(tx) = &self.events {
            tx.send(ChatEvent::EscalationPrompt { reason, source });
        }
    }

    /// stdout 错误点位（run 初始化/state 持久化/操作失败）→
    /// [`ChatEvent::Error`]（stream=Stdout）。
    fn error_stdout(&self, body: String) {
        println!("[chat] {body}");
        if let Some(tx) = &self.events {
            tx.send(ChatEvent::Error {
                message: body,
                stream: ErrorStream::Stdout,
            });
        }
    }

    /// stderr 错误点位（多挂起消歧清单）→ [`ChatEvent::Error`]（stream=Stderr）。
    fn error_stderr(&self, body: String) {
        eprintln!("[chat] {body}");
        if let Some(tx) = &self.events {
            tx.send(ChatEvent::Error {
                message: body,
                stream: ErrorStream::Stderr,
            });
        }
    }
}

/// 属主输入源抽象：REPL（stdin/rustyline 行）与 TUI 治理 worker（通道行）同一
/// read_line 语义（EOF/中断 → None 会话结束），会话主体 [`chat_session`] 泛型
/// 复用不重写。
trait OwnerInput {
    fn read_line(&mut self, prompt: &str) -> Result<Option<String>>;
}

/// TUI 治理 worker 输入源：TUI submit → 通道行。prompt 不经通道（TUI 输入区
/// 提示由 run 态派生，见 chat_events.rs 排除项）；发送端 Drop（TUI 退出）→
/// None（EOF 语义，对齐 REPL Ctrl-D）。
struct ChannelInput {
    rx: mpsc::Receiver<String>,
}

impl OwnerInput for ChannelInput {
    fn read_line(&mut self, _prompt: &str) -> Result<Option<String>> {
        match self.rx.recv() {
            Ok(line) => Ok(Some(line)),
            Err(_) => Ok(None),
        }
    }
}

/// `alfred chat [--run-dir <dir>]`：owner 持续会话入口（TUI 方案 S1 起分流）。
///
/// 入口分流：交互双 tty 且 TERM 有效 → TUI 呈现层（`chat_tui`，S1 骨架）；
/// 否则（管道/重定向/脚本）走本函数既有 REPL——非 tty 管道行为逐字节不变
/// （e2e 硬底线）。
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

    // TUI 降级门（方案 `.plans/施工方案-TUI界面.md`）：交互双 tty 且 TERM 有效
    // → TUI 呈现层（S2a 汇合：治理 worker 线程跑 chat_session，事件/看板喂
    // chat_tui 事件循环，见 chat_tui.rs）；否则（管道/重定向/dumb 终端）降级
    // 下方既有 REPL 路径——非 tty 管道行为逐字节不变，e2e 硬底线。TUI 初始
    // 化/运行失败（极罕见）打错误回退 REPL，保持入口恒可用。
    if crate::chat_tui::tui_supported() {
        match crate::chat_tui::run(run_dir_flag.as_deref()) {
            Ok(()) => return Ok(()),
            // 覆盖表 Error/Stderr 点位，但此处 TUI 已退（事件端无存）且即将
            // 进入 REPL——直打 stderr 与 REPL sink 行为逐字节一致，双发无对象。
            Err(e) => eprintln!("[chat] TUI 运行失败（{e:#}），回退 REPL。"),
        }
    }

    chat_session_repl(run_dir_flag.as_deref())
}

/// REPL 路径（管道/降级）：stdin 逐行进、stdout 直打出（逐字节不变）。
fn chat_session_repl(run_dir_flag: Option<&Path>) -> Result<()> {
    let sink = SessionSink::repl();
    chat_session(&sink, run_dir_flag, None, || ChatInput::new(&sink))
}

/// TUI 治理 worker 入口（chat_tui 起线程调用，主线程跑 TUI 事件循环）：通道
/// 输入 + 事件 sink 跑同一会话主体 [`chat_session`]。Err（定位失败/reload 失败
/// 等 REPL 会冒泡退出的错误）转 [`ChatEvent::Error`] 事件呈现后 worker 退出
/// ——TUI 侧输入通道断开即知会话不可续。
pub(crate) fn run_tui_session(
    run_dir_flag: Option<PathBuf>,
    input: mpsc::Receiver<String>,
    events: ChatEventSender,
    run_dir_out: Arc<Mutex<Option<PathBuf>>>,
) {
    let sink = SessionSink::tui(events);
    let watch = run_dir_out;
    if let Err(e) = chat_session(&sink, run_dir_flag.as_deref(), Some(&watch), || ChannelInput {
        rx: input,
    }) {
        // REPL 由 main 的 Result 打印（"Error: …"）；TUI 经 Error 事件呈现。
        sink.error_stderr(format!("{e:#}"));
    }
}

/// 会话主体（REPL 与 TUI 治理 worker 共用）：定位 run → 横幅 → 断点恢复 →
/// 状态循环。`input` 抽象属主输入源（stdin 行 / 通道行）；`sink` 承接全部
/// owner 可见输出（REPL 直打 / TUI 双发）；`run_dir_watch`（TUI 传入）同步
/// 当前 run 目录给看板轮询（dashboard snapshot 数据源），REPL 传 None。
/// `make_input` 在断点恢复后原位构造输入器（REPL 行编辑初始化的 eprintln
/// 保持原时序——逐字节底线）。
fn chat_session<I: OwnerInput>(
    sink: &SessionSink,
    run_dir_flag: Option<&Path>,
    run_dir_watch: Option<&Arc<Mutex<Option<PathBuf>>>>,
    make_input: impl FnOnce() -> I,
) -> Result<()> {
    let publish_run_dir = |dir: &Path| {
        if let Some(w) = run_dir_watch {
            *w.lock() = Some(dir.to_path_buf());
        }
    };
    let (mut run_dir, mut run) = locate_run(run_dir_flag, sink)?;
    publish_run_dir(&run_dir);

    // REPL 横幅常显 run_dir（P3：owner 永远知道自己在哪个 run 上说话）；其余
    // 元数据极简——恢复态一行（run_id + state），需求不复述（升级包/终态呈现时
    // 仍可见）。
    match &run {
        Some(r) => {
            sink.notice("── alfred chat（owner 持续会话；Ctrl-D 退出）──".into());
            sink.notice(format!("run_dir: {}", run_dir.display()));
            sink.notice(format!(
                "恢复 run {}（state={}）",
                r.run_id,
                state_label(r.state())
            ));
        }
        None => {
            sink.notice("── alfred chat（owner 持续会话；Ctrl-D 退出）──".into());
            sink.notice("未发现进行中的治理 run——请直接说需求。".into());
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
            match drive_loop(run.as_mut().expect("run"), &run_dir, sink) {
                Ok(()) => {}
                Err(e) => reload_after_error(&mut run, &run_dir, &e, sink)?,
            }
        }
    }

    let mut input = make_input();
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
                match drive_loop(&mut r, &run_dir, sink) {
                    Ok(()) => run = Some(r),
                    Err(e) => reload_after_error(&mut run, &run_dir, &e, sink)?,
                }
            }
            Some(GovernanceState::PlanReviewing) | Some(GovernanceState::Executing) => {
                let mut r = run.take().expect("intermediate state has run");
                match drive_loop(&mut r, &run_dir, sink) {
                    Ok(()) => run = Some(r),
                    Err(e) => reload_after_error(&mut run, &run_dir, &e, sink)?,
                }
            }
            // ── 需求收集态：无 run，或终态后的新需求 ──
            None | Some(GovernanceState::Completed) | Some(GovernanceState::Abandoned) => {
                if let Some(r) = run.as_ref() {
                    if !fresh_terminal {
                        present_terminal_result(r, &run_dir, sink);
                        fresh_terminal = true;
                    }
                }
                let Some(requirement) = collect_requirement(&mut input, sink)? else {
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
                let new_dir = next_new_run_dir(run_dir_flag);
                // 转写呈现极简：一行"已受理"（id/criteria 不再四行块铺陈）。
                sink.notice(format!("新建 run: {}", new_dir.display()));
                sink.notice(format!("已受理：{}", request.title));
                let mut r =
                    match init_governance_run(&new_dir, request, GovernanceOptions::default()) {
                        Ok(r) => r,
                        Err(e) => {
                            sink.error_stdout(format!("run 初始化失败：{e:#}"));
                            continue;
                        }
                    };
                // init 落盘 state.json 即发布——首段 drive 期间看板/顶栏就能
                // 实时跟上（不必等 drive 返回；后续 Ok/reload 同目录不重发）。
                publish_run_dir(&new_dir);
                match drive_loop(&mut r, &new_dir, sink) {
                    Ok(()) => {
                        run_dir = new_dir;
                        run = Some(r);
                        fresh_terminal = false;
                    }
                    Err(e) => {
                        // 尽力从 state.json 恢复；无 state.json（初始化即败）→ 回需求
                        // 收集态，残缺 run 目录留在磁盘可审计。
                        reload_after_error(&mut run, &new_dir, &e, sink).ok();
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
                    sink.notice("空输入已忽略。".into());
                    continue;
                }
                // P1-2：对话态同样暴露放弃出口（P2a 转移表支持）；其余（含"重试"——
                // Planning 无重跑语义）一律 Revise + 整行续入对话。
                let (decision, message) = parse_planning_decision(line);
                feed_and_present(&mut run, &run_dir, decision, &message, sink)?;
            }
            // ── 挂起态：升级包呈现 + 确定性决策解析 ──
            Some(GovernanceState::PlanRejected) | Some(GovernanceState::Escalated) => {
                let r = run.as_ref().expect("suspended state has run");
                present_suspension(r, &run_dir, sink);
                let Some(line) =
                    input.read_line("[chat] 回复：重试 / 放弃 / 或直接说修改意见：")?
                else {
                    break;
                };
                let line = line.trim();
                if line.is_empty() {
                    sink.notice("空输入已忽略。".into());
                    continue;
                }
                let (decision, message) = parse_suspended_decision(line);
                feed_and_present(&mut run, &run_dir, decision, &message, sink)?;
            }
        }
    }
    sink.notice("会话结束。".into());
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
    sink: &SessionSink,
) -> Result<()> {
    let mut ctx = match build_governance_context(run_dir) {
        Ok(ctx) => ctx,
        Err(e) => return reload_after_error(run, run_dir, &e, sink),
    };
    // S2b：TUI 会话把事件通道接入治理环（governance.rs [orchestrator] 点位
    // 事件化）；REPL（events=None）通知照旧打终端，逐字节不变。
    ctx.events = sink.events.clone();
    let turns_before = conversation_turn_count(run_dir);
    let mut r = run.take().expect("feed state has run");
    // 过程呈现（工单：规划过程透明）：feed（Planning 续聊 / 挂起拍板重规划）
    // 内部续跑 converse——tail 窗口同 drive_loop；Abandon 等无 converse 的决策
    // 零输出、stop 即退。
    let tail = PlannerAuditTail::spawn(run_dir, sink.events.clone());
    let fed = feed_owner_message(&mut r, &ctx, message, decision);
    tail.stop();
    match fed {
        Ok(outcome) => {
            surface_planner_output(run_dir, &outcome.reply, turns_before, sink);
            sink.notice(format!("当前状态: {}", state_label(outcome.state)));
            *run = Some(r);
        }
        Err(e) => reload_after_error(run, run_dir, &e, sink)?,
    }
    Ok(())
}

/// 推进治理环到下一个挂起/终态/Reply 停驻（创建后首推与断点续跑共用）：
/// loop 返回后 persist（P3 崩溃恢复显式化）+ planner 产出呈现。
fn drive_loop(run: &mut GovernanceRun, run_dir: &Path, sink: &SessionSink) -> Result<()> {
    let mut ctx = build_governance_context(run_dir)?;
    // S2b：TUI 会话把事件通道接入治理环（同 feed_and_present）。
    ctx.events = sink.events.clone();
    let turns_before = conversation_turn_count(run_dir);
    // 过程呈现：converse（及维护者，同写 planner AGT 审计）执行期间 tail 审计打
    // 动作行；loop 返回（含 Err）先停 tail 再呈现——过程行先于答复/状态行打完。
    let tail = PlannerAuditTail::spawn(run_dir, sink.events.clone());
    let result = run_governance_loop(run, &ctx);
    tail.stop();
    let reply = result?;
    if let Err(e) = persist_governance_run(run_dir, run) {
        sink.error_stdout(format!("state 持久化失败：{e:#}"));
    }
    surface_planner_output(run_dir, &reply, turns_before, sink);
    Ok(())
}

/// feed/loop 出错后的续会话处理（P3）：打印错误 + 从 state.json reload（磁盘单一
/// 真源——feed/loop 的转移均在其内部 persist，出错点两侧一致）继续会话不退进程；
/// reload 失败（state.json 不可读/缺失）→ 显式报错退出（run 已不可续）。
fn reload_after_error(
    run: &mut Option<GovernanceRun>,
    run_dir: &Path,
    e: &anyhow::Error,
    sink: &SessionSink,
) -> Result<()> {
    sink.error_stdout(format!("操作失败：{e:#}"));
    let fresh = load_governance_run(run_dir).with_context(|| {
        format!(
            "alfred chat: run 状态重载失败（{}）——会话无法继续",
            run_dir.join("state.json").display()
        )
    })?;
    sink.notice(format!(
        "已从 state.json 重载（state={}），会话继续（可重试/放弃/改口）。",
        state_label(fresh.state())
    ));
    *run = Some(fresh);
    Ok(())
}

/// 入口 run 定位（P3 发现规则）。返回 (run_dir, run)——run 为 None 表示全新会话
/// （首个需求收集后建 run）。
fn locate_run(
    explicit: Option<&Path>,
    sink: &SessionSink,
) -> Result<(PathBuf, Option<GovernanceRun>)> {
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
        sink.error_stderr(format!(
            "发现 {} 个挂起 run（plan_rejected/escalated），需 --run-dir 指定要续的：",
            suspended.len()
        ));
        for (dir, state, ts) in &suspended {
            sink.error_stderr(format!("  {}（state={state}, updated_at={ts}）", dir.display()));
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
fn collect_requirement<I: OwnerInput>(input: &mut I, sink: &SessionSink) -> Result<Option<String>> {
    loop {
        let Some(line) = input.read_line("[chat] 需求（一行；Ctrl-D 退出）：")? else {
            return Ok(None);
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            sink.notice("需求为空——请直接说需求。".into());
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
fn present_suspension(run: &GovernanceRun, run_dir: &Path, sink: &SessionSink) {
    sink.notice(format!(
        "── 治理挂起，等待属主拍板（state={}）──",
        state_label(run.state())
    ));
    match run.state() {
        GovernanceState::PlanRejected => match run.plan_verdicts.last() {
            Some(v) => sink.escalation(
                format!("计划审查意见（打回）：{}", v.reason),
                EscalationSource::PlanReview,
            ),
            None => sink.escalation(
                format!(
                    "打回原因: {}",
                    last_escalation_reason(run_dir).unwrap_or_else(|| "未知".into())
                ),
                EscalationSource::PlanReview,
            ),
        },
        GovernanceState::Escalated => {
            // 升级来源：run 上的 Option<EscalationSource>（转移表 None 走执行侧
            // 重跑路由，事件侧同口径取 Execution 兜底）。
            let source = run.escalation_source.unwrap_or(EscalationSource::Execution);
            match run.exec_verdicts.last() {
                Some(v) => sink.escalation(
                    format!(
                        "执行审查意见（{:?}，来源 {:?}）：{}",
                        v.value, run.escalation_source, v.explanation
                    ),
                    source,
                ),
                None => sink.escalation(
                    format!(
                        "升级原因: {}（来源 {:?}）",
                        last_escalation_reason(run_dir).unwrap_or_else(|| "未知".into()),
                        run.escalation_source
                    ),
                    source,
                ),
            }
        }
        _ => {}
    }
}

/// 终态呈现 + 新需求引导（工单⑤：呈现结果 + "新需求请直接说 / Ctrl-D 退出"）。
fn present_terminal_result(run: &GovernanceRun, run_dir: &Path, sink: &SessionSink) {
    match run.state() {
        GovernanceState::Completed => {
            sink.notice(format!(
                "── run 完成（Completed）：需求「{}」已通过执行审查（验收 C）。",
                run.request.title
            ));
            sink.notice(format!(
                "产物: {}/ws（执行审查已 git diff 验收）",
                run_dir.display()
            ));
        }
        GovernanceState::Abandoned => {
            sink.notice(format!(
                "── run 已放弃（Abandoned）：需求「{}」。",
                run.request.title
            ));
        }
        _ => {}
    }
    sink.notice("新需求请直接说（Ctrl-D 退出）。".into());
}

/// planner 产出呈现（P2 纠偏，单一真源 conversation.json M4-a 语义轮次）：
/// reply 分支的答复直显；否则扫描本轮新增轮次，最后一个 planner ConverseReply 轮
/// （= 建图分支的计划摘要）以 [pi] 呈现。
fn surface_planner_output(
    run_dir: &Path,
    reply: &Option<String>,
    turns_before: usize,
    sink: &SessionSink,
) {
    if let Some(r) = reply {
        sink.pi_reply(r.clone());
        return;
    }
    let Ok(Some(log)) = load_conversation(run_dir) else {
        return;
    };
    for turn in log.turns.iter().skip(turns_before).rev() {
        if turn.role == ConversationRole::Planner
            && turn.source == ConversationSource::ConverseReply
        {
            sink.pi_reply(turn.content.clone());
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

/// 过程动作行截断上限（chars——与 title 截断同范式，中文安全；命令/路径均适用）。
const ACTION_TRUNC_CHARS: usize = 80;
/// 审计 tail 轮询间隔（ms）：终端实时感与空转开销的平衡点。
const TAIL_POLL_MS: u64 = 200;

/// planner AGT 审计过程呈现（规划过程透明）：converse 调用前 [`Self::spawn`]
/// 后台线程 tail `<run>/planner/agt/audit/audit.jsonl`（路径经
/// `planner_audit_path` 单一真源），新增决策行实时打 `[pi] ⋯` 动作行；converse
/// 返回后 [`Self::stop`] join——过程行先于 `[pi]` 答复打完，过程→答复连贯。
///
/// - **动作真源 = AGT 审计**（pi 子进程实时追加）：allow bash → `探查:`、
///   allow read → `读取:`、deny → `探查（被治理拦截）:`（owner 面向全可见——
///   AGT deny reason 对 planner 中性化是 planner 侧约束，不约束 owner 呈现）；
///   其余（write/edit 产出写）不呈现——过程行只呈现探查动作。
/// - **跨轮续写同文件**（audit-baseline 持久基线机制同源事实）：tail 从打开
///   时刻的文件末尾增量读，历史轮次行不重放；文件截断（len<offset）回退从 0
///   读（tail -F 语义，防偏移越界漏行）。
/// - **节流**：同一渲染行（decision+tool+目标）本 tail 会话内重复不重打。
/// - **离线/AGT 关闭**：audit 文件不出现 → 线程空轮询，stop 即退零输出（e2e
///   离线路径行为不变）。
/// - **退出安全**：线程只做非阻塞元数据/增量读 + 短睡眠，stop 置位后 ≤1 轮询
///   周期（末轮再 drain 一次，兜住停止前最后窗口落盘的行）退出，join 无恐慌
///   路径；打印走 `writeln!` 吞错（不 `println!`——写失败 panic 会杀后台线程）。
///   spawn 失败（线程资源耗尽）→ 无过程行呈现，converse 照常。
struct PlannerAuditTail {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PlannerAuditTail {
    fn spawn(run_dir: &Path, events: Option<ChatEventSender>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name("planner-audit-tail".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                let path = alfred_planner::host::planner_audit_path(run_dir);
                move || tail_planner_audit(&path, &stop, events)
            })
            .ok();
        Self { stop, handle }
    }

    /// converse 返回后调用（所有路径——含 Err）：置位 + join。
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// tail 主体：轮询步进（[`AuditTailState::poll`]）→ 终端行 + 事件双发；stop
/// 置位后末轮再 poll 一次（兜住停止前最后窗口落盘的行）退出。语义全在 poll
/// （可测），线程壳只有步进+呈现。事件端 None（REPL 路径）只打终端行。
fn tail_planner_audit(path: &Path, stop: &AtomicBool, events: Option<ChatEventSender>) {
    let mut state = AuditTailState::new(path.to_path_buf());
    loop {
        for (kind, detail) in state.poll() {
            emit_process_line(&format!("[pi] ⋯ {}", pi_action_text(kind, &detail)));
            if let Some(tx) = &events {
                tx.send(ChatEvent::PiAction { kind, detail });
            }
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(TAIL_POLL_MS));
    }
}

/// 审计 tail 轮询状态机（单步推进语义可测）：持有文件偏移/半行缓冲/已打印行
/// 集，[`Self::poll`] 一步 = 等创建 → 首开定位末尾 → 增量读新行 → 渲染节流，
/// 返回本步应打印的动作行（顺序）；无新内容 → 空。
struct AuditTailState {
    path: PathBuf,
    offset: u64,
    /// tail 启动时文件已存在（多轮续写）：首开定位到打开时刻末尾——历史轮次行
    /// 不重放。启动时无文件（首轮）：文件在本会话中出现——从 0 读，写进来的
    /// 全是本轮新行（pi 启动慢于首个轮询时，首行已在文件里也不能当历史跳过）。
    existed_at_start: bool,
    /// 已完成首开定位（false = 文件还没出现，继续等创建）。
    opened: bool,
    /// 半行缓冲（并发写半行不误渲，残缺尾行下轮续读）。
    pending: Vec<u8>,
    /// 节流：本 tail 会话内已打印的渲染行（同 command 重复不重打）。
    printed: std::collections::HashSet<String>,
}

impl AuditTailState {
    fn new(path: PathBuf) -> Self {
        let existed_at_start = std::fs::metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        Self {
            path,
            offset: 0,
            existed_at_start,
            opened: false,
            pending: Vec::new(),
            printed: std::collections::HashSet::new(),
        }
    }
    /// 轮询一步。任何文件系统错误（文件消失等）按“无新内容”处理，状态保留
    /// 下轮重试——不崩不跳。
    fn poll(&mut self) -> Vec<(ActionKind, String)> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Vec::new();
        };
        if !meta.is_file() {
            return Vec::new();
        }
        let len = meta.len();
        if !self.opened {
            // 首开定位：见 `existed_at_start` 字段注释（首轮从 0 / 续写从末尾）。
            self.offset = if self.existed_at_start { len } else { 0 };
            self.opened = true;
        } else if len < self.offset {
            // 截断/重置：偏移回退重读（tail -F 语义，防偏移越界漏行）。
            self.offset = 0;
            self.pending.clear();
        }
        if len <= self.offset {
            return Vec::new();
        }
        self.read_delta()
    }

    /// 读 `[offset, EOF)` 增量进 pending，剥完整行渲染节流；任何读失败停在
    /// 原位（下轮重试）。
    fn read_delta(&mut self) -> Vec<(ActionKind, String)> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        self.pending.extend_from_slice(&buf);
        let mut out = Vec::new();
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.pending[..nl]).into_owned();
            self.pending.drain(..=nl);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((kind, detail)) = parse_audit_action(line) {
                if self.printed.insert(pi_action_text(kind, &detail)) {
                    out.push((kind, detail));
                }
            }
        }
        out
    }
}

/// 过程行打终端：`writeln!` 吞错（坏管道不杀线程）；行缓冲 stdout 遇 \n 自动
/// flush，管道路径同样及时落日志（e2e 日志可断言）。
fn emit_process_line(text: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// AGT 审计行 → REPL 终端行（`[pi] ⋯ {…}`）：测试断言面（渲染 = parse_audit_action
/// + pi_action_text + 前缀拼接，与 [`tail_planner_audit`] 打印行同一套件真源）。
#[cfg(test)]
fn render_audit_action(line: &str) -> Option<String> {
    let (kind, detail) = parse_audit_action(line)?;
    Some(format!("[pi] ⋯ {}", pi_action_text(kind, &detail)))
}

/// AGT 审计行 → 动作分类 + 已截断目标（纯函数：tail 线程、事件载荷与测试共用
/// 单一真源）。
///
/// 宽进：多余字段忽略；非法 JSON / 未知 decision / 无呈现目标 → None（不呈现
/// 不崩——审计是 pi 子进程写的，坏行不能杀呈现）。
fn parse_audit_action(line: &str) -> Option<(ActionKind, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let decision = v.get("decision").and_then(|d| d.as_str())?;
    let tool = v.get("tool_name").and_then(|t| t.as_str()).unwrap_or("");
    let command = v
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let path = v
        .get("path")
        .and_then(|p| p.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (decision, tool) {
        ("allow", "bash") => Some((ActionKind::Probe, truncate_action(command?))),
        ("allow", "read") => Some((ActionKind::Read, truncate_action(path?))),
        // deny 全可见（任意工具——拦截即治理边界信号）；command 缺失回退 path。
        ("deny", _) => Some((
            ActionKind::Blocked,
            truncate_action(command.or(path)?),
        )),
        _ => None,
    }
}

/// 过程动作行格式（kind → label + 目标文本）：REPL 终端行（`[pi] ⋯ {…}`）与
/// TUI 左列渲染共用单一真源（chat_events.rs ActionKind 文档口径——行格式
/// 真源留在本渲染侧，事件只承载分类）。
pub(crate) fn pi_action_text(kind: ActionKind, detail: &str) -> String {
    match kind {
        ActionKind::Probe => format!("探查: {detail}"),
        ActionKind::Read => format!("读取: {detail}"),
        ActionKind::Blocked => format!("探查（被治理拦截）: {detail}"),
    }
}

/// 动作目标截断：空白规整（多行命令压平单行——过程行一行一动作）+ 超限
/// [`ACTION_TRUNC_CHARS`] 字截断带省略号（chars 中文安全，同 [`first_n_chars`]）。
fn truncate_action(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > ACTION_TRUNC_CHARS {
        format!("{}…", first_n_chars(&flat, ACTION_TRUNC_CHARS))
    } else {
        flat
    }
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
    /// `sink` 仅承接行编辑初始化失败行（覆盖表 Error/Stderr 点位；TUI 路径
    /// 用 [`ChannelInput`] 不会走到这里）。
    fn new(sink: &SessionSink) -> Self {
        // 行编辑仅交互双 tty 启用：stdout 非 tty（如 `alfred chat | tee`）时编辑
        // UI 无处渲染，回退裸读保持现状。
        let editor = if io::stdin().is_terminal() && io::stdout().is_terminal() {
            match Editor::with_history(Config::default(), MemHistory::new()) {
                Ok(ed) => Some(ed),
                Err(e) => {
                    sink.error_stderr(format!("行编辑初始化失败（{e}），回退裸读。"));
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
}

/// 读一行（EOF/Ctrl-C → None，会话结束——对齐裸读时代 Ctrl-D→None、
/// Ctrl-C→SIGINT 终止语义，不 panic）。
impl OwnerInput for ChatInput {
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
    use super::{
        render_audit_action, sanitize_raw_line, truncate_action, AuditTailState, PlannerAuditTail,
    };
    use alfred_cli::chat_events::ActionKind;

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
        assert_eq!(
            sanitize_raw_line(b"test\xff\xe6\x96\xb0\n"),
            "test\u{FFFD}新"
        );
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

    // ── planner AGT 审计过程行渲染（动作真源 audit.jsonl → [pi] ⋯ 呈现投影） ──

    /// allow 两形态：bash → 探查（command）、read → 读取（path）；真实审计行
    /// 字段子集宽进（多余字段忽略）。
    #[test]
    fn audit_action_allow_probe_and_read() {
        let bash = r#"{"ts":"2026-09-06T20:18:32.690Z","tool_name":"bash","tool_call_id":"call_d43ee88cb0944ef2bf4fa2c3","command":"omp --help","decision":"allow","rule":null,"reason":"default_action=allow"}"#;
        assert_eq!(
            render_audit_action(bash).as_deref(),
            Some("[pi] ⋯ 探查: omp --help")
        );
        let read = r#"{"ts":"2026-09-06T20:18:33.000Z","tool_name":"read","path":"/tmp/ws/marker.txt","decision":"allow"}"#;
        assert_eq!(
            render_audit_action(read).as_deref(),
            Some("[pi] ⋯ 读取: /tmp/ws/marker.txt")
        );
    }

    /// deny owner 全可见（任意工具）；无 command 的 deny 回退 path 呈现。
    #[test]
    fn audit_action_deny_owner_visible() {
        let deny_bash = r#"{"tool_name":"bash","command":"ls ~/.omp/runs","decision":"deny","rule":"run-dir-guard"}"#;
        assert_eq!(
            render_audit_action(deny_bash).as_deref(),
            Some("[pi] ⋯ 探查（被治理拦截）: ls ~/.omp/runs")
        );
        let deny_read = r#"{"tool_name":"read","path":"/run/conversation.json","decision":"deny"}"#;
        assert_eq!(
            render_audit_action(deny_read).as_deref(),
            Some("[pi] ⋯ 探查（被治理拦截）: /run/conversation.json")
        );
    }

    /// 80 字截断带省略号（chars 中文安全）；多行命令压平单行（一行一动作）。
    #[test]
    fn audit_action_truncate_and_flatten() {
        let line = format!(
            r#"{{"tool_name":"bash","command":"{}","decision":"allow"}}"#,
            "x".repeat(100)
        );
        let rendered = render_audit_action(&line).expect("long command renders");
        let prefix = "[pi] ⋯ 探查: ".chars().count();
        assert_eq!(rendered.chars().count(), prefix + 80 + 1, "80 字 + 省略号");
        assert!(rendered.ends_with('…'));

        // JSON \n 转义解析为真换行 → 压平；多空格规整。
        let multiline = r#"{"tool_name":"bash","command":"echo a\nls   -la","decision":"allow"}"#;
        assert_eq!(
            render_audit_action(multiline).as_deref(),
            Some("[pi] ⋯ 探查: echo a ls -la")
        );

        let cn = format!(
            r#"{{"tool_name":"read","path":"{}","decision":"allow"}}"#,
            "超".repeat(100)
        );
        let rendered = render_audit_action(&cn).expect("long path renders");
        assert_eq!(
            rendered.chars().count(),
            "[pi] ⋯ 读取: ".chars().count() + 80 + 1,
            "中文按 chars 截断不 panic"
        );
    }

    /// 非呈现面静默跳过（None 不崩）：write/edit 产出写、未知 decision、无目标、
    /// 坏 JSON——审计是 pi 子进程写的，坏行不能杀呈现。
    #[test]
    fn audit_action_skips_non_probe_lines() {
        let write =
            r#"{"tool_name":"write","path":"/run/planner/outputs/reply.txt","decision":"allow"}"#;
        assert_eq!(render_audit_action(write), None);
        assert_eq!(
            render_audit_action(r#"{"tool_name":"bash","decision":"maybe"}"#),
            None
        );
        // bash 无 command / read 无 path → 无呈现目标。
        assert_eq!(
            render_audit_action(r#"{"tool_name":"bash","decision":"allow"}"#),
            None
        );
        assert_eq!(
            render_audit_action(r#"{"tool_name":"read","decision":"allow"}"#),
            None
        );
        assert_eq!(render_audit_action("not-json"), None);
        assert_eq!(render_audit_action(""), None);
    }

    /// 空白目标（空 command/path）不呈现空行动作。
    #[test]
    fn audit_action_skips_empty_targets() {
        assert_eq!(
            render_audit_action(r#"{"tool_name":"bash","command":"   ","decision":"allow"}"#),
            None
        );
        assert_eq!(truncate_action("  \n  "), "");
    }

    // ── 审计 tail 状态机（真实文件系统，断言面 = poll 返回的动作行序列） ──

    /// 增量语义：文件未建零输出 → 历史轮次行不重放 → 本轮新增按序渲染 → 同
    /// command 节流 → 半行不误渲补齐后渲出 → 截断回退重读。
    #[test]
    fn audit_tail_state_polls_incremental_lines() {
        let dir = std::env::temp_dir().join(format!("alfred-chat-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audit = dir.join("audit.jsonl");

        // 文件未创建（离线 / AGT 尚未落盘）→ 空轮询零输出。
        let mut st = AuditTailState::new(audit.clone());
        assert_eq!(st.poll(), Vec::<(ActionKind, String)>::new());

        // 首轮竞态（真跑实证：pi 启动慢于首个轮询，文件带着首行出现）：启动时
        // 无文件 → 会话中出现即从 0 读，已在文件里的行也照常渲出（不当历史跳过）。
        std::fs::write(
            &audit,
            concat!(
                r#"{"tool_name":"bash","command":"ls -la .","decision":"allow"}"#,
                "\n"
            ),
        )
        .unwrap();
        assert_eq!(
            st.poll(),
            vec![(ActionKind::Probe, "ls -la .".to_string())]
        );

        // 多轮语义：tail 启动时文件已存在（上一轮 converse 产物）→ 历史不重放。
        let mut st = AuditTailState::new(audit.clone());
        assert_eq!(st.poll(), Vec::<(ActionKind, String)>::new());

        // 本轮新增：探查/读取/拦截按序渲染。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&audit)
            .unwrap();
        writeln!(
            f,
            r#"{{"tool_name":"bash","command":"ls src","decision":"allow"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"tool_name":"read","path":"/ws/Cargo.toml","decision":"allow"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"tool_name":"bash","command":"cat ~/.omp/runs","decision":"deny"}}"#
        )
        .unwrap();
        drop(f);
        assert_eq!(
            st.poll(),
            vec![
                (ActionKind::Probe, "ls src".to_string()),
                (ActionKind::Read, "/ws/Cargo.toml".to_string()),
                (ActionKind::Blocked, "cat ~/.omp/runs".to_string()),
            ]
        );

        // 节流：同 command 重复不重打；不同命令照常。
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&audit)
            .unwrap();
        writeln!(
            f,
            r#"{{"tool_name":"bash","command":"ls src","decision":"allow"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"tool_name":"bash","command":"ls tests","decision":"allow"}}"#
        )
        .unwrap();
        drop(f);
        assert_eq!(st.poll(), vec![(ActionKind::Probe, "ls tests".to_string())]);

        // 半行（无 \n）不误渲；补齐后渲出。
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&audit)
            .unwrap();
        write!(f, r#"{{"tool_name":"read","path":"/ws/half"#).unwrap();
        drop(f);
        assert_eq!(st.poll(), Vec::<(ActionKind, String)>::new());
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&audit)
            .unwrap();
        writeln!(f, r#".txt","decision":"allow"}}"#).unwrap();
        drop(f);
        assert_eq!(st.poll(), vec![(ActionKind::Read, "/ws/half.txt".to_string())]);

        // 截断重置：文件缩回 < offset → 回退从 0 重读（新行照常渲出）。
        std::fs::write(
            &audit,
            concat!(
                r#"{"tool_name":"read","path":"/fresh/x.txt","decision":"allow"}"#,
                "\n"
            ),
        )
        .unwrap();
        assert_eq!(st.poll(), vec![(ActionKind::Read, "/fresh/x.txt".to_string())]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 线程壳退出安全：无 audit 文件（离线路径）spawn→stop 干净 join，不残留
    /// 不 panic；spawn 失败（资源耗尽 → handle=None）stop 同样安全。
    #[test]
    fn planner_audit_tail_stop_joins_cleanly() {
        let dir =
            std::env::temp_dir().join(format!("alfred-chat-tail-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run_dir = dir.join("run");
        std::fs::create_dir_all(run_dir.join("planner/agt/audit")).unwrap();
        PlannerAuditTail::spawn(&run_dir, None).stop();
        let _ = std::fs::remove_dir_all(&dir);
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
