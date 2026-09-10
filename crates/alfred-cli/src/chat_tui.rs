//! alfred chat TUI 呈现层（S1 骨架 + S2a 汇合 + S4 体验收口）。
//!
//! 定位（方案 `.plans/施工方案-TUI界面.md`）：治理内核零改动，本模块是
//! `alfred chat` 的终端呈现层。S2a 汇合后数据全部接线：
//!
//! - **线程模型**：治理会话（chat.rs [`chat_session`] 会话主体 + [`ChannelInput`]
//!   通道输入 + 事件 sink）跑在治理 worker 线程；本线程（主线程）跑 TUI 事件
//!   循环。通道双向：submit 行 → worker（治理路由复用 chat.rs 现有 parse——
//!   TUI 不重写任何治理语义）；治理输出 → [`ChatEvent`]（左列）+ run 目录
//!   （右列看板）。
//! - **左列"对话"**：[`ChatEvent`] 按变体渲染行（`try_recv` 非阻塞汇聚）——
//!   属主回声 / `[pi] ⋯` 过程动作（行格式单一真源 [`chat::pi_action_text`]）/
//!   `[pi]` 答复 / `[chat]` 状态行 / 挂起升级包 / 错误行。
//! - **右列"状态"看板**：周期 [`dashboard::snapshot`]（run 目录 → 快照）——
//!   治理态 / 计划节点 / 维护者 / 审查结论 / 参考卷 / 产物。
//! - **顶状态条**：run_id + 治理态实时（看板快照数据源）。
//! - **捕获管道**：TUI 期间进程 stdout/stderr 被重定向进捕获管道（libc dup2）
//!   ——drainer 线程过滤兜底：`[chat]`/`[pi]`/`[orchestrator]` 前缀行均已
//!   [`ChatEvent`] 事件化（透传=左列双显）丢弃，例外白名单
//!   `[orchestrator] warn:` 行透传（alfred-reviewer verdict 解析告警未事件化，
//!   S2 审 P2 / S3 修复，见 [`ORCHESTRATOR_WARN_PREFIX`]）；空行丢弃；其余
//!   （未事件化的残留输出）整串透传左列。governance.rs `[orchestrator]` 点位
//!   S2b 起事件化（TUI 模式不打终端），捕获透传路径退役。渲染流写 `/dev/tty`
//!   （与捕获流物理分离，画面不毁）。
//! - **多行输入自实现**（不引 tui-textarea：需求面只有字符/Tab 缩进/退格/
//!   回车提交/↑↓历史/Ctrl-J 换行，百行内可控且光标语义完全自明）。Tab
//!   插入 '\t'——数据层保真（对齐 REPL `sanitize_raw_line` 保留 \t：粘贴
//!   含缩进文本不失真），显示层在渲染边界定宽展开（[`expand_tabs`]：
//!   ratatui cell 不接受控制字符）。光标 = (行, char 列) 坐标——列按
//!   char 计（中文安全，字节换算集中 [`char_to_byte`]）；折行 CJK 宽度感知
//!   （unicode-width：ratatui/rustyline 传递依赖共 0.2.x 单副本，零新增
//!   编译重量），渲染与光标定位共用 [`wrap_segments`] 单一真源（'\t' 的
//!   折行/光标/展开宽同源 [`TAB_WIDTH`]）。
//! - **退出语义对齐 REPL**：Ctrl-D 空缓冲退出 / Ctrl-C 恒退出（rustyline
//!   时代 Eof/Interrupted → 会话结束，同语义；raw mode 下 Ctrl-C 不产生
//!   SIGINT，作为按键处理）；Ctrl-D 非空按行编辑惯例删光标处字符。空提交
//!   走治理路由（chat_session 单一真源回应，文案与 REPL 逐字对齐）。
//! - **降级门**：[`tui_supported`]（stdin&&stdout 双 tty 且 TERM 非空非
//!   dumb）——非 tty（管道/脚本）走既有 REPL 裸读路径，e2e 管道行为逐字节
//!   不变是硬底线。
//!
//! - **终端生命周期**：手动 init/restore（等价 [`ratatui::try_init`] 序列，
//!   但写端是 `/dev/tty` 而非 stdout——stdout 已被捕获重定向，走库函数会把
//!   备用屏恢复序列写进捕获管道恢复不了真终端）：raw mode + 备用屏 + panic
//!   hook 兜底（panic 冒泡前先恢复终端与 fd——panic 报文落真终端，属主终端
//!   不留坏状态）。正常/错误路径都先恢复再返回——含 init 半途失败各步骤的
//!   幂等补恢复；初始化失败由 cmd_chat 降级回 REPL（见 chat.rs 降级门）。
//! - **事件循环**：crossterm 同步 `poll(timeout)+read`（按键/resize 事件
//!   驱动）；draw 为 ratatui 双缓冲差分，无变化帧零输出。治理事件/捕获行/
//!   看板快照在每帧 poll 前非阻塞汇聚（[`TuiApp::pump`]）——汇聚-渲染-等键
//!   三拍循环，事件显示延迟 ≤ 一个 poll 窗口。resize 事件无需特判：下一轮
//!   draw 的 `Terminal::autoresize` 按新尺寸重排。
//! - **worker 不 join**：Ctrl-D/Ctrl-C 退出时会话可能仍在推进（converse 秒级
//!   返回），强等会吊死已恢复的终端；进程退出即收割，run 落盘状态可断点续跑
//!   （P3 语义，与 REPL 时代 Ctrl-C 同级）。
//!
//! - **S4 体验收口**：着色（治理态 [`state_color`]——状态条徽标 + 右列看板
//!   态行；左列事件按来源 [`LineVoice`] 着色：[pi] 答复青 / [orchestrator]
//!   白 / warn 黄 / Error 红 / 过程行暗灰——**REPL 面逐字节不变**，着色只
//!   在 TUI 渲染层）；滚动（左列 PgUp/PgDn 翻页 + End 回底；手动滚动视口
//!   锚定内容，新行到达不打断——底部"↓ 新增 N 行"提示）；前缀分职
//!   （[`ChatEvent::ShellNotice`]——chat.rs 会话壳通知渲染 `[chat]`，不再
//!   误标 `[orchestrator]`，与 REPL 逐面对齐）；折行 grapheme 原子性
//!   （ZWJ emoji 序列不被拆行）；光标行尾满宽落下一显示行首；0x0 pty
//!   （script/expect 类）降级 REPL（[`tui_supported`] 尺寸门）；终端恢复
//!   失败非致命（成功路径不回退 REPL——fd 仍在捕获会黑屏）；state.json
//!   原子写对冲"退出击杀 worker × 非原子写"撕裂窗口（governance.rs）。

use std::env;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};
use unicode_segmentation::UnicodeSegmentation;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::chat::pi_action_text;
use alfred_cli::chat_events::{ChatEvent, ChatEventBus, ChatEventReceiver};
use alfred_cli::dashboard::{self, DashboardSnapshot};

/// 事件轮询超时：无输入事件时静默等待（draw 差分零输出，不闪烁）；治理事件/
/// 捕获行/看板快照每帧 poll 前非阻塞汇聚（延迟 ≤ 本窗口）。
const POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// 看板快照轮询周期（run 目录全量读；事件到达置脏即触发，无事件兜底周期刷）。
const DASH_REFRESH: Duration = Duration::from_millis(1000);

/// 状态条底色（前景按内容着色：治理态徽标走 [`state_color`]，标题/提示白灰）。
const STATUS_BG: Color = Color::DarkGray;

/// Tab 显示宽：'\t' 渲染为固定 4 列空格缩进。不对齐 8 列终端 tab stop——
/// 软折行显示行无绝对列基准，定宽可预期；编辑器/历史/提交数据层恒保真
/// '\t'，仅显示层展开。折行（[`wrap_segments`]）、光标（`cursor_segment_pos`）、
/// 展开（[`expand_tabs`]）共用此单一真源。
const TAB_WIDTH: usize = 4;

/// TUI 终端（backend = `/dev/tty` 独占写端；进程 stdout/stderr 已被捕获重定向，
/// 渲染流与捕获流物理分离）。
type TuiTerminal = Terminal<CrosstermBackend<File>>;

/// TUI 降级门（cmd_chat 入口分流判定）：交互双 tty 且 TERM 有效且终端尺寸
/// 非零 → TUI；否则（管道/重定向/dumb 终端/TERM 缺失/0x0 pty）降级既有
/// REPL 路径。TERM/尺寸判定拆纯函数 [`term_enables_tui`]/
/// [`size_enables_tui`]（可测）。
///
/// 尺寸门（S1 审 P3，S4 落地）：script/expect 类工具起的 0x0 pty 下
/// stdin/stdout 双 tty、TERM 亦有效——尺寸检查缺席则 TUI 进零宽画面静默
/// 空白。crossterm `size()` 走 /dev/tty ioctl（TTYCOLS/TTYROWS=0 如实上报）
/// ——查询失败（无控制终端）视同不可用，不赌。
pub fn tui_supported() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && term_enables_tui(env::var("TERM").ok().as_deref())
        && matches!(size(), Ok((w, h)) if size_enables_tui(w, h))
}

/// TERM 判定：非空且非 `dumb`（dumb 无光标寻址，TUI 无处渲染）；未设置视
/// 同 dumb（POSIX 未定义行为的终端不赌）。
fn term_enables_tui(term: Option<&str>) -> bool {
    match term {
        Some(t) => !t.is_empty() && t != "dumb",
        None => false,
    }
}

/// 尺寸判定：宽高均非零（0x0 pty = script/expect 类驱动终端，TUI 无处渲染
/// ——降级 REPL 保持可用性，见 [`tui_supported`] 文档）。
fn size_enables_tui(w: u16, h: u16) -> bool {
    w > 0 && h > 0
}

/// TUI 主入口（cmd_chat 降级门调用）：捕获重定向 + 治理 worker + 终端生命周期
/// → 事件循环 → 恢复。错误路径（REPL 回退）恢复 fd 与终端；成功路径终端恢复
/// 后 fd 保持捕获直至进程退出（worker 残留输出绝不落真终端），告别行直写原
/// stdout。
pub fn run(run_dir_flag: Option<&Path>) -> Result<()> {
    // 通道双向：submit 行 → 治理 worker；治理事件/捕获残留行 → TUI 事件循环。
    let (input_tx, input_rx) = mpsc::channel::<String>();
    let (event_tx, event_rx) = ChatEventBus::new();
    let (capture_tx, capture_rx) = mpsc::channel::<String>();
    // run 目录共享位：worker 定位/新建 run 后发布，看板轮询取数据源。
    let run_dir_watch: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

    // panic hook 先装（fd 回位 + 终端恢复），捕获重定向随后——hook 覆盖重定向
    // 后的任何 panic 时点（panic 报文落真终端，不残留在捕获管道里）。
    let saved_fds: Arc<Mutex<Option<(RawFd, RawFd)>>> = Arc::new(Mutex::new(None));
    install_panic_hook(saved_fds.clone());
    let capture = CaptureRedirect::capture(&saved_fds, capture_tx)
        .context("TUI 输出捕获初始化失败")?;

    // 治理 worker：chat.rs 会话主体 + 通道输入 + 事件 sink（保留打印落捕获
    // 管道；Err 转错误事件，TUI 侧事件端 Disconnected 即知会话不可续）。
    let _worker = std::thread::Builder::new()
        .name("chat-governance".to_string())
        .spawn({
            let watch = Arc::clone(&run_dir_watch);
            let flag = run_dir_flag.map(Path::to_path_buf);
            move || crate::chat::run_tui_session(flag, input_rx, event_tx, watch)
        })
        .context("TUI 治理会话线程启动失败")?;

    let mut terminal = match init_terminal() {
        Ok(t) => t,
        Err(e) => {
            // init 半途失败：终端面已由 init_terminal 自恢复；fd 回位 + drainer
            // 收割——REPL 回退路径需要真 stdout。
            restore_fds_once(&saved_fds);
            capture.join();
            return Err(e);
        }
    };
    let mut app = TuiApp::new(input_tx, event_rx, capture_rx, run_dir_watch);
    let result = event_loop(&mut terminal, &mut app);

    // 恢复顺序：终端面先退（画面消失）→ 成功路径 fd 保持捕获、告别行直写原
    // stdout；错误路径 fd 回位（REPL 回退需要真 stdout）。
    //
    // 终端恢复失败非致命（S2 审 P3，S4 落地）：成功路径此前直接 `return Err`
    // ——cmd_chat 会回退 REPL，而此时 fd 仍在捕获（成功路径约定），REPL 全部
    // 输出进捕获管道 = 属主黑屏。改为兜底重试（[`restore_tty_only`] 重开
    // /dev/tty 写恢复序列，幂等）后照常告别返回：会话本身已正常结束，恢复
    // 失败只损失画面收尾，不构成回退 REPL 的理由。错误路径维持原 Err（回退
    // REPL 前 fd 已回位，错误如实上报）。
    if restore_terminal(&mut terminal).is_err() {
        restore_tty_only();
    }
    match result {
        Ok(()) => {
            farewell_line(&saved_fds);
            Ok(())
        }
        Err(e) => {
            drop(app); // input 端先断——worker 收 EOF 自然退出（不 join，见模块文档）
            restore_fds_once(&saved_fds);
            capture.join();
            Err(e)
        }
    }
}

/// 事件循环：汇聚（事件/捕获行/看板）→ draw（差分渲染）→ poll 键盘/resize →
/// 分发。鼠标/焦点/粘贴不消费（无对应交互面）。worker 退出（事件端
/// Disconnected）不终止循环——左列提示 + 输入停用，属主读完余量再关。
fn event_loop(terminal: &mut TuiTerminal, app: &mut TuiApp) -> Result<()> {
    while !app.exit {
        app.pump();
        terminal.draw(|f| ui(f, app)).context("TUI 渲染失败")?;
        if !event::poll(POLL_TIMEOUT).context("TUI 事件轮询失败")? {
            continue;
        }
        match event::read().context("TUI 事件读取失败")? {
            Event::Key(key) => app.handle_key(key),
            // resize 由下一轮 draw 按新尺寸重排兜底；其余事件不消费。
            _ => {}
        }
    }
    Ok(())
}

// ── 终端生命周期（/dev/tty 手动 init/restore，见模块文档） ──

/// 终端初始化：raw mode + 备用屏（写 `/dev/tty`）+ backend。半途失败逐步
/// 幂等补恢复（raw mode 已开则退；未开时 disable 是 no-op），Err 交 cmd_chat
/// 降级 REPL。
fn init_terminal() -> Result<TuiTerminal> {
    enable_raw_mode().context("TUI raw mode 开启失败")?;
    let tty = match open_tty() {
        Ok(f) => f,
        Err(e) => {
            let _ = disable_raw_mode();
            return Err(e).context("TUI 打开 /dev/tty 失败");
        }
    };
    // 先建 terminal（backend=`/dev/tty` 写端），备用屏序列经 backend 写——
    // `File` 同时实现 Read/Write（execute! 的 by_ref 推断歧义），backend 只
    // 实现 Write 无歧义。
    let mut terminal = match Terminal::new(CrosstermBackend::new(tty)) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            return Err(e).context("TUI 终端创建失败");
        }
    };
    if let Err(e) = execute!(terminal.backend_mut(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e).context("TUI 备用屏进入失败");
    }
    Ok(terminal)
}

/// 终端恢复：raw mode 退 + 备用屏退。`LeaveAlternateScreen` 经 terminal 自带
/// backend（`/dev/tty`）写回——进程 stdout 已被捕获，走 `ratatui::try_restore`
/// 会把恢复序列写进捕获管道恢复不了真终端。
fn restore_terminal(terminal: &mut TuiTerminal) -> Result<()> {
    disable_raw_mode().context("TUI raw mode 退出失败")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("TUI 备用屏退出失败")?;
    Ok(())
}

/// 打开控制终端（backend 写端；resize 查询走 crossterm 内部 tty fd）。
fn open_tty() -> io::Result<File> {
    std::fs::OpenOptions::new().write(true).open("/dev/tty")
}

/// 终端面兜底恢复（raw mode 退 + 备用屏退）：init 半途失败与 panic hook 共用
/// （后者拿不到 terminal，重开 `/dev/tty` 写恢复序列；未进备用屏时终端忽略
/// 该序列，幂等）。`Box<dyn Write>` 只实现 Write——绕开 `File` 的
/// Read/Write 双实现给 execute! 带来的 by_ref 推断歧义。
fn restore_tty_only() {
    let _ = disable_raw_mode();
    if let Ok(tty) = open_tty() {
        let mut tty: Box<dyn std::io::Write> = Box::new(tty);
        let _ = execute!(tty, LeaveAlternateScreen);
    }
}

/// panic hook：fd 回位 + 终端恢复后链回原 hook——panic 报文（stderr）落真
/// 终端，属主可见；终端不留 raw mode/备用屏坏状态。
fn install_panic_hook(saved_fds: Arc<Mutex<Option<(RawFd, RawFd)>>>) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_fds_once(&saved_fds);
        restore_tty_only();
        prev(info);
    }));
}

// ── 进程级 stdout/stderr 捕获重定向（libc dup2） ──

/// fd 原始位（1/2）回位：take 语义幂等——首个恢复者（正常路径/错误路径/panic
/// hook 并发）回位并关 fd，后来者见 None 直接跳过（防 close 复用 fd 号误伤）。
fn restore_fds_once(saved: &Arc<Mutex<Option<(RawFd, RawFd)>>>) {
    let Some((out, err)) = saved.lock().take() else { return };
    unsafe {
        libc::dup2(out, 1);
        libc::dup2(err, 2);
        libc::close(out);
        libc::close(err);
    }
}

/// 告别行直写原 stdout（dup 共享 fd，不回位全局 fd 1/2）：成功路径 fd 保持
/// 捕获直至进程退出——治理 worker 存活期间的残留输出（会话结束尾行/收尾
/// 打印）继续落捕获管道，绝不在画面退出后落真终端。
fn farewell_line(saved: &Arc<Mutex<Option<(RawFd, RawFd)>>>) {
    let guard = saved.lock();
    let Some((out, _)) = *guard else { return };
    drop(guard);
    unsafe {
        let dup = libc::dup(out);
        if dup >= 0 {
            let mut f = File::from_raw_fd(dup);
            let _ = writeln!(f, "[chat] 会话结束。");
        }
    }
}

/// 捕获重定向：`dup2` 管道写端覆到 fd 1/2（原 fd 经 `dup` 留底给回位/告别），
/// drainer 线程逐行读管道——`[chat]`/`[pi]`/`[orchestrator]` 前缀行已由
/// [`ChatEvent`] 事件化（透传=左列双显）丢弃（例外白名单
/// [`ORCHESTRATOR_WARN_PREFIX`] 透传，见 [`forward_line`]）；其余（未事件化的
/// 残留输出）整串透传左列。governance.rs `[orchestrator]` 点位 S2b 事件化后
/// 不再进管道（TUI 模式不打终端），本通道只剩残留兜底 + reviewer warn 白名单。
///
/// 退出协议：fd 1/2 是管道唯一写端（`dup2` 后关原写端 fd）——回位后管道
/// EOF，drainer 自然退；捕获线程绝不写坏画面（读端独立 fd）。
struct CaptureRedirect {
    drainer: std::thread::JoinHandle<()>,
}

impl CaptureRedirect {
    /// 重定向 fd 1/2 → 捕获管道 + 起 drainer。失败路径闭环恢复（不留半重定向）。
    fn capture(
        saved: &Arc<Mutex<Option<(RawFd, RawFd)>>>,
        forward: mpsc::Sender<String>,
    ) -> Result<Self> {
        unsafe {
            let mut fds = [0 as libc::c_int; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error()).context("TUI 捕获管道创建失败");
            }
            let (r, w) = (fds[0], fds[1]);
            let dup_fd = |fd: RawFd| -> std::result::Result<RawFd, io::Error> {
                let d = libc::dup(fd);
                if d < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(d)
                }
            };
            // 原始位先留底，再 dup2 覆盖——任何失败不留半重定向。
            let (saved_out, saved_err) = match (dup_fd(1), dup_fd(2)) {
                (Ok(o), Ok(e)) => (o, e),
                pair => {
                    if let Ok(d) = pair.0 {
                        libc::close(d);
                    }
                    if let Ok(d) = pair.1 {
                        libc::close(d);
                    }
                    libc::close(r);
                    libc::close(w);
                    return Err(io::Error::last_os_error()).context("TUI 捕获重定向失败（dup 原始 fd）");
                }
            };
            if libc::dup2(w, 1) < 0 || libc::dup2(w, 2) < 0 {
                let e = io::Error::last_os_error();
                libc::dup2(saved_out, 1);
                libc::dup2(saved_err, 2);
                libc::close(saved_out);
                libc::close(saved_err);
                libc::close(r);
                libc::close(w);
                return Err(e).context("TUI 捕获重定向失败（dup2）");
            }
            libc::close(w); // fd 1/2 是唯一写端——回位即 EOF，drainer 自退
            *saved.lock() = Some((saved_out, saved_err));
            let read_end = File::from_raw_fd(r);
            let drainer = match std::thread::Builder::new()
                .name("tui-stdout-capture".to_string())
                .spawn(move || drain_capture(read_end, forward))
            {
                Ok(h) => h,
                Err(e) => {
                    // drainer 起不来 → 管道无读端，后续 println 会 EPIPE panic
                    // 治理线程——回位放弃捕获，不留半重定向。
                    restore_fds_once(saved);
                    return Err(e).context("TUI 捕获 drainer 线程启动失败");
                }
            };
            Ok(Self { drainer })
        }
    }

    /// 收割 drainer（调用方先回位 fd → 管道 EOF → 线程自退）。
    fn join(self) {
        let _ = self.drainer.join();
    }
}

/// drainer 主体：阻塞读管道 → 剥完整行 → 过滤转发（[`forward_line`]）；EOF
/// 后补发残缺尾行。任何读错（EINTR 重试，其余）退出——捕获是呈现增强面，
/// 不崩不吊。
fn drain_capture(mut f: File, forward: mpsc::Sender<String>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match f.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&pending[..nl]).into_owned();
                    pending.drain(..=nl);
                    forward_line(&forward, &line);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        forward_line(&forward, &String::from_utf8_lossy(&pending).into_owned());
    }
}

/// 捕获白名单前缀（S2 审 P2，S3 修复）：alfred-reviewer 两处 verdict 解析告警
/// （exec_review.rs / plan_review.rs 的 `eprintln!`，stderr → 捕获管道）未事件化
/// （reviewer crate 无事件通道，完整事件化留后续切片）——整体 `[orchestrator]`
/// 丢弃规则会让审查 verdict 解析失败时属主在 TUI 看不到根因（REPL 面恒可见，
/// 构成可见性回退），故白名单放行透传左列。governance.rs `[orchestrator]` 点位
/// S2b 起 TUI 模式不打终端（事件化），该前缀行只可能来自捕获管道——无双显。
const ORCHESTRATOR_WARN_PREFIX: &str = "[orchestrator] warn:";

/// 捕获行过滤转发（纯函数，可测）：`[chat] `/`[pi] `/`[orchestrator] ` 前缀行
/// 已由 [`ChatEvent`] 事件化覆盖 → 丢弃（透传=左列双显）；**例外白名单**
/// （S2 审 P2）：[`ORCHESTRATOR_WARN_PREFIX`] 前缀行透传（alfred-reviewer
/// verdict 解析告警未事件化，丢弃=审查失败时属主看不到根因）；空行不透传
/// （噪音）；其余（未事件化的残留输出）整串透传。
fn forward_line(tx: &mpsc::Sender<String>, line: &str) {
    let line = line.trim_end_matches('\r');
    if line.is_empty()
        || line.starts_with("[chat] ")
        || line.starts_with("[pi] ")
        || (line.starts_with("[orchestrator] ") && !line.starts_with(ORCHESTRATOR_WARN_PREFIX))
    {
        return;
    }
    let _ = tx.send(line.to_string());
}

// ── 应用状态 ──

/// 左列行来源（前缀分职 + 着色单一真源，S4）：事件变体 → 前缀 + 颜色的
/// 唯一映射。着色只作用于 TUI 面（REPL 逐字节不变是硬底线——REPL 路径
/// 不经本模块）；色板口径：[pi] 答复青 / [orchestrator] 白 / warn 黄 /
/// Error 红，过程行（[pi] ⋯/捕获残留）暗灰退居次要，挂起拍板意见黄
/// （需属主行动）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineVoice {
    /// 属主回声（`你: …`）。
    Owner,
    /// `[pi] ⋯` 过程动作行（次要——暗灰）。
    PiAction,
    /// `[pi]` 答复（青）。
    PiReply,
    /// `[chat]` 会话壳状态行（S4 前缀分职：ShellNotice 专属——灰）。
    Chat,
    /// `[chat]` 挂起拍板/升级意见（需属主行动——黄）。
    Escalation,
    /// `[orchestrator]` 治理环通知（白）。
    Orchestrator,
    /// `[orchestrator] warn:` 白名单透传（黄）。
    Warn,
    /// `[chat]` 错误行（红）。
    Error,
    /// 未事件化捕获残留（次要——暗灰）。
    Plain,
}

impl LineVoice {
    /// 行颜色（着色单一真源，测试锁定）。
    fn color(self) -> Color {
        match self {
            LineVoice::Owner => Color::White,
            LineVoice::PiAction => Color::DarkGray,
            LineVoice::PiReply => Color::Cyan,
            LineVoice::Chat => Color::Gray,
            LineVoice::Escalation => Color::Yellow,
            LineVoice::Orchestrator => Color::White,
            LineVoice::Warn => Color::Yellow,
            LineVoice::Error => Color::Red,
            LineVoice::Plain => Color::DarkGray,
        }
    }
}

/// 左列对话流条目：正文 + 来源（渲染按来源加前缀/着色；数据层只存原文）。
#[derive(Debug, Clone, PartialEq)]
struct ConvLine {
    text: String,
    voice: LineVoice,
}

/// 治理态着色（S4，状态条徽标 + 右列看板态行共用单一真源）：planning 黄 /
/// reviewing（计划/执行审查）蓝 / executing 绿 / 挂起（plan_rejected/
/// escalated）红 / completed 亮绿 / abandoned 灰（终态中性）/ 未知白。
/// 态值词表与 [`input_hint`] 同源（state.json 投影）。
fn state_color(state: &str) -> Color {
    match state {
        "planning" => Color::Yellow,
        "plan_reviewing" | "exec_reviewing" => Color::Blue,
        "executing" => Color::Green,
        "plan_rejected" | "escalated" => Color::Red,
        "completed" => Color::LightGreen,
        "abandoned" => Color::Gray,
        _ => Color::White,
    }
}

/// TUI 应用状态（S2a 汇合 + S2b 治理事件化 + S4 着色/滚动）：输入编辑器 +
/// 左列对话流（事件渲染行/捕获残留行/属主回声，来源分职着色 + PgUp/PgDn
/// 回溯）+ 右列看板快照 + 治理通道两端。
struct TuiApp {
    input: InputEditor,
    /// 左列对话流（一事件一条目；跟随底部，PgUp/PgDn/End 滚动见
    /// [`TuiApp::scroll_page`]）。
    messages: Vec<ConvLine>,
    exit: bool,
    /// 治理会话已退出（事件端 Disconnected——worker Err 退场）：输入停用，
    /// 状态条提示，属主读完余量再退出。
    session_ended: bool,
    /// 右列看板快照（周期/置脏时从 run 目录聚合；Err 保持上次——宁缺勿错）。
    dashboard: Option<DashboardSnapshot>,
    dash_dirty: bool,
    dash_last: Instant,
    /// 治理 worker 输入端（submit 路由：行原文整发，parse/决策在 chat_session
    /// 单一真源）。
    worker: mpsc::Sender<String>,
    /// 治理事件端（try_recv 非阻塞消费）。
    events: ChatEventReceiver,
    /// 捕获残留行端（drainer 线程 → 左列；未事件化的残留输出）。
    captured: mpsc::Receiver<String>,
    /// run 目录共享位（worker 发布，看板取数据源）。
    run_dir: Arc<Mutex<Option<PathBuf>>>,
    /// 左列滚动位（S4）：`None` = 跟随底部（新内容恒可见）；`Some(top)` =
    /// 手动滚动（视口首行 = 显示行绝对序号——新行到达只追加在下方，视口
    /// 锚定内容不打断历史区阅读）。
    conv_top: Option<usize>,
    /// 手动滚动期间新到达的显示行数（底部提示"↓ 新增 N 行"；回底清零）。
    new_below: usize,
    /// 左列几何缓存（上帧渲染尺寸：折行宽/可视高——滚动键按可视高分页、
    /// 新行计数按折行宽折算；首帧前 (0,0) 不可滚动）。
    conv_geom: (usize, usize),
}

impl TuiApp {
    fn new(
        worker: mpsc::Sender<String>,
        events: ChatEventReceiver,
        captured: mpsc::Receiver<String>,
        run_dir: Arc<Mutex<Option<PathBuf>>>,
    ) -> Self {
        Self {
            input: InputEditor::new(),
            messages: Vec::new(),
            exit: false,
            session_ended: false,
            dashboard: None,
            // 首帧即刷（run 目录可能已由 worker 秒级发布）。
            dash_dirty: true,
            dash_last: Instant::now(),
            worker,
            events,
            captured,
            run_dir,
            conv_top: None,
            new_below: 0,
            conv_geom: (0, 0),
        }
    }

    /// 每帧 poll 前的数据汇聚（全部非阻塞）：捕获残留行 → 治理事件（FIFO
    /// 取尽；Disconnected = worker 已退）→ 看板快照（置脏或周期）。
    fn pump(&mut self) {
        while let Ok(line) = self.captured.try_recv() {
            // warn 白名单行（S3）黄色警示；其余残留暗灰退居次要。
            let voice = if line.starts_with(ORCHESTRATOR_WARN_PREFIX) {
                LineVoice::Warn
            } else {
                LineVoice::Plain
            };
            self.push_line(ConvLine { text: line, voice });
        }
        loop {
            match self.events.try_recv() {
                Ok(ev) => self.apply_event(ev),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.session_ended {
                        self.session_ended = true;
                        self.push_line(ConvLine {
                            text: "[chat] 治理会话已退出（Ctrl-D/Ctrl-C 关闭界面）。".into(),
                            voice: LineVoice::Chat,
                        });
                    }
                    break;
                }
            }
        }
        self.refresh_dashboard();
    }

    /// 左列消息入列单一入口（S4 滚动）：跟随态直接入列；手动滚动态视口
    /// 锚定内容（`conv_top` 不动），新行折算显示行数计入 [`Self::new_below`]
    /// （底部"↓ 新增 N 行"提示——不打断历史区阅读）。
    fn push_line(&mut self, line: ConvLine) {
        if self.conv_top.is_some() {
            self.new_below += display_row_count(&line.text, self.conv_geom.0);
        }
        self.messages.push(line);
    }

    /// 左列全部显示行数（滚动窗口/分页计算用；折行与渲染同一真源
    /// [`wrap_segments`]）。
    fn total_conv_rows(&self, width: usize) -> usize {
        self.messages.iter().map(|m| display_row_count(&m.text, width)).sum()
    }

    /// 左列视口（渲染入口，每帧调用）：跟随态取底部窗口；手动态锚定
    /// `conv_top`，越界（resize 后内容不足一屏）自动回跟随。缓存几何
    /// （`conv_geom`）供滚动键分页/新行计数。返回视口显示行。
    fn conv_viewport(&mut self, width: usize, height: usize) -> Vec<(String, LineVoice)> {
        let total = self.total_conv_rows(width);
        if let Some(t) = self.conv_top {
            if t >= total.saturating_sub(height) {
                self.follow_bottom();
            }
        }
        self.conv_geom = (width, height);
        conversation_rows(&self.messages, width, height, self.conv_top)
    }

    /// 左列滚动键分页（S4）：PgUp 上翻一页（进入手动滚动——视口锚定内容，
    /// 新行不打断）；PgDn 下翻一页（触底回跟随）。页大小 = 上帧左列可视高
    /// （首帧前无几何不可滚动）；内容不足一屏无处可滚。
    fn scroll_page(&mut self, up: bool) {
        let (w, h) = self.conv_geom;
        if w == 0 || h == 0 {
            return;
        }
        let total = self.total_conv_rows(w);
        if total <= h {
            return;
        }
        let bottom_start = total - h;
        match (up, self.conv_top) {
            (true, None) => self.conv_top = Some(bottom_start.saturating_sub(h)),
            (true, Some(t)) => self.conv_top = Some(t.saturating_sub(h)),
            (false, None) => {} // 已在底部
            (false, Some(t)) => {
                let t = t + h;
                if t + h >= total {
                    self.follow_bottom();
                } else {
                    self.conv_top = Some(t);
                }
            }
        }
    }

    /// 回底跟随：清滚动位 + 新行计数（提示随消）。
    fn follow_bottom(&mut self) {
        self.conv_top = None;
        self.new_below = 0;
    }

    /// 看板快照刷新：run 目录未发布（worker 定位中）跳过；置脏或周期到点才
    /// 真读（run 目录全量读，事件风暴不放大成 fs 风暴）；Err（目录未建/写中）
    /// 保持上次快照下轮再试。
    fn refresh_dashboard(&mut self) {
        let Some(dir) = self.run_dir.lock().clone() else {
            return;
        };
        if !self.dash_dirty && self.dash_last.elapsed() < DASH_REFRESH {
            return;
        }
        self.dash_last = Instant::now();
        self.dash_dirty = false;
        if let Ok(snap) = dashboard::snapshot(&dir) {
            self.dashboard = Some(snap);
        }
    }
    /// 治理事件 → 左列条目（按变体渲染行 + 来源；行格式与 REPL 逐字节同面
    /// ——前缀分职见 [`LineVoice`]：ShellNotice 渲染 `[chat]`（S4 前缀分职
    /// 修复：此前 chat.rs 会话壳通知误标 `[orchestrator]`），OrchestratorNotice
    /// 渲染 `[orchestrator]`，与 REPL 两面前缀逐面对齐）。挂起/错误类事件
    /// 置脏看板（态/verdict 可能已变）。
    fn apply_event(&mut self, ev: ChatEvent) {
        let stateful = !matches!(ev, ChatEvent::OwnerEcho(_) | ChatEvent::PiAction { .. });
        let (text, voice) = match ev {
            ChatEvent::OwnerEcho(text) => (format!("你: {text}"), LineVoice::Owner),
            ChatEvent::PiAction { kind, detail } => (
                format!("[pi] ⋯ {}", pi_action_text(kind, &detail)),
                LineVoice::PiAction,
            ),
            ChatEvent::PiReply(text) => (format!("[pi] {text}"), LineVoice::PiReply),
            ChatEvent::ShellNotice(text) => (format!("[chat] {text}"), LineVoice::Chat),
            ChatEvent::OrchestratorNotice(text) => {
                (format!("[orchestrator] {text}"), LineVoice::Orchestrator)
            }
            ChatEvent::EscalationPrompt { reason, source } => {
                let _ = source; // S2a 正文即含来源（REPL 同文）；着色按变体不按来源
                (format!("[chat] {reason}"), LineVoice::Escalation)
            }
            ChatEvent::Error { message, stream } => {
                let _ = stream; // REPL 按流写回原点位；TUI 单列统一呈现
                (format!("[chat] {message}"), LineVoice::Error)
            }
        };
        if stateful {
            self.dash_dirty = true;
        }
        self.push_line(ConvLine { text, voice });
    }

    /// 键 → 状态转移。raw mode 下 Ctrl-C/Ctrl-D 以按键到达（无 SIGINT）：
    /// Ctrl-C 恒退出、Ctrl-D 空缓冲退出（对齐 REPL 会话结束语义）；Enter
    /// 恒为提交（治理消息边界，路由治理 worker）；Ctrl-J 换行（LF 键序在 raw
    /// mode 下即 Ctrl-J，与 Enter 的 CR 区分）；Tab 插入 '\t'（数据层保真，
    /// 对齐 REPL sanitize_raw_line 保留 \t——粘贴缩进不失真；显示层展开见
    /// [`expand_tabs`]）；PgUp/PgDn 左列翻页、End 回底（S4 滚动——End 仅在
    /// 手动滚动态承担回底，跟随态仍是输入行行尾：滚动态有"End 回底"提示
    /// 在场，无歧义）；其余可见字符进编辑器。只处理按下/自动重复
    /// （Windows 终端按下与释放都发事件）。
    fn handle_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.exit = true;
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.input.is_empty() {
                    self.exit = true;
                } else {
                    self.input.delete_forward();
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.insert_newline();
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete_forward(),
            KeyCode::Up => self.input.up(),
            KeyCode::Down => self.input.down(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::PageUp => self.scroll_page(true),
            KeyCode::PageDown => self.scroll_page(false),
            // End 双职（S4）：手动滚动态回底（提示"End 回底"在场）；跟随态
            // 输入行行尾（编辑惯例——两种状态各有明确在场提示，不混义）。
            KeyCode::End if self.conv_top.is_some() => self.follow_bottom(),
            KeyCode::End => self.input.end(),
            KeyCode::Tab => self.input.insert_char('\t'),
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert_char(c);
            }
            _ => {}
        }
    }

    /// 提交 → 治理路由：行原文整发 worker（决策 parse/空行回应全在
    /// chat_session 单一真源——文案与 REPL 逐字对齐，TUI 不重写治理语义）；
    /// 非空行同时回声左列（[`ChatEvent::OwnerEcho`]——REPL 由终端原生回显
    /// 承担，TUI 无此机制故补此点位）。会话已退出：清缓冲即止，不再投递。
    fn submit(&mut self) {
        let text = self.input.submit();
        if self.session_ended {
            return;
        }
        if !text.trim().is_empty() {
            self.apply_event(ChatEvent::OwnerEcho(text.clone()));
        }
        // send 失败 = worker 已退（事件端 Disconnected 下一拍置 session_ended）。
        let _ = self.worker.send(text);
    }
}

/// 多行输入编辑器。光标 = (逻辑行, 行内 char 列)——列按 char 计，中文安全
/// （字节换算集中 [`char_to_byte`]）。历史进程内 Vec 暂存（对齐 rustyline
/// MemHistory 语义：不持久化文件、非空才进历史），draft 兜底历史浏览中的
/// 未提交草稿（↓ 越过最新时恢复，readline 惯例）。
struct InputEditor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    history: Vec<String>,
    /// 历史浏览位置（Some(i) = 正在看 history[i]；None = 新草稿）。
    browsing: Option<usize>,
    /// 进入历史浏览前的草稿。
    draft: Option<String>,
}

impl InputEditor {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            browsing: None,
            draft: None,
        }
    }

    /// 缓冲全空（Ctrl-D 退出判据）。
    fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    /// 缓冲全文（多行 join；提交/历史条目统一形态）。
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.row];
        line.insert(char_to_byte(line, self.col), c);
        self.col += 1;
    }

    /// 光标处断行（Ctrl-J）：后半成为新行，光标落新行行首。
    fn insert_newline(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// 退格：行内删光标前一 char；行首则并入上一行（光标停接缝处）。
    fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let start = char_to_byte(line, self.col - 1);
            let end = char_to_byte(line, self.col);
            line.replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&tail);
        }
    }

    /// 前向删除（Delete 键 / Ctrl-D 非空）：行内删光标处 char；行尾则并
    /// 入下一行。
    fn delete_forward(&mut self) {
        let line_len = self.lines[self.row].chars().count();
        if self.col < line_len {
            let line = &mut self.lines[self.row];
            let start = char_to_byte(line, self.col);
            let end = char_to_byte(line, self.col + 1);
            line.replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
        }
    }

    /// ↑：首行时向更早历史（readline 惯例），否则行间上移（列夹取目标行
    /// 长度）。
    fn up(&mut self) {
        if self.row == 0 {
            self.history_prev();
        } else {
            self.row -= 1;
            self.clamp_col();
        }
    }

    /// ↓：末行时向更新历史，否则行间下移。
    fn down(&mut self) {
        if self.row + 1 == self.lines.len() {
            self.history_next();
        } else {
            self.row += 1;
            self.clamp_col();
        }
    }

    /// ←：行内左移；行首跳上一行行尾。
    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    /// →：行内右移；行尾跳下一行行首。
    fn right(&mut self) {
        if self.col < self.lines[self.row].chars().count() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn home(&mut self) {
        self.col = 0;
    }

    fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(self.lines[self.row].chars().count());
    }

    /// 提交：返回缓冲全文，非空才进历史（对齐 rustyline add_history_entry
    /// 非空判据）；编辑器复位为单空行草稿。
    fn submit(&mut self) -> String {
        let text = self.text();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        self.reset();
        text
    }

    fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.browsing = None;
        self.draft = None;
    }

    /// ↑（首行）：向更早历史；首次进入保存当前草稿。
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.browsing {
            None => {
                self.draft = Some(self.text());
                self.browsing = Some(self.history.len() - 1);
            }
            Some(0) => return, // 已是最早
            Some(i) => self.browsing = Some(i - 1),
        }
        let entry = self.history[self.browsing.expect("just set")].clone();
        self.load(entry);
    }

    /// ↓（末行）：向更新历史；越过最新恢复草稿。
    fn history_next(&mut self) {
        let Some(i) = self.browsing else { return };
        if i + 1 < self.history.len() {
            self.browsing = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.load(entry);
        } else {
            self.browsing = None;
            let draft = self.draft.take().unwrap_or_default();
            self.load(draft);
        }
    }

    /// 载入整段文本为缓冲（历史条目含显式换行）并置光标于末尾。
    fn load(&mut self, text: String) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }
}

/// char 列偏移 → 字节偏移（col 越界按行尾；编辑器光标统一 char 语义，唯一
/// 的字节换算点）。
fn char_to_byte(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

/// 字符显示宽（单一真源）：'\t' = [`TAB_WIDTH`]（定宽缩进，见 const 文档）；
/// 其余按 unicode-width，控制字符 0（编辑器只可能进 '\t'，防御兜底）。
fn display_width(c: char) -> usize {
    if c == '\t' {
        TAB_WIDTH
    } else {
        c.width().unwrap_or(0)
    }
}

/// 渲染边界展开：段文本 '\t' → [`TAB_WIDTH`] 空格。ratatui cell 不接受控制
/// 字符——零宽 '\t' 会附着进前一 cell 的 symbol 原样写给终端（终端按 tab
/// stop 跳列，整帧错位），纯 '\t' 行更因行宽 0 整行不渲染。展开只发生在
/// 段文本进入 ratatui 的最后一步（输入区/左列两个渲染边界共用）；编辑器/
/// 历史/提交数据层恒保真 '\t'。
fn expand_tabs(s: &str) -> String {
    if s.contains('\t') {
        s.replace('\t', &" ".repeat(TAB_WIDTH))
    } else {
        s.to_string()
    }
}

/// 软折行（CJK 宽度感知 + grapheme 原子性，S4）：一条逻辑行按显示宽
/// `width` 折成显示段序列，每段 (段首 char 偏移, 段文本)。段文本保真原文
/// （含 '\t'——光标定位按原 char 计，展开是渲染边界 [`expand_tabs`] 的
/// 事）；显示宽走 [`display_width`] 单一真源（'\t' = [`TAB_WIDTH`]）。
///
/// **grapheme 迭代**（S1 审 P3，S4 落地）：按 grapheme cluster 而非 char
/// 折段——ZWJ emoji 序列（👨‍👩‍👧‍👦）、肤色修饰（👍🏽）、旗帜（🇨🇳）是用户
/// 感知的单个字符，char 迭代会把序列拆到两行 = 视觉碎裂。cluster 宽 =
/// 成员 char 宽求和（'\t' 真源同源）。宽度 0（防御：极小终端边框内宽为
/// 0）不折行单段返回；单 cluster 宽 > 总宽（极小终端放 CJK/Tab/emoji）
/// 不可再分——独占一段。
fn wrap_segments(text: &str, width: usize) -> Vec<(usize, String)> {
    if width == 0 {
        return vec![(0, text.to_string())];
    }
    let mut segs = Vec::new();
    let (mut seg, mut start, mut w) = (String::new(), 0usize, 0usize);
    let mut char_off = 0usize; // 段首 char 偏移追踪（cluster 含多 char）
    for g in text.graphemes(true) {
        let cw: usize = g.chars().map(display_width).sum();
        if !seg.is_empty() && w + cw > width {
            segs.push((start, std::mem::take(&mut seg)));
            start = char_off;
            w = 0;
        }
        seg.push_str(g);
        w += cw;
        char_off += g.chars().count();
    }
    segs.push((start, seg));
    segs
}

/// 光标 char 列 → 显示坐标（显示段序号, 段内显示宽）。折行边界上的光标归
/// 下一段段首（光标行 = 下一个将输入字符所在行，视觉直觉）；行尾归末段末。
fn cursor_segment_pos(segs: &[(usize, String)], col: usize) -> (usize, usize) {
    for (ri, (start, seg)) in segs.iter().enumerate() {
        let len = seg.chars().count();
        if col < start + len || ri + 1 == segs.len() {
            let off = col.saturating_sub(*start).min(len);
            let w: usize = seg.chars().take(off).map(display_width).sum();
            return (ri, w);
        }
    }
    (0, 0) // wrap_segments 恒返回非空，不可达
}

/// 输入区显示布局：全部逻辑行折行后的显示行 + 光标显示坐标（显示行号, 行内
/// 显示宽）。输入框内容宽（= 框宽 - 2 边框）确定后一次算清，渲染与光标定位
/// 共用（同一折行真源，无两套口径）。
struct InputDisplay {
    rows: Vec<String>,
    /// 光标（显示行号, 行内显示宽）。
    cursor: (usize, usize),
}

fn input_display(editor: &InputEditor, width: usize) -> InputDisplay {
    let mut rows = Vec::new();
    let mut cursor = (0, 0);
    for (li, line) in editor.lines.iter().enumerate() {
        let segs = wrap_segments(line, width);
        let mut seg_rows: Vec<String> = segs.iter().map(|(_, s)| expand_tabs(s)).collect();
        if li == editor.row {
            let (ri, w) = cursor_segment_pos(&segs, editor.col);
            // 行尾恰在满宽末段（S1 审 P3，S4 落地）：下一个将输入的字符必落
            // 下一显示行（同输入再敲一字即折行），光标随之——补一空续行承载，
            // 不再被 ui 的列 clamp 压回本行末列（压在末字符上，视觉错位）。
            if w >= width && ri + 1 == seg_rows.len() {
                seg_rows.push(String::new());
                cursor = (rows.len() + ri + 1, 0);
            } else {
                cursor = (rows.len() + ri, w);
            }
        }
        rows.extend(seg_rows);
    }
    InputDisplay { rows, cursor }
}

/// 一条消息的左列显示行数（滚动分页/新行计数用；折行与渲染同一真源
/// [`wrap_segments`]，宽度 0 时每逻辑行单段）。
fn display_row_count(text: &str, width: usize) -> usize {
    text.split('\n').map(|l| wrap_segments(l, width).len()).sum()
}

/// 左列显示行（对话流，S4 滚动）：全部消息按内宽折行成显示行序列（每行
/// 带来源——着色是渲染关注点，数据层只存原文），视口二选一：`top = None`
/// 跟随底部（末 `height` 行，新内容恒可见）；`top = Some(t)` 手动滚动
/// （`[t, t+height)` 窗口——新行到达不移动视口，见 [`TuiApp::push_line`]）。
/// `t` 越界夹取到末行（防御：resize/直接调用）。
fn conversation_rows(
    messages: &[ConvLine],
    width: usize,
    height: usize,
    top: Option<usize>,
) -> Vec<(String, LineVoice)> {
    let mut rows: Vec<(String, LineVoice)> = Vec::new();
    for msg in messages {
        for line in msg.text.split('\n') {
            let voice = msg.voice;
            rows.extend(
                wrap_segments(line, width)
                    .into_iter()
                    .map(|(_, s)| (expand_tabs(&s), voice)),
            );
        }
    }
    let (start, end) = match top {
        None => (rows.len().saturating_sub(height), rows.len()),
        Some(t) => {
            let start = t.min(rows.len().saturating_sub(1));
            (start, (start + height).min(rows.len()))
        }
    };
    rows[start..end].to_vec()
}

/// 看板行宽适配：超宽截断带省略号（按显示宽——CJK/ASCII 混排对齐；字符宽走
/// [`display_width`] 真源）。宽度 0 返回空串；1 且超宽返回省略号。
fn fit_width(s: &str, width: usize) -> String {
    let total: usize = s.chars().map(display_width).sum();
    if total <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = display_width(c);
        if w + cw > width - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    if out.is_empty() {
        return "…".to_string();
    }
    out.push('…');
    out
}

/// 输入区提示（挂起拍板引导，S3 拍板交互的 S2a 落点）：由看板治理态派生
/// （覆盖表排除项——REPL prompt 留在 chat.rs，TUI 输入区提示从 state.json
/// 投影派生，两处不共线）。态值与 [`crate::chat`] 治理态标签同一词表。
fn input_hint(state: Option<&str>) -> &'static str {
    match state {
        Some("planning") => "对 pi 说",
        Some("plan_rejected") | Some("escalated") => "回复：重试 / 放弃 / 或直接说修改意见",
        Some("plan_reviewing") | Some("executing") | Some("exec_reviewing") => {
            "治理推进中（输入将排队）"
        }
        Some("completed") | Some("abandoned") => "新需求",
        _ => "需求",
    }
}

/// 右列看板行（[`DashboardSnapshot`] → 渲染行；每行 [`fit_width`] 适配内宽，
/// 超出可视高的行被 Paragraph 裁掉）。段序：治理环（态/计划节点/维护者/
/// 审查结论/参考卷）→ 产物清单。
fn dashboard_lines(snap: &DashboardSnapshot, width: usize) -> Vec<String> {
    let fit = |s: String| fit_width(&s, width);
    let mut rows = Vec::new();
    let state = if snap.state.is_empty() { "…" } else { &snap.state };
    rows.push(fit(format!("● {state}")));
    if snap.nodes.is_empty() {
        rows.push(fit("计划: —".into()));
    } else {
        rows.push(fit(format!("计划: {} 节点", snap.nodes.len())));
        for n in &snap.nodes {
            rows.push(fit(format!("  {}: {}", n.id, n.summary)));
        }
    }
    let m = &snap.maintainer;
    let maintainer_empty = m.key_file_paths.is_empty()
        && m.key_conclusions.is_empty()
        && m.review_summary.is_empty();
    if maintainer_empty {
        rows.push(fit("维护者: —".into()));
    } else {
        let conclusions = m.key_conclusions.len() + m.review_summary.len();
        rows.push(fit(format!("维护者 ✓（{conclusions} 条结论）")));
    }
    if let Some(v) = &snap.plan_verdict {
        rows.push(fit(format!("计划审查: {}", v.outcome)));
    }
    if let Some(latest) = snap.exec_verdicts.last() {
        let conf = latest.confidence.as_deref().unwrap_or("?");
        rows.push(fit(format!(
            "执行审查: {}({}) ×{}",
            latest.outcome,
            conf,
            snap.exec_verdicts.len()
        )));
    }
    if snap.references.is_empty() {
        rows.push(fit("参考: —".into()));
    } else {
        rows.push(fit(format!("参考: {}", snap.references.join("、"))));
    }
    if snap.artifacts.is_empty() {
        rows.push(fit("产物: —".into()));
    } else {
        rows.push(fit(format!("产物: {} 个文件", snap.artifacts.len())));
        for a in &snap.artifacts {
            rows.push(fit(format!("  {}（{} 行）", a.path, a.lines)));
        }
    }
    rows
}

/// 四区渲染：顶状态条（alfred 版本 + run_id + 治理态徽标——按态着色，实时）/
/// 左列"对话"（事件渲染行 + 捕获残留行——按来源着色；跟随底部，PgUp/PgDn
/// 手动滚动时视口锚定 + 底部新行提示）/ 右列"状态"（看板快照，态行按态
/// 着色）/ 底部"输入"（多行编辑 + 按态提示标题，光标可见）。输入框内容宽
/// 与终端宽同源（框横贯全宽）：先定折行再定布局，无循环依赖；框高随内容
/// 增长（上限半屏），内容超高时可视窗口贴底、光标行越窗顶则上移保光标
/// 可见。着色只在此渲染层（REPL 面逐字节不变是硬底线）。
fn ui(f: &mut Frame, app: &mut TuiApp) {
    let area = f.area();
    let inner_w = area.width.saturating_sub(2) as usize;
    let disp = input_display(&app.input, inner_w);
    let max_input_h = (area.height / 2).max(3);
    let input_h = (disp.rows.len() as u16 + 2).clamp(3, max_input_h);

    let [status_area, main_area, input_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(input_h),
    ])
    .areas(area);
    let [conv_area, panel_area] = Layout::horizontal([
        Constraint::Percentage(70),
        Constraint::Percentage(30),
    ])
    .areas(main_area);

    // ── 顶状态条：标题 + run/态徽标（S4 按态着色）+ 按键提示，整行底色
    //    （Paragraph.style 铺满区域） ──
    let title = format!(" alfred v{} ", env!("CARGO_PKG_VERSION"));
    let mut spans = vec![Span::styled(
        title.clone(),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    )];
    let mut mid_w = 0usize;
    if app.session_ended {
        let mid = " 治理会话已退出 ";
        spans.push(Span::styled(mid, Style::new().fg(Color::White)));
        mid_w += mid.width();
    } else {
        match &app.dashboard {
            Some(s) if !s.run_id.is_empty() => {
                let run = format!(" {} ", s.run_id);
                let state = if s.state.is_empty() { "…" } else { s.state.as_str() };
                let badge = format!("● {state} ");
                spans.push(Span::styled(run.clone(), Style::new().fg(Color::White)));
                spans.push(Span::styled(
                    badge.clone(),
                    Style::new().fg(state_color(&s.state)).add_modifier(Modifier::BOLD),
                ));
                mid_w += run.width() + badge.width();
            }
            _ => {
                spans.push(Span::raw(" 无 run "));
                mid_w += " 无 run ".width();
            }
        }
    }
    let hint = "Enter 提交 · Ctrl-J 换行 · ↑↓ 历史 · PgUp/PgDn/End 滚动 · Ctrl-D/C 退出 ";
    let pad = status_area
        .width
        .saturating_sub(title.width() as u16 + mid_w as u16 + hint.width() as u16);
    spans.push(Span::raw(" ".repeat(pad as usize)));
    spans.push(Span::styled(hint, Style::new().fg(Color::Gray)));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(STATUS_BG)),
        status_area,
    );

    // ── 左列：对话流（事件/捕获残留行——按来源着色；视口 = 跟随底部或
    //    手动滚动锚定，新行不打断历史区阅读） ──
    let conv_block = Block::bordered().title(" 对话 ");
    let conv_inner = conv_block.inner(conv_area);
    let viewport = app.conv_viewport(conv_inner.width as usize, conv_inner.height as usize);
    let mut conv_lines: Vec<Line<'_>> = viewport
        .into_iter()
        .map(|(text, voice)| Line::styled(text, Style::new().fg(voice.color())))
        .collect();
    // 手动滚动期间新行到达：视口末行让位新行提示（黄色——历史区阅读不被
    // 打断，新内容不静默）。
    if app.conv_top.is_some() && app.new_below > 0 {
        if let Some(last) = conv_lines.last_mut() {
            *last = Line::styled(
                format!("↓ 新增 {} 行（End 回底）", app.new_below),
                Style::new().fg(Color::Yellow),
            );
        }
    }
    f.render_widget(Paragraph::new(conv_lines).block(conv_block), conv_area);

    // ── 右列：状态看板（run 目录周期聚合快照；首行 = 治理态行——按态着色，
    //    构造序由 dashboard_lines 固定） ──
    let panel_block = Block::bordered().title(" 状态 ");
    let panel_inner = panel_block.inner(panel_area);
    let panel_lines: Vec<Line<'_>> = match &app.dashboard {
        Some(snap) => dashboard_lines(snap, panel_inner.width as usize)
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                if i == 0 {
                    Line::styled(s, Style::new().fg(state_color(&snap.state)))
                } else {
                    Line::from(s)
                }
            })
            .collect(),
        None => vec![Line::from("（无 run——提交需求后建立）")],
    };
    f.render_widget(Paragraph::new(panel_lines).block(panel_block), panel_area);

    // ── 底部：输入框（多行编辑 + 按态提示标题 + 光标） ──
    let state = app.dashboard.as_ref().map(|s| s.state.as_str());
    let input_block = Block::bordered().title(format!(" {} ", input_hint(state)));
    let input_inner = input_block.inner(input_area);
    let visible = input_inner.height as usize;
    let offset = disp.rows.len().saturating_sub(visible).min(disp.cursor.0);
    let input_lines = disp.rows[offset..]
        .iter()
        .map(|s| Line::from(s.clone()))
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(input_lines).block(input_block), input_area);
    if input_inner.width > 0 && input_inner.height > 0 {
        let x = input_inner.x + (disp.cursor.1 as u16).min(input_inner.width - 1);
        let y = input_inner.y + (disp.cursor.0 - offset) as u16;
        f.set_cursor_position((x, y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use alfred_cli::chat_events::ActionKind;
    use alfred_cli::dashboard::{ArtifactView, MaintainerView, NodeView};

    /// 构造无修饰按键事件。
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// 构造 Ctrl 组合键事件。
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// 测试应用（空通道端 + 未发布 run 目录）+ worker 输入接收端（断言 submit
    /// 路由）。
    fn test_app() -> (TuiApp, mpsc::Receiver<String>) {
        let (wtx, wrx) = mpsc::channel();
        let (_etx, erx) = ChatEventBus::new();
        let (_ctx, crx) = mpsc::channel();
        let app = TuiApp::new(wtx, erx, crx, Arc::new(Mutex::new(None)));
        (app, wrx)
    }

    /// 快照构造（看板渲染测试数据源）。
    fn sample_snapshot() -> DashboardSnapshot {
        DashboardSnapshot {
            run_id: "run-18d353f6".into(),
            state: "planning".into(),
            nodes: vec![NodeView {
                id: "task-1".into(),
                summary: "create hello.txt".into(),
            }],
            maintainer: MaintainerView {
                key_file_paths: vec!["ws/a.py".into()],
                key_conclusions: vec!["结论一".into()],
                review_summary: vec!["摘要一".into(), "摘要二".into()],
            },
            references: vec!["omp-session.jsonl".into()],
            artifacts: vec![ArtifactView {
                path: "docs/x.md".into(),
                lines: 12,
            }],
            plan_verdict: None,
            exec_verdicts: Vec::new(),
            updated_at: "2026-09-10T00:00:00Z".into(),
        }
    }

    /// TestBackend 整帧 → 按行拼回字符串（断言辅助）。宽字符后随的隐藏格
    /// 跳过（ratatui buffer_view 同款 skip 语义），CJK 标题断言不被补位空格
    /// 打断。
    fn frame_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let mut out = String::new();
        for cells in buf.content.chunks(w) {
            let mut skip = 0usize;
            for c in cells {
                if skip == 0 {
                    out.push_str(c.symbol());
                }
                skip = skip.max(c.symbol().width()).saturating_sub(1);
            }
            out.push('\n');
        }
        out
    }

    /// 整帧里找 marker 首字符所在 cell 的前景色（着色断言辅助，S4）：宽字符
    /// 跳格语义同 [`frame_text`]（CJK 不打断定位）。
    fn frame_fg(terminal: &Terminal<TestBackend>, marker: &str) -> Option<Color> {
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        for cells in buf.content.chunks(w) {
            let mut s = String::new();
            let mut idx = Vec::new();
            let mut skip = 0usize;
            for (i, c) in cells.iter().enumerate() {
                if skip == 0 {
                    s.push_str(c.symbol());
                    idx.push(i);
                }
                skip = skip.max(c.symbol().width()).saturating_sub(1);
            }
            if let Some(pos) = s.find(marker) {
                return Some(cells[idx[pos]].fg);
            }
        }
        None
    }

    // ── 降级门 ──

    /// TERM 判定：有效终端名通过；dumb/空/缺失降级 REPL。
    #[test]
    fn term_gate() {
        assert!(term_enables_tui(Some("xterm-256color")));
        assert!(term_enables_tui(Some("xterm")));
        assert!(!term_enables_tui(Some("dumb")));
        assert!(!term_enables_tui(Some("")));
        assert!(!term_enables_tui(None));
    }

    /// 尺寸门（S1 审 P3，S4 落地）：0x0 pty（script/expect 类）双 tty 且 TERM
    /// 有效——尺寸非零检查是唯一能拦住它的门；宽或高任一为零均降级 REPL。
    #[test]
    fn size_gate_zero_pty_degrades_to_repl() {
        assert!(size_enables_tui(80, 24));
        assert!(size_enables_tui(1, 1), "极小但非零——可渲染（折行/截断兜底）");
        assert!(!size_enables_tui(0, 0), "0x0 pty：无处渲染，降级 REPL");
        assert!(!size_enables_tui(0, 24), "零宽降级");
        assert!(!size_enables_tui(80, 0), "零高降级");
    }

    // ── 折行真源 ──

    /// ASCII 按显示宽折段；CJK 按双宽折段（两字一行）；空行单空段；
    /// 宽 0 防御不折；单字符超总宽独占一段。
    #[test]
    fn wrap_ascii_cjk_and_edges() {
        assert_eq!(
            wrap_segments("abcdef", 4),
            vec![(0, "abcd".to_string()), (4, "ef".to_string())]
        );
        assert_eq!(
            wrap_segments("超超超超", 4),
            vec![(0, "超超".to_string()), (2, "超超".to_string())]
        );
        assert_eq!(wrap_segments("", 4), vec![(0, String::new())]);
        assert_eq!(wrap_segments("abc", 0), vec![(0, "abc".to_string())]);
        assert_eq!(
            wrap_segments("a超b", 2),
            vec![
                (0, "a".to_string()),
                (1, "超".to_string()),
                (2, "b".to_string())
            ]
        );
    }

    /// grapheme 原子性（S1 审 P3，S4 落地）：ZWJ 家庭 emoji / 旗帜 / 肤色
    /// 修饰是用户感知的单个字符——折行绝不把 cluster 拆到两段；cluster 宽
    /// 超总宽时独占一段（同"单字符超宽"惯例）；段首 char 偏移按 cluster
    /// 累计（光标定位同源）。
    #[test]
    fn wrap_grapheme_clusters_never_split() {
        // ZWJ 家庭 emoji：4 人 + 3 ZWJ = 7 char 一个 cluster。
        let family = "👨‍👩‍👧‍👦";
        assert_eq!(family.chars().count(), 7);
        let segs = wrap_segments(&format!("a{family}b"), 4);
        // "a"(1) + cluster(8) > 4 → 折段；cluster 整体一段（不拆）；b 另起。
        assert_eq!(segs.len(), 3, "三段：a / family / b：{segs:?}");
        assert_eq!(segs[1].1, family, "ZWJ 序列不拆段：{:?}", segs[1].1);
        assert_eq!(segs[2], (8, "b".to_string()), "cluster 后段首 char 偏移 = 1+7");

        // 旗帜（区域指示符对）与肤色修饰同理原子。
        let flag = "🇨🇳";
        assert_eq!(wrap_segments(flag, 1).len(), 1, "旗帜不拆段");
        let tone = "👍🏽";
        assert_eq!(wrap_segments(&format!("x{tone}"), 2)[1].1, tone, "肤色修饰不拆段");

        // 纯 CJK/ASCII 行为不变（单 char 即单 cluster，既有语义回归锁）。
        assert_eq!(
            wrap_segments("超超超超", 4),
            vec![(0, "超超".to_string()), (2, "超超".to_string())]
        );
    }

    /// 光标定位：段内偏移按显示宽；折行边界归下段段首；行尾归末段末。
    #[test]
    fn cursor_pos_on_wrap_boundaries() {
        let segs = wrap_segments("abcdef", 4); // [(0,"abcd"), (4,"ef")]
        assert_eq!(cursor_segment_pos(&segs, 2), (0, 2));
        assert_eq!(cursor_segment_pos(&segs, 4), (1, 0));
        assert_eq!(cursor_segment_pos(&segs, 6), (1, 2));
        let cjk = wrap_segments("超超超超", 4); // [(0,"超超"), (2,"超超")]
        assert_eq!(cursor_segment_pos(&cjk, 1), (0, 2)); // 第一字后
        assert_eq!(cursor_segment_pos(&cjk, 2), (1, 0)); // 折点归下段
    }

    // ── 输入编辑器 ──

    /// 插入/退格/前删/越界安全；中文按整 char 删（光标 char 语义）。
    #[test]
    fn editor_edit_and_cjk_safety() {
        let mut ed = InputEditor::new();
        for c in "写个hel".chars() {
            ed.insert_char(c);
        }
        assert_eq!(ed.text(), "写个hel");
        ed.left();
        ed.insert_char('X');
        assert_eq!(ed.text(), "写个heXl");
        ed.end();
        ed.backspace();
        assert_eq!(ed.text(), "写个heX");
        ed.home();
        ed.delete_forward(); // 删整个"写"
        assert_eq!(ed.text(), "个heX");
        ed.end();
        for _ in 0..6 {
            ed.backspace(); // 越界退格安全（删空后行首无上行，静默停）
        }
        assert_eq!(ed.text(), "");
        assert_eq!((ed.row, ed.col), (0, 0));
        assert!(ed.is_empty());
    }

    /// Ctrl-J 断行 / ↑ 行间导航（多行时不触发历史）/ 行首退格并行为一行。
    #[test]
    fn editor_newline_join_navigation() {
        let mut ed = InputEditor::new();
        for c in "ab".chars() {
            ed.insert_char(c);
        }
        ed.insert_newline();
        for c in "cd".chars() {
            ed.insert_char(c);
        }
        ed.up(); // 多行缓冲：↑ 是行间导航
        assert_eq!((ed.row, ed.col), (0, 2));
        ed.backspace(); // 行尾退格：删 'b'（行内分支）
        assert_eq!(ed.text(), "a\ncd");
        assert_eq!((ed.row, ed.col), (0, 1));
        ed.down(); // 回末行（↓ 行间导航）
        ed.home();
        ed.backspace(); // 行首退格 = 并行，光标停接缝
        assert_eq!(ed.text(), "acd");
        assert_eq!((ed.row, ed.col), (0, 1));
        ed.right();
        ed.left();
        assert_eq!((ed.row, ed.col), (0, 1));
    }

    /// 提交/历史/草稿：非空进历史；↑↓ 遍历到最早/越过最新恢复草稿；空提交
    /// 不进历史；多行历史条目整段载入光标落末。
    #[test]
    fn editor_submit_history_and_draft() {
        let mut ed = InputEditor::new();
        for c in "第一条".chars() {
            ed.insert_char(c);
        }
        assert_eq!(ed.submit(), "第一条");
        for c in "第二条".chars() {
            ed.insert_char(c);
        }
        assert_eq!(ed.submit(), "第二条");
        assert!(ed.is_empty());
        ed.up();
        assert_eq!(ed.text(), "第二条");
        ed.up();
        assert_eq!(ed.text(), "第一条");
        ed.up(); // 已是最早，停住
        assert_eq!(ed.text(), "第一条");
        ed.down();
        assert_eq!(ed.text(), "第二条");
        ed.down(); // 越过最新 → 恢复草稿（空）
        assert_eq!(ed.text(), "");
        for c in "xy".chars() {
            ed.insert_char(c);
        }
        ed.up(); // 草稿保全：浏览历史前保存
        assert_eq!(ed.text(), "第二条");
        ed.down(); // 越过最新 → 恢复草稿 "xy"
        assert_eq!(ed.text(), "xy");
        assert_eq!(ed.submit(), "xy"); // 恢复的草稿提交照常进历史
        assert_eq!(ed.history.len(), 3);
        assert_eq!(ed.submit(), ""); // 空提交不进历史
        assert_eq!(ed.history.len(), 3);
    }

    // ── 应用级按键语义 ──

    /// 提交路由治理 worker（行原文整发，非空回声左列）/ 空提交不回声、由
    /// chat_session 单一真源回应（经事件返回，本地零文案）/ Ctrl-D 空退出、
    /// 非空删字符 / Ctrl-C 恒退出。
    #[test]
    fn app_submit_routes_and_exit_semantics() {
        let (mut app, wrx) = test_app();
        for c in "需求甲".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.messages,
            vec![ConvLine { text: "你: 需求甲".into(), voice: LineVoice::Owner }]
        );
        assert_eq!(wrx.try_recv(), Ok("需求甲".to_string()));

        app.handle_key(key(KeyCode::Enter)); // 空提交：发原文，不回声不本地文案
        assert_eq!(app.messages.len(), 1, "空提交本地零新增");
        assert_eq!(wrx.try_recv(), Ok("".to_string()));

        app.handle_key(ctrl('d')); // 空缓冲 → 退出
        assert!(app.exit);

        let (mut app2, _wrx2) = test_app();
        app2.handle_key(key(KeyCode::Char('a')));
        app2.handle_key(key(KeyCode::Char('b')));
        app2.handle_key(key(KeyCode::Left)); // 光标移到 'b' 前
        app2.handle_key(ctrl('d')); // 非空：前删 'b'，不退出
        assert!(!app2.exit);
        assert_eq!(app2.input.text(), "a");
        app2.handle_key(ctrl('c')); // Ctrl-C 恒退出
        assert!(app2.exit);
    }

    /// Tab 保留（S1 审查修复）：Tab 键插入 '\t'——数据层保真（对齐 REPL
    /// `sanitize_raw_line` 保留 \t，粘贴含缩进文本不失真），char 列推进；
    /// 显示层渲染边界定宽展开为可见缩进（折行/光标/展开共用 TAB_WIDTH 单一
    /// 真源），帧内无 '\t' 控制字符。
    #[test]
    fn tab_kept_in_data_expanded_for_display() {
        // 键层 → 编辑器：'\t' 入缓冲，后续字符接续其后（char 列推进）。
        let (mut app, _wrx) = test_app();
        for c in "ab".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.input.text(), "ab\tc");
        assert_eq!((app.input.row, app.input.col), (0, 4));

        // 折行/光标真源：'\t' 计 TAB_WIDTH 显示宽，光标 x 跨过展开宽；
        // 极窄行 tab 宽 > 行宽独占一段（同"单字符宽 > 总宽"惯例）。
        let segs = wrap_segments("ab\tc", 10);
        assert_eq!(segs, vec![(0, "ab\tc".to_string())]);
        assert_eq!(cursor_segment_pos(&segs, 3), (0, 2 + TAB_WIDTH));
        assert_eq!(
            wrap_segments("a\tb", 2),
            vec![
                (0, "a".to_string()),
                (1, "\t".to_string()),
                (2, "b".to_string())
            ]
        );

        // 输入区显示：段文本展开为空格，光标 x 与渲染同源（行尾 = 2+4+1）。
        let d = input_display(&app.input, 10);
        assert_eq!(d.rows, vec!["ab    c".to_string()]);
        assert_eq!(d.cursor, (0, 7));

        // 提交：消息数据保真 '\t'（回声含原文），左列渲染展开缩进（+来源着色）。
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.messages.last().unwrap().text, "你: ab\tc");
        assert_eq!(app.messages.last().unwrap().voice, LineVoice::Owner);
        assert_eq!(
            conversation_rows(&app.messages, 20, 5, None),
            vec![("你: ab    c".to_string(), LineVoice::Owner)]
        );

        // 整帧黑盒：回显含可见缩进，帧内无 '\t'（cell 只见空格）。
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = frame_text(&terminal);
        assert!(text.contains("ab    c"), "回显应含可见缩进，实际帧：\n{text}");
        assert!(!text.contains('\t'), "帧内不应有 '\\t' 控制字符");
    }

    // ── 事件渲染（左列，按变体） ──

    /// 各变体左列渲染（正文 + 来源）：前缀分职（S4）——ShellNotice 渲染
    /// `[chat]`（chat.rs 会话壳通知，修复此前误标 `[orchestrator]`）、
    /// OrchestratorNotice 渲染 `[orchestrator]`（S2b 契约：governance.rs 点位
    /// 载荷去前缀，前缀由 sink 渲染时统一加回——左列与 REPL 逐面同面前缀）；
    /// [pi]/错误行各自前缀；来源 → 着色单一映射。
    #[test]
    fn apply_event_renders_each_variant() {
        let (mut app, _wrx) = test_app();
        app.dash_dirty = false; // 构造的首帧置脏先消费掉，断言聚焦事件本身
        app.apply_event(ChatEvent::OwnerEcho("需求".into()));
        app.apply_event(ChatEvent::PiAction {
            kind: ActionKind::Probe,
            detail: "ls src".into(),
        });
        app.apply_event(ChatEvent::PiAction {
            kind: ActionKind::Blocked,
            detail: "cat ~/.omp/runs".into(),
        });
        assert!(!app.dash_dirty, "回声/动作不置脏");
        app.apply_event(ChatEvent::PiReply("计划分两步".into()));
        assert!(app.dash_dirty, "状态类事件置脏");
        app.dash_dirty = false;
        app.apply_event(ChatEvent::ShellNotice("已受理：X".into()));
        app.apply_event(ChatEvent::OrchestratorNotice(
            "进入计划审查（state=plan_reviewing）".into(),
        ));
        app.apply_event(ChatEvent::EscalationPrompt {
            reason: "计划审查意见（打回）：太粗".into(),
            source: alfred_core::governance::EscalationSource::PlanReview,
        });
        app.apply_event(ChatEvent::Error {
            message: "操作失败：boom".into(),
            stream: alfred_cli::chat_events::ErrorStream::Stderr,
        });
        assert!(app.dash_dirty, "挂起/错误置脏");
        assert_eq!(
            app.messages,
            vec![
                ConvLine { text: "你: 需求".into(), voice: LineVoice::Owner },
                ConvLine {
                    text: "[pi] ⋯ 探查: ls src".into(),
                    voice: LineVoice::PiAction,
                },
                ConvLine {
                    text: "[pi] ⋯ 探查（被治理拦截）: cat ~/.omp/runs".into(),
                    voice: LineVoice::PiAction,
                },
                ConvLine { text: "[pi] 计划分两步".into(), voice: LineVoice::PiReply },
                ConvLine { text: "[chat] 已受理：X".into(), voice: LineVoice::Chat },
                ConvLine {
                    text: "[orchestrator] 进入计划审查（state=plan_reviewing）".into(),
                    voice: LineVoice::Orchestrator,
                },
                ConvLine {
                    text: "[chat] 计划审查意见（打回）：太粗".into(),
                    voice: LineVoice::Escalation,
                },
                ConvLine { text: "[chat] 操作失败：boom".into(), voice: LineVoice::Error },
            ]
        );
    }

    /// pump：捕获残留行入列 + 事件 FIFO 取尽 + Disconnected 置 session_ended
    /// （提示行一次性，输入停用）。S2b 后治理 `[orchestrator]` 行走事件通道
    /// （渲染加回前缀），捕获通道只剩未事件化残留。
    #[test]
    fn pump_drains_channels_and_marks_session_end() {
        let (wtx, wrx) = mpsc::channel();
        let (etx, erx) = ChatEventBus::new();
        let (ctx, crx) = mpsc::channel();
        ctx.send("docker: pulled image（未事件化残留）".to_string()).unwrap();
        ctx.send("[orchestrator] warn: exec verdict parse failed".to_string()).unwrap();
        assert!(etx.send(ChatEvent::OrchestratorNotice(
            "进入计划审查（state=plan_reviewing）".into()
        )));
        assert!(etx.send(ChatEvent::PiReply("答复".into())));
        drop(etx); // worker 退场
        let mut app = TuiApp::new(wtx, erx, crx, Arc::new(Mutex::new(None)));
        app.pump();
        assert_eq!(
            app.messages,
            vec![
                ConvLine {
                    text: "docker: pulled image（未事件化残留）".into(),
                    voice: LineVoice::Plain,
                },
                ConvLine {
                    text: "[orchestrator] warn: exec verdict parse failed".into(),
                    voice: LineVoice::Warn,
                },
                ConvLine {
                    text: "[orchestrator] 进入计划审查（state=plan_reviewing）".into(),
                    voice: LineVoice::Orchestrator,
                },
                ConvLine { text: "[pi] 答复".into(), voice: LineVoice::PiReply },
                ConvLine {
                    text: "[chat] 治理会话已退出（Ctrl-D/Ctrl-C 关闭界面）。".into(),
                    voice: LineVoice::Chat,
                },
            ]
        );
        assert!(app.session_ended);
        assert!(app.dash_dirty, "Disconnected 前的状态事件已置脏");

        // 会话已退：submit 清缓冲即止，不再投递。
        for c in "迟到的输入".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(wrx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(app.input.is_empty());
    }

    // ── 捕获行过滤 ──

    /// [chat]/[pi]/[orchestrator] 前缀行已事件化（S2b 起 [orchestrator] 含
    /// governance.rs 全部点位）→ 丢弃；**例外白名单**（S2 审 P2，S3 翻转）：
    /// `[orchestrator] warn:` 行透传——alfred-reviewer verdict 解析告警未
    /// 事件化（REPL 面可见，丢弃=TUI 可见性回退）；近前缀（`warning:`）不
    /// 匹配白名单仍丢弃；空行丢弃；其余（未事件化残留——无前缀输出/
    /// [driver] 行）整串透传。
    #[test]
    fn capture_line_filter() {
        let (tx, rx) = mpsc::channel();
        forward_line(&tx, "[chat] 已受理：X");
        forward_line(&tx, "[pi] ⋯ 探查: ls");
        forward_line(&tx, "");
        forward_line(&tx, "[orchestrator] 计划审查中");
        forward_line(&tx, "[orchestrator] warn: exec verdict parse failed");
        forward_line(&tx, "[orchestrator] warning: 近前缀不匹配白名单");
        forward_line(&tx, "  run_dir: /tmp/x（残留续行）");
        forward_line(&tx, "[driver] 状态行");
        drop(tx);
        let got: Vec<String> = rx.iter().collect();
        assert_eq!(
            got,
            vec![
                "[orchestrator] warn: exec verdict parse failed".to_string(),
                "  run_dir: /tmp/x（残留续行）".to_string(),
                "[driver] 状态行".to_string(),
            ]
        );
    }

    /// drainer 端到端（真实文件读端驱动 [`drain_capture`] 主体，S2 审 P2 的
    /// warn 可见性黑盒）：行剥分 + 过滤规则生效——warn 白名单行透传（含无
    /// 换行尾行 EOF 补发），事件化前缀行丢弃。
    #[test]
    fn drain_capture_warn_whitelist_end_to_end() {
        let path = std::env::temp_dir()
            .join(format!("alfred-drain-capture-{}.txt", std::process::id()));
        std::fs::write(
            &path,
            "[orchestrator] warn: exec verdict parse failed: bad json\n\
             [chat] 已受理：X\n\
             [pi] 答复\n\
             [orchestrator] 执行审查中…\n\
             [orchestrator] warn: plan verdict parse failed: also bad\n\
             docker: pulled image\n\
             [driver] 尾行无换行",
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        drain_capture(File::open(&path).unwrap(), tx);
        let got: Vec<String> = rx.iter().collect();
        assert_eq!(
            got,
            vec![
                "[orchestrator] warn: exec verdict parse failed: bad json".to_string(),
                "[orchestrator] warn: plan verdict parse failed: also bad".to_string(),
                "docker: pulled image".to_string(),
                "[driver] 尾行无换行".to_string(),
            ]
        );
        std::fs::remove_file(&path).ok();
    }

    // ── 看板渲染 ──

    /// 快照 → 看板行：态/计划节点/维护者/审查/参考/产物各段；超宽截断带
    /// 省略号（CJK 宽度感知）。
    #[test]
    fn dashboard_renders_snapshot_sections() {
        let snap = sample_snapshot();
        let lines = dashboard_lines(&snap, 40);
        assert_eq!(lines[0], "● planning");
        assert_eq!(lines[1], "计划: 1 节点");
        assert_eq!(lines[2], "  task-1: create hello.txt");
        assert_eq!(lines[3], "维护者 ✓（3 条结论）");
        assert_eq!(lines[4], "参考: omp-session.jsonl");
        assert_eq!(lines[5], "产物: 1 个文件");
        assert_eq!(lines[6], "  docs/x.md（12 行）");

        // 执行审查历史徽标：最新一条 + 计数。
        let mut snap2 = snap.clone();
        snap2.exec_verdicts = vec![
            alfred_cli::dashboard::VerdictView {
                outcome: "I".into(),
                confidence: Some("high".into()),
                detail: "d1".into(),
            },
            alfred_cli::dashboard::VerdictView {
                outcome: "C".into(),
                confidence: Some("medium".into()),
                detail: "d2".into(),
            },
        ];
        let lines2 = dashboard_lines(&snap2, 40);
        assert!(lines2.contains(&"执行审查: C(medium) ×2".to_string()));

        // 超宽截断：CJK 双宽计入（省略号占 1）。
        let narrow = dashboard_lines(&snap, 6);
        assert!(narrow[0].ends_with('…'), "超宽行带省略号：{}", narrow[0]);
        assert!(narrow.iter().all(|l| l.width() <= 6), "全部行不超宽");
    }

    /// 空快照段占位（无节点/维护者/产物 → —）。
    #[test]
    fn dashboard_empty_sections_placeholder() {
        let snap = DashboardSnapshot {
            run_id: "run-x".into(),
            state: String::new(),
            nodes: Vec::new(),
            maintainer: MaintainerView::default(),
            references: Vec::new(),
            artifacts: Vec::new(),
            plan_verdict: None,
            exec_verdicts: Vec::new(),
            updated_at: String::new(),
        };
        let lines = dashboard_lines(&snap, 40);
        assert_eq!(lines[0], "● …");
        assert_eq!(lines[1], "计划: —");
        assert_eq!(lines[2], "维护者: —");
        assert_eq!(lines[3], "参考: —");
        assert_eq!(lines[4], "产物: —");
    }

    /// 宽度适配：CJK/ASCII 混排按显示宽截断；宽度 0/1 防御。
    #[test]
    fn fit_width_cjk_aware() {
        assert_eq!(fit_width("abc", 5), "abc");
        assert_eq!(fit_width("abcdef", 4), "abc…");
        assert_eq!(fit_width("超超超超", 5), "超超…");
        assert_eq!(fit_width("超超超超", 2), "…");
        assert_eq!(fit_width("任何", 0), "");
        assert_eq!(fit_width("a\tb", 3), "a…"); // '\t' 计 TAB_WIDTH
    }

    /// 输入区提示按治理态派生（挂起/推进/对话/收集/终态新需求）——全部 8 态
    /// 覆盖（S3 补 plan_reviewing/exec_reviewing 两态断言，hint 完整性锁定）。
    #[test]
    fn input_hint_by_state() {
        assert_eq!(input_hint(None), "需求");
        assert_eq!(input_hint(Some("")), "需求");
        assert_eq!(input_hint(Some("planning")), "对 pi 说");
        assert_eq!(input_hint(Some("plan_reviewing")), "治理推进中（输入将排队）");
        assert_eq!(input_hint(Some("executing")), "治理推进中（输入将排队）");
        assert_eq!(input_hint(Some("exec_reviewing")), "治理推进中（输入将排队）");
        assert_eq!(input_hint(Some("plan_rejected")), "回复：重试 / 放弃 / 或直接说修改意见");
        assert_eq!(input_hint(Some("escalated")), "回复：重试 / 放弃 / 或直接说修改意见");
        assert_eq!(input_hint(Some("completed")), "新需求");
        assert_eq!(input_hint(Some("abandoned")), "新需求");
    }

    /// 治理态着色（S4）：planning 黄 / 审查态蓝 / executing 绿 / 挂起红 /
    /// completed 亮绿 / abandoned 灰 / 未知白——状态条徽标 + 看板态行共用。
    #[test]
    fn state_color_mapping() {
        assert_eq!(state_color("planning"), Color::Yellow);
        assert_eq!(state_color("plan_reviewing"), Color::Blue);
        assert_eq!(state_color("exec_reviewing"), Color::Blue);
        assert_eq!(state_color("executing"), Color::Green);
        assert_eq!(state_color("plan_rejected"), Color::Red);
        assert_eq!(state_color("escalated"), Color::Red);
        assert_eq!(state_color("completed"), Color::LightGreen);
        assert_eq!(state_color("abandoned"), Color::Gray);
        assert_eq!(state_color(""), Color::White);
        assert_eq!(state_color("未知态"), Color::White);
    }

    /// 左列来源着色（S4）：[pi] 答复青 / [orchestrator] 白 / warn 黄 /
    /// Error 红 / 过程行（[pi] ⋯、捕获残留）暗灰 / [chat] 灰 / 挂起意见黄 /
    /// 属主回声白。
    #[test]
    fn line_voice_color_mapping() {
        assert_eq!(LineVoice::PiReply.color(), Color::Cyan);
        assert_eq!(LineVoice::Orchestrator.color(), Color::White);
        assert_eq!(LineVoice::Warn.color(), Color::Yellow);
        assert_eq!(LineVoice::Error.color(), Color::Red);
        assert_eq!(LineVoice::PiAction.color(), Color::DarkGray);
        assert_eq!(LineVoice::Plain.color(), Color::DarkGray);
        assert_eq!(LineVoice::Chat.color(), Color::Gray);
        assert_eq!(LineVoice::Escalation.color(), Color::Yellow);
        assert_eq!(LineVoice::Owner.color(), Color::White);
    }

    // ── 显示布局 ──

    /// 输入区折行布局 + 光标跨行定位（渲染与光标同一真源）。
    #[test]
    fn input_display_wraps_and_tracks_cursor() {
        let mut ed = InputEditor::new();
        for c in "abcd".chars() {
            ed.insert_char(c);
        }
        let d = input_display(&ed, 2);
        // S4 光标满行修正（S1 审 P3）：行尾恰在满宽末段——下一个将输入字符
        // 必落下一显示行，光标随之（补空续行承载），不再被压回本行末列。
        assert_eq!(d.rows, vec!["ab".to_string(), "cd".to_string(), String::new()]);
        assert_eq!(d.cursor, (2, 0));

        // 光标在段中/未满行：常规定位（段内偏移 = 显示宽）。
        let mut ed_mid = InputEditor::new();
        for c in "abcd".chars() {
            ed_mid.insert_char(c);
        }
        ed_mid.left(); // col 3：末段（"cd"）中段
        let d_mid = input_display(&ed_mid, 2);
        assert_eq!(d_mid.rows, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!(d_mid.cursor, (1, 1), "末段中段（col 3）= 末段 x=1");

        let mut ed2 = InputEditor::new();
        for c in "ab".chars() {
            ed2.insert_char(c);
        }
        ed2.insert_newline();
        for c in "cd".chars() {
            ed2.insert_char(c);
        }
        let d2 = input_display(&ed2, 2);
        // 逻辑行 1 行尾满宽 → 续行承载光标（多行缓冲中间不吞行）。
        assert_eq!(d2.rows, vec!["ab".to_string(), "cd".to_string(), String::new()]);
        assert_eq!(d2.cursor, (2, 0));

        // 未满行行尾：光标本行末列（无续行）。
        let mut ed4 = InputEditor::new();
        for c in "abc".chars() {
            ed4.insert_char(c);
        }
        let d4 = input_display(&ed4, 10);
        assert_eq!(d4.rows, vec!["abc".to_string()]);
        assert_eq!(d4.cursor, (0, 3));

        let mut ed3 = InputEditor::new();
        for c in "超超".chars() {
            ed3.insert_char(c);
        }
        ed3.left(); // 光标在两字之间（显示宽 2 处）
        let d3 = input_display(&ed3, 4);
        assert_eq!(d3.rows, vec!["超超".to_string()]);
        assert_eq!(d3.cursor, (0, 2));
    }

    /// 左列视口（S4 滚动）：跟随态取末 `height` 行；手动态锚定 `[top, top+height)`；
    /// top 越界夹取；多行消息按行折行展开。
    #[test]
    fn conversation_viewport_follow_and_manual() {
        let msgs: Vec<ConvLine> = (0..50)
            .map(|i| ConvLine { text: format!("行{i}"), voice: LineVoice::Plain })
            .collect();
        let rows = conversation_rows(&msgs, 10, 3, None);
        assert_eq!(
            rows,
            vec![
                ("行47".to_string(), LineVoice::Plain),
                ("行48".to_string(), LineVoice::Plain),
                ("行49".to_string(), LineVoice::Plain),
            ]
        );
        // 手动滚动：top 锚定窗口（新行追加不移动——由 app.conv_top 语义承担，
        // 此处断言纯函数窗口）。
        let rows_top = conversation_rows(&msgs, 10, 3, Some(10));
        assert_eq!(
            rows_top,
            vec![
                ("行10".to_string(), LineVoice::Plain),
                ("行11".to_string(), LineVoice::Plain),
                ("行12".to_string(), LineVoice::Plain),
            ]
        );
        // top 越界夹取到末行（防御：resize/内容收缩）。
        let rows_clamp = conversation_rows(&msgs, 10, 3, Some(48));
        assert_eq!(rows_clamp.last().unwrap().0, "行49");
        assert_eq!(rows_clamp.len(), 2, "末行起窗口只剩 2 行：{:?}", rows_clamp);
        // 多行消息按行折行展开后同样只留尾部。
        let multi = vec![ConvLine { text: "a\nb\nc".into(), voice: LineVoice::Chat }];
        let rows2 = conversation_rows(&multi, 10, 2, None);
        assert_eq!(
            rows2,
            vec![("b".to_string(), LineVoice::Chat), ("c".to_string(), LineVoice::Chat)]
        );
    }

    /// 左列滚动（S4 app 级）：PgUp 进手动滚动（视口锚定，新行不打断 + 计数）/
    /// PgDn 翻回 / End 回底清计数；内容不足一屏不可滚；几何由渲染帧缓存
    /// （scroll_page 依赖 conv_geom）。
    #[test]
    fn scroll_paging_anchor_and_new_line_count() {
        let (mut app, _wrx) = test_app();
        // 首帧渲染缓存几何（80x24 → 左列区 56 宽 20 高，内宽 54/内高 18）。
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for i in 0..60 {
            app.push_line(ConvLine { text: format!("消息{i}"), voice: LineVoice::Chat });
        }
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert_eq!(app.conv_geom, (54, 18), "几何缓存 = 左列内宽×内高");
        assert_eq!(app.conv_top, None, "初始跟随底部");

        // PgUp：进入手动滚动，视口上翻一页（top = 60-18-18 = 24）。
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.conv_top, Some(24));
        let view = app.conv_viewport(54, 18);
        assert_eq!(view.first().unwrap().0, "消息24", "视口锚定历史区");
        assert!(!view.iter().any(|(t, _)| t == "消息59"), "最新行不在视口");

        // 手动滚动期间新行到达：视口不动 + 新行计数（提示数据源）。
        app.apply_event(ChatEvent::PiReply("新答复".into()));
        app.apply_event(ChatEvent::ShellNotice("新通知".into()));
        assert_eq!(app.conv_top, Some(24), "视口锚定不打断");
        assert_eq!(app.new_below, 2, "两条新行计数");
        let view2 = app.conv_viewport(54, 18);
        assert_eq!(view2.first().unwrap().0, "消息24", "新行不移视口");

        // End：回底跟随 + 计数清零。
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.conv_top, None);
        assert_eq!(app.new_below, 0);
        let view3 = app.conv_viewport(54, 18);
        assert_eq!(view3.last().unwrap().0, "[chat] 新通知", "回底最新行可见");

        // PgDn 在底部：无操作（跟随态保持）。
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.conv_top, None);

        // 再次 PgUp → PgDn 翻回底部窗口 → 自动回跟随。
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.conv_top, Some(62 - 18 - 18));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.conv_top, None, "PgDn 触底回跟随");

        // 内容不足一屏：PgUp 无处可滚（保持跟随）。
        let (mut app2, _wrx2) = test_app();
        let mut terminal2 = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app2.push_line(ConvLine { text: "只有一行".into(), voice: LineVoice::Chat });
        terminal2.draw(|f| ui(f, &mut app2)).unwrap();
        app2.handle_key(key(KeyCode::PageUp));
        assert_eq!(app2.conv_top, None, "不足一屏不可滚");
    }

    /// 滚动整帧黑盒（S4）：手动滚动 + 新行 → 视口末行让位"↓ 新增 N 行
    /// （End 回底）"提示（黄色）；End 回底提示消失、最新行回归。
    #[test]
    fn render_scroll_new_line_hint() {
        let (mut app, _wrx) = test_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        for i in 0..60 {
            app.push_line(ConvLine { text: format!("消息{i}"), voice: LineVoice::Chat });
        }
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        app.handle_key(key(KeyCode::PageUp));
        app.apply_event(ChatEvent::PiReply("滚动期间新答复".into()));
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = frame_text(&terminal);
        assert!(text.contains("↓ 新增 1 行（End 回底）"), "新行提示占末行：\n{text}");
        assert!(!text.contains("滚动期间新答复"), "新行本体不打断视口：\n{text}");
        assert_eq!(frame_fg(&terminal, "↓ 新增"), Some(Color::Yellow), "提示黄色");
        assert!(text.contains("消息24"), "历史区可见：\n{text}");

        app.handle_key(key(KeyCode::End));
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text2 = frame_text(&terminal);
        assert!(!text2.contains("新增"), "回底提示消失：\n{text2}");
        assert!(text2.contains("滚动期间新答复"), "最新行回归：\n{text2}");
    }

    // ── 整帧渲染（TestBackend 黑盒：帧内容 + 光标落点） ──

    /// 四区汇合：状态条（alfred 版本 + 无 run 占位）/ 左列"对话"（回显可见）/
    /// 右列"状态"看板（无 run 占位）/ 输入框（打字可见 + 光标落输入框内）。
    #[test]
    fn render_four_zones_and_cursor() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (mut app, _wrx) = test_app();
        for c in "hi".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let text = frame_text(&terminal);
        assert!(text.contains(&format!("alfred v{}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("无 run"), "无 run 占位：\n{text}");
        assert!(text.contains("对话"));
        assert!(text.contains("状态"));
        assert!(text.contains("需求"), "输入框按态提示（需求收集）：\n{text}");
        assert!(text.contains("hi"), "输入框应显示已打字内容");

        // 光标：输入框在底部 3 行（80x24 → 状态条 1 + 主体 20），内容区
        // (1, 22)，"hi" 后光标在 x=3。
        assert!(terminal.backend().cursor_visible());
        terminal.backend_mut().assert_cursor_position((3, 22));

        // 提交回声左列后，输入框清空、光标回内容区行首
        app.handle_key(key(KeyCode::Enter));
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text2 = frame_text(&terminal);
        assert!(text2.contains("你: hi"), "提交后回声左列：\n{text2}");
        terminal.backend_mut().assert_cursor_position((1, 22));
    }

    /// 看板接线整帧：dashboard 置入快照后右列/状态条/输入框提示三处联动
    /// （run_id + 治理态徽标 + 节点行 + 挂起态提示）。
    #[test]
    fn render_dashboard_linked_zones() {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let (mut app, _wrx) = test_app();
        let mut snap = sample_snapshot();
        snap.state = "plan_rejected".into();
        app.dashboard = Some(snap);
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = frame_text(&terminal);
        assert!(text.contains("run-18d353f6"), "状态条 run_id：\n{text}");
        assert!(text.contains("● plan_rejected"), "状态条治理态：\n{text}");
        assert!(text.contains("task-1: create hello.txt"), "看板节点行：\n{text}");
        assert!(text.contains("维护者 ✓（3 条结论）"), "看板维护者行：\n{text}");
        assert!(
            text.contains("回复：重试 / 放弃 / 或直接说修改意见"),
            "挂起态输入提示：\n{text}"
        );

        // 会话退出：状态条改提示。
        app.session_ended = true;
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text2 = frame_text(&terminal);
        assert!(text2.contains("治理会话已退出"), "会话退出提示：\n{text2}");
    }

    /// 着色整帧黑盒（S4）：状态条徽标按治理态、看板态行同色、左列按来源——
    /// [pi] 答复青 / [orchestrator] 白 / [chat] 灰 / Error 红 / Escalation 黄 /
    /// 属主回声白 / 过程行暗灰。TestBackend cell fg 即最终 ANSI 前景（pty 面
    /// 由 tui_pty.py 按 SGR 色码复核）。
    #[test]
    fn render_colored_zones_by_state_and_voice() {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let (mut app, _wrx) = test_app();
        let mut snap = sample_snapshot();
        snap.state = "planning".into();
        app.dashboard = Some(snap);
        app.apply_event(ChatEvent::OwnerEcho("需求".into()));
        app.apply_event(ChatEvent::PiAction {
            kind: ActionKind::Probe,
            detail: "ls src".into(),
        });
        app.apply_event(ChatEvent::PiReply("计划分两步".into()));
        app.apply_event(ChatEvent::ShellNotice("已受理：X".into()));
        app.apply_event(ChatEvent::OrchestratorNotice("计划审查中…".into()));
        app.apply_event(ChatEvent::EscalationPrompt {
            reason: "计划审查意见（打回）：太粗".into(),
            source: alfred_core::governance::EscalationSource::PlanReview,
        });
        app.apply_event(ChatEvent::Error {
            message: "操作失败：boom".into(),
            stream: alfred_cli::chat_events::ErrorStream::Stdout,
        });
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        // 状态条徽标 + 看板态行：planning 黄（同色单一真源）。
        assert_eq!(frame_fg(&terminal, "● planning"), Some(Color::Yellow), "状态条徽标");
        // 左列按来源。
        assert_eq!(frame_fg(&terminal, "你: 需求"), Some(Color::White), "属主回声白");
        assert_eq!(frame_fg(&terminal, "[pi] ⋯"), Some(Color::DarkGray), "过程行暗灰");
        assert_eq!(frame_fg(&terminal, "[pi] 计划分两步"), Some(Color::Cyan), "答复青");
        assert_eq!(frame_fg(&terminal, "[chat] 已受理"), Some(Color::Gray), "壳通知灰");
        assert_eq!(
            frame_fg(&terminal, "[orchestrator] 计划审查中"),
            Some(Color::White),
            "治理环通知白"
        );
        assert_eq!(
            frame_fg(&terminal, "[chat] 计划审查意见"),
            Some(Color::Yellow),
            "挂起意见黄"
        );
        assert_eq!(frame_fg(&terminal, "[chat] 操作失败"), Some(Color::Red), "错误红");

        // 看板态行同源着色（escalated 红）。
        let mut snap2 = sample_snapshot();
        snap2.state = "escalated".into();
        app.dashboard = Some(snap2);
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert_eq!(frame_fg(&terminal, "● escalated"), Some(Color::Red), "挂起态红");
    }
}
