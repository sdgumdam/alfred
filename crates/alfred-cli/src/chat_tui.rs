//! alfred chat TUI 呈现层（S1 骨架）。
//!
//! 定位（方案 `.plans/施工方案-TUI界面.md`）：治理内核零改动，本模块是
//! `alfred chat` 的终端呈现层。S1 只做骨架——ratatui 事件循环 + 四区布局
//! （顶状态条 / 左列"对话"过程流 / 右列"状态"看板 / 底部"输入"框）+ 自实现
//! 多行输入编辑；数据接线（过程流 / 看板 / 治理事件）是 S2，本片输入提交只
//! 回显到左列滚动区。
//!
//! - **终端生命周期**：`ratatui::try_init` / `try_restore` 单一真源（raw mode
//!   + 备用屏 + panic hook 兜底恢复终端——panic 冒泡前先恢复，属主终端不留
//!   坏状态）。正常/错误路径都先恢复再返回；初始化失败由 cmd_chat 降级回
//!   REPL（见 chat.rs 降级门）。
//! - **事件循环**：crossterm 同步 `poll(timeout)+read`（按键/resize 事件
//!   驱动；draw 为 ratatui 双缓冲差分，无变化帧零输出）。方案原文写
//!   EventStream——那是异步 API，工作区无 tokio/futures，为骨架引入异步
//!   运行时不合算；同步 poll 等价达成按键/resize 语义，S2 治理事件经
//!   std 通道在同一 poll 超时窗口汇聚，范式不变。resize 事件无需特判：
//!   下一轮 draw 的 `Terminal::autoresize` 按新尺寸重排。
//! - **多行输入自实现**（不引 tui-textarea：需求面只有字符/退格/回车提交/
//!   ↑↓历史/Ctrl-J 换行，百行内可控且光标语义完全自明）。光标 = (行, char
//!   列) 坐标——列按 char 计（中文安全，字节换算集中 [`char_to_byte`]）；
//!   折行 CJK 宽度感知（unicode-width：ratatui/rustyline 传递依赖共 0.2.x
//!   单副本，零新增编译重量），渲染与光标定位共用 [`wrap_segments`] 单一
//!   真源。
//! - **退出语义对齐 REPL**：Ctrl-D 空缓冲退出 / Ctrl-C 恒退出（rustyline
//!   时代 Eof/Interrupted → 会话结束，同语义；raw mode 下 Ctrl-C 不产生
//!   SIGINT，作为按键处理）；Ctrl-D 非空按行编辑惯例删光标处字符。空提交
//!   打 `[chat] 空输入已忽略。`（与 REPL 文案逐字对齐）。
//! - **降级门**：[`tui_supported`]（stdin&&stdout 双 tty 且 TERM 非空非
//!   dumb）——非 tty（管道/脚本）走既有 REPL 裸读路径，e2e 管道行为逐字节
//!   不变是硬底线。

use std::env;
use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 事件轮询超时：无输入事件时静默等待（draw 差分零输出，不闪烁）；S2 治理
/// 事件将经通道在超时窗口内汇聚进同一循环。
const POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// 状态条底色（S4 治理态着色前的骨架底色）。
const STATUS_BG: Color = Color::DarkGray;

/// TUI 降级门（cmd_chat 入口分流判定）：交互双 tty 且 TERM 有效 → TUI；
/// 否则（管道/重定向/dumb 终端/TERM 缺失）降级既有 REPL 路径。TERM 判定拆
/// 纯函数 [`term_enables_tui`]（可测）。
pub fn tui_supported() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && term_enables_tui(env::var("TERM").ok().as_deref())
}

/// TERM 判定：非空且非 `dumb`（dumb 无光标寻址，TUI 无处渲染）；未设置视
/// 同 dumb（POSIX 未定义行为的终端不赌）。
fn term_enables_tui(term: Option<&str>) -> bool {
    match term {
        Some(t) => !t.is_empty() && t != "dumb",
        None => false,
    }
}

/// TUI 主入口：初始化（raw mode + 备用屏 + panic hook）→ 事件循环 → 无论
/// 正常/错误路径先恢复终端再返回（恢复失败不掩盖主结果）。正常退出打一行
/// 会话结束（与 REPL 尾行文案对齐）。
pub fn run() -> Result<()> {
    let mut terminal = ratatui::try_init().context("TUI 终端初始化失败")?;
    let mut app = TuiApp::new();
    let result = event_loop(&mut terminal, &mut app);
    let _ = ratatui::try_restore();
    result?;
    println!("[chat] 会话结束。");
    Ok(())
}

/// 事件循环：draw（差分渲染）→ poll 键盘/resize → 分发。鼠标/焦点/粘贴
/// 不消费（S1 无对应交互面）。
fn event_loop(terminal: &mut DefaultTerminal, app: &mut TuiApp) -> Result<()> {
    while !app.exit {
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

/// TUI 应用状态（S1：输入编辑器 + 回显消息列表；S2 换数据源——过程流+看板）。
struct TuiApp {
    input: InputEditor,
    messages: Vec<String>,
    exit: bool,
}

impl TuiApp {
    fn new() -> Self {
        Self {
            input: InputEditor::new(),
            messages: Vec::new(),
            exit: false,
        }
    }

    /// 键 → 状态转移。raw mode 下 Ctrl-C/Ctrl-D 以按键到达（无 SIGINT）：
    /// Ctrl-C 恒退出、Ctrl-D 空缓冲退出（对齐 REPL 会话结束语义）；Enter
    /// 恒为提交（治理消息边界，S1 仅回显左列）；Ctrl-J 换行（LF 键序在 raw
    /// mode 下即 Ctrl-J，与 Enter 的 CR 区分）；其余可见字符进编辑器。
    /// 只处理按下/自动重复（Windows 终端按下与释放都发事件）。
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
            KeyCode::End => self.input.end(),
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input.insert_char(c);
            }
            _ => {}
        }
    }

    /// 提交：S1 骨架只回显左列（S2 起换治理路由）。空提交与 REPL 同文案
    /// 提示；非空回显 trim 后文本。
    fn submit(&mut self) {
        let text = self.input.submit();
        if text.trim().is_empty() {
            self.messages.push("[chat] 空输入已忽略。".to_string());
        } else {
            self.messages.push(text.trim().to_string());
        }
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

/// 软折行（CJK 宽度感知）：一条逻辑行按显示宽 `width` 折成显示段序列，每段
/// (段首 char 偏移, 段文本)。渲染与光标定位共用这一单一真源。宽度 0（防御：
/// 极小终端边框内宽为 0）不折行单段返回；单字符宽 > 总宽（极小终端放 CJK）
/// 不可再分——独占一段。
fn wrap_segments(text: &str, width: usize) -> Vec<(usize, String)> {
    if width == 0 {
        return vec![(0, text.to_string())];
    }
    let mut segs = Vec::new();
    let (mut seg, mut start, mut w) = (String::new(), 0usize, 0usize);
    for (ci, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if !seg.is_empty() && w + cw > width {
            segs.push((start, std::mem::take(&mut seg)));
            start = ci;
            w = 0;
        }
        seg.push(ch);
        w += cw;
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
            let w: usize = seg.chars().take(off).map(|c| c.width().unwrap_or(0)).sum();
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
    cursor: (usize, usize),
}

fn input_display(editor: &InputEditor, width: usize) -> InputDisplay {
    let mut rows = Vec::new();
    let mut cursor = (0, 0);
    for (li, line) in editor.lines.iter().enumerate() {
        let segs = wrap_segments(line, width);
        if li == editor.row {
            let (ri, w) = cursor_segment_pos(&segs, editor.col);
            cursor = (rows.len() + ri, w);
        }
        rows.extend(segs.into_iter().map(|(_, s)| s));
    }
    InputDisplay { rows, cursor }
}

/// 左列显示行（S1：提交回显；S2：过程流）：消息按内宽折行，只保留最后
/// `height` 行——自动跟随底部（新内容恒可见，PgUp/PgDn 回溯是 S4）。
fn conversation_rows(messages: &[String], width: usize, height: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for msg in messages {
        for line in msg.split('\n') {
            rows.extend(wrap_segments(line, width).into_iter().map(|(_, s)| s));
        }
    }
    let start = rows.len().saturating_sub(height);
    rows.split_off(start)
}

/// 四区渲染：顶状态条（占位 "alfred+版本"，S2 换 run_id+治理态徽标）/ 左列
/// "对话"（S1：提交回显）/ 右列 "状态"（空占位，S2：看板）/ 底部 "输入"
/// （多行编辑，光标可见）。输入框内容宽与终端宽同源（框横贯全宽）：先定
/// 折行再定布局，无循环依赖；框高随内容增长（上限半屏），内容超高时可视
/// 窗口贴底、光标行越窗顶则上移保光标可见。
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

    // ── 顶状态条：标题 + 按键提示，整行底色（Paragraph.style 铺满区域） ──
    let title = format!(" alfred v{} ", env!("CARGO_PKG_VERSION"));
    let hint = "Enter 提交 · Ctrl-J 换行 · ↑↓ 历史 · Ctrl-D/Ctrl-C 退出 ";
    let pad = status_area
        .width
        .saturating_sub(title.width() as u16 + hint.width() as u16);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title,
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad as usize)),
            Span::styled(hint, Style::new().fg(Color::Gray)),
        ]))
        .style(Style::new().bg(STATUS_BG)),
        status_area,
    );

    // ── 左列：对话（S1 提交回显，自动跟随底部） ──
    let conv_block = Block::bordered().title(" 对话 ");
    let conv_inner = conv_block.inner(conv_area);
    let conv_lines = conversation_rows(
        &app.messages,
        conv_inner.width as usize,
        conv_inner.height as usize,
    )
    .into_iter()
    .map(Line::from)
    .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(conv_lines).block(conv_block), conv_area);

    // ── 右列：状态看板（空占位，S2 接线 state/dagspec/verdicts/ws） ──
    f.render_widget(
        Paragraph::new("").block(Block::bordered().title(" 状态 ")),
        panel_area,
    );

    // ── 底部：输入框（多行编辑 + 光标） ──
    let input_block = Block::bordered().title(" 输入 ");
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

    /// 构造无修饰按键事件。
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// 构造 Ctrl 组合键事件。
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
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

    /// 提交回显左列 / 空提交提示行（与 REPL 文案逐字对齐）/ Ctrl-D 空退出、
    /// 非空删字符 / Ctrl-C 恒退出。
    #[test]
    fn app_submit_echo_and_exit_semantics() {
        let mut app = TuiApp::new();
        for c in "需求甲".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.messages, vec!["需求甲".to_string()]);
        app.handle_key(ctrl('d')); // 提交后缓冲空 → 退出
        assert!(app.exit);

        let mut app2 = TuiApp::new();
        app2.handle_key(key(KeyCode::Enter)); // 空提交
        assert_eq!(app2.messages, vec!["[chat] 空输入已忽略。".to_string()]);
        assert!(!app2.exit);

        let mut app3 = TuiApp::new();
        app3.handle_key(key(KeyCode::Char('a')));
        app3.handle_key(key(KeyCode::Char('b')));
        app3.handle_key(key(KeyCode::Left)); // 光标移到 'b' 前
        app3.handle_key(ctrl('d')); // 非空：前删 'b'，不退出
        assert!(!app3.exit);
        assert_eq!(app3.input.text(), "a");
        app3.handle_key(ctrl('c')); // Ctrl-C 恒退出
        assert!(app3.exit);
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
        assert_eq!(d.rows, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!(d.cursor, (1, 2)); // 行尾 = 末段末

        let mut ed2 = InputEditor::new();
        for c in "ab".chars() {
            ed2.insert_char(c);
        }
        ed2.insert_newline();
        for c in "cd".chars() {
            ed2.insert_char(c);
        }
        let d2 = input_display(&ed2, 2);
        assert_eq!(d2.rows, vec!["ab".to_string(), "cd".to_string()]);
        assert_eq!(d2.cursor, (1, 2)); // 逻辑行 1 的行尾

        let mut ed3 = InputEditor::new();
        for c in "超超".chars() {
            ed3.insert_char(c);
        }
        ed3.left(); // 光标在两字之间（显示宽 2 处）
        let d3 = input_display(&ed3, 4);
        assert_eq!(d3.rows, vec!["超超".to_string()]);
        assert_eq!(d3.cursor, (0, 2));
    }

    /// 左列自动跟随底部：只保留最后可视行数。
    #[test]
    fn conversation_follows_bottom() {
        let msgs: Vec<String> = (0..50).map(|i| format!("行{i}")).collect();
        let rows = conversation_rows(&msgs, 10, 3);
        assert_eq!(rows, vec!["行47".to_string(), "行48".to_string(), "行49".to_string()]);
        // 多行消息按行折行展开后同样只留尾部
        let rows2 = conversation_rows(&["a\nb\nc".to_string()], 10, 2);
        assert_eq!(rows2, vec!["b".to_string(), "c".to_string()]);
    }

    // ── 整帧渲染（TestBackend 黑盒：帧内容 + 光标落点） ──

    /// 四区骨架：状态条（alfred+版本）/ 左列"对话"（回显可见）/ 右列"状态"
    /// 空占位 / 输入框（打字可见 + 光标落输入框内）。
    #[test]
    fn render_four_zones_and_cursor() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut app = TuiApp::new();
        for c in "hi".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        let text = frame_text(&terminal);
        assert!(text.contains(&format!("alfred v{}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("对话"));
        assert!(text.contains("状态"));
        assert!(text.contains("输入"));
        assert!(text.contains("hi"), "输入框应显示已打字内容");

        // 光标：输入框在底部 3 行（80x24 → 状态条 1 + 主体 20），内容区
        // (1, 22)，"hi" 后光标在 x=3。
        assert!(terminal.backend().cursor_visible());
        terminal.backend_mut().assert_cursor_position((3, 22));

        // 提交回显到左列后，输入框清空、光标回内容区行首
        app.handle_key(key(KeyCode::Enter));
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text2 = frame_text(&terminal);
        assert!(text2.contains("hi"), "提交后回显左列");
        terminal.backend_mut().assert_cursor_position((1, 22));
    }
}
