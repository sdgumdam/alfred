//! chat 治理事件类型与通道（TUI S2B：println → ChatEvent 的基础设施）。
//!
//! 定位（方案 `.plans/施工方案-TUI界面.md` S2 数据接线）：`alfred chat` 治理
//! 内核（chat.rs REPL 壳 + governance.rs 治理环）的 owner 可见输出目前全靠
//! `println!` 直打 stdout；TUI 呈现层（chat_tui.rs）需要这些输出改经事件通道
//! 喂进事件循环（crossterm poll 超时窗口内汇聚，见 chat_tui.rs
//! `POLL_TIMEOUT`）。本模块是那条通道的**纯类型 + std::sync::mpsc 封装**——
//! 零 UI 依赖（不 import ratatui/crossterm）；点位替换接线是 S1 骨架落地后
//! 的汇合切片，本片不动 chat.rs / chat_tui.rs / governance.rs。
//!
//! # 覆盖对照表（ground truth = 输出点位实查，无损表达：每个点位有唯一归属）
//!
//! | 变体 | 覆盖点位 |
//! |---|---|
//! | [`ChatEvent::OwnerEcho`] | 属主输入回显（TUI 左列呈现属主消息；REPL 由终端原生回显承担，无 println 点位——接线切片为 TUI 面新增） |
//! | [`ChatEvent::PiAction`] | chat.rs `emit_process_line`：AGT 审计 tail 过程行 `[pi] ⋯ 探查/读取/探查（被治理拦截）`（`render_audit_action` 三呈现分支 ↔ [`ActionKind`]） |
//! | [`ChatEvent::PiReply`] | chat.rs `surface_planner_output`：`[pi] {答复}`（reply 直显 + conversation.json 新增 ConverseReply 轮两分支） |
//! | [`ChatEvent::ShellNotice`] | 会话壳通知（chat.rs REPL 面 `[chat]` 前缀点位，S4 前缀分职拆出）：横幅/run_dir/恢复 run/未发现 run/新建 run/已受理/当前状态/已重载/空输入×2/需求为空/会话结束/终态呈现（完成×2/放弃/新需求引导）/挂起块头行 |
//! | [`ChatEvent::OrchestratorNotice`] | governance.rs **全部** `[orchestrator]` 点位（S2b 经 `orchestrator_notice` 单一出口事件化）：每状态进入的流转状态行（`orchestrator_status_line`）、机械重跑提示（`post_apply_notices`：超时放大重跑/同契约重跑）、contract_fault 预标注、规划失败升级块、计划/执行审查宿主失败升级块（升级块保持编排器语音，与 chat.rs 挂起呈现分置，见下行） |
//! | [`ChatEvent::Error`] | 一切错误行（原点位输出流由 `stream` 标记，见 [`ErrorStream`]）——chat.rs TUI 运行失败回退（stderr）、run 初始化失败、state 持久化失败、操作失败、多挂起 run 消歧清单（头行+列表行，stderr）、行编辑初始化失败（stderr）；reviewer verdict 解析 warn（alfred-reviewer，stderr） |
//!
//! 明确排除（非事件）：`print!` 输入提示（需求/对 pi 说/挂起拍板三处 prompt
//! 文本）——TUI 输入区提示由 run 态派生（S2A 看板数据源 state.json），REPL
//! 提示留在 chat.rs；`[driver]` 状态行（main.rs run/feed/status 技术 driver
//! 子命令，非 owner 会话面）——同一模型可表达（OrchestratorNotice），不在
//! 接线范围。
//!
//! 载荷口径（定死，无例外）：正文**不含终端前缀**（`[chat]`/`[pi]`/
//! `[orchestrator]` 是 sink 的渲染关注点——REPL sink 加前缀回 stdout 逐字节
//! 等价，TUI sink 自由着色/分区）。[`ChatEvent::OrchestratorNotice`] 同口径：
//! governance.rs 侧格式化产物自带 `[orchestrator] ` 前缀，发送点
//! （`orchestrator_notice` 单一出口）负责剥离——S2a（chat.rs）+ S2b
//! （governance.rs）已落地；前缀由 sink 渲染时统一加回（TUI 左列
//! `[orchestrator] {text}`），载荷只承载纯正文。
//!
//! # 通道语义
//!
//! - **sender**：`Send + Clone`（治理内核多处持有——主循环 + planner AGT
//!   审计 tail 后台线程并发发动作事件）；`send` 吞错：接收端已 Drop → 返回
//!   `false` 不 panic（对齐 chat.rs `emit_process_line` `writeln!` 吞错的
//!   "坏管道不杀线程"语义——内核/tail 线程不能因 TUI 先退而崩）。
//! - **receiver**：TUI 事件循环 `try_recv` 非阻塞 poll 消费——空通道 →
//!   `Err(Empty)` 继续下一帧；发送端全 Drop → `Err(Disconnected)`（内核已
//!   退，TUI 可收尾）。缓冲消息先取尽才报 Disconnected（std mpsc 语义）。

use std::sync::mpsc;

use alfred_core::governance::EscalationSource;

/// planner AGT 审计过程动作的种类（ground truth = chat.rs `render_audit_action`
/// 的三个呈现分支）。
///
/// 行格式（接线后单一真源仍在 chat.rs 渲染侧，此处只承载分类）：
/// - [`ActionKind::Probe`] ↔ `("allow", "bash")` → `探查: {command}`
/// - [`ActionKind::Read`] ↔ `("allow", "read")` → `读取: {path}`
/// - [`ActionKind::Blocked`] ↔ `("deny", _)` → `探查（被治理拦截）: {command|path}`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// 探查（allow bash，命令）。
    Probe,
    /// 读取（allow read，路径）。
    Read,
    /// 被治理拦截（deny 任意工具，command 缺失回退 path）。
    Blocked,
}

/// chat 治理事件：治理内核 → 呈现层（TUI 事件循环 / REPL sink）的全部
/// owner 可见输出（覆盖对照表见模块文档）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// 属主输入回显（正文 = 属主输入原行，无前缀）。REPL 由终端原生回显承担、
    /// 无对应 println 点位；TUI 左列需要（对 pi 说的消息进对话流）。
    OwnerEcho(String),
    /// planner 过程动作行（AGT 审计 tail，`[pi] ⋯` 前缀的动作行）。
    /// `detail` = 已截断目标文本（chat.rs `truncate_action` 单一真源产物：
    /// 空白压平 + 80 字截断，中文 chars 安全）——接线时 tail 线程渲染产物
    /// 直接入载荷，格式化不搬家。
    PiAction {
        /// 动作种类（探查/读取/拦截）。
        kind: ActionKind,
        /// 已截断的目标文本（命令/路径）。
        detail: String,
    },
    /// planner 答复（正文 = 答复/计划摘要文本，`[pi] {…}` 的 `{…}` 部分；
    /// 数据源 conversation.json M4-a 语义轮次单一真源，接线切片沿用
    /// `surface_planner_output` 取数）。
    PiReply(String),
    /// 会话壳通知（chat.rs 本壳的 owner 提示音——REPL 面 `[chat]` 前缀点位）。
    ///
    /// S4 前缀分职（S2 审 P3）：这些点位 REPL 恒呈现 `[chat] {…}`，此前复用
    /// [`ChatEvent::OrchestratorNotice`] 导致 TUI 左列误标 `[orchestrator]`
    /// （同一载荷两面前缀漂移）。拆独立变体后前缀各归其主：本变体渲染
    /// `[chat]`，OrchestratorNotice 只承载 governance.rs 治理环点位（REPL
    /// `[orchestrator]`，TUI 同面前缀加回）——与 REPL 逐面（prefix×voice）
    /// 对齐。载荷口径同全局约定：去前缀纯正文。
    ShellNotice(String),
    /// 治理环编排器通知（governance.rs 治理环全部 `[orchestrator]` 点位：
    /// 流转状态行 / 机械重跑提示 / contract_fault 预标注 / 规划失败与审查
    /// 宿主失败升级块，经 `orchestrator_notice` 单一出口）。
    ///
    /// 载荷口径（单一口径，无例外）：**去前缀纯正文**——governance.rs 治理环
    /// 通知（`orchestrator_status_line` / `post_apply_notices` / 升级块）的
    /// 格式化产物自带 `[orchestrator] ` 前缀，发送点（`orchestrator_notice`
    /// 单一出口）负责剥离（S2a/S2b 已落地）。`[orchestrator]` 前缀由 sink
    /// 渲染时统一加回——REPL sink 回 stdout 逐字节等价，TUI sink 自由分区/
    /// 着色；载荷不再混装两种口径。
    OrchestratorNotice(String),
    /// 挂起拍板（升级包）：run 挂起（plan_rejected/escalated）等待属主
    /// 重试/放弃/改口。
    ///
    /// - `reason` = **已按态格式化的意见/原因正文**（含意见 label，如
    ///   `计划审查意见（打回）：…`/`执行审查意见（…）：…`）——格式化留在数据
    ///   所在地（chat.rs `present_suspension` 四分支，单一真源不搬家），事件
    ///   只承载结果文本。governance.rs 升级块不走本变体（S2b 定夺：保持
    ///   `[orchestrator]` 编排器语音，走 [`ChatEvent::OrchestratorNotice`]，
    ///   与 REPL 逐字节同面）。
    /// - `source` = 升级/打回来源，直接用 `alfred_core::governance::
    ///   EscalationSource` 类型（单一真源，消第二份字符串词表）：
    ///   `Planning`（规划失败升级）/ `PlanReview`（计划打回 + 计划审查宿主
    ///   失败升级）/ `Execution`（执行/执行审查升级）。TUI 着色/来源标签用
    ///   （S4）。
    EscalationPrompt {
        /// 已格式化的意见/原因正文。
        reason: String,
        /// 升级/打回来源（alfred_core 单一真源枚举，见变体文档）。
        source: EscalationSource,
    },
    /// 错误行（正文 = 已格式化错误文本）。`stream` 标记原点位输出流——REPL
    /// sink 按标记写回原流（逐字节等价），TUI sink 统一进对话流/状态区并
    /// 可按标记着色区分。
    Error {
        /// 已格式化的错误文本。
        message: String,
        /// 原点位输出流（stdout/stderr）。
        stream: ErrorStream,
    },
}

/// 错误行原点位输出流（ground truth = 覆盖表 Error 行点位的 println!/
/// eprintln! 实况）：REPL sink 按标记写回原流；TUI sink 不分流，仅作呈现
/// 区分依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorStream {
    /// stdout 点位（println!）：run 初始化失败 / state 持久化失败 / 操作失败。
    Stdout,
    /// stderr 点位（eprintln!）：TUI 运行失败回退 / 行编辑初始化失败回退 /
    /// 多挂起 run 消歧清单 / reviewer verdict 解析 warn。
    Stderr,
}

/// chat 治理事件通道的命名构造入口（mpsc 封装的装配点）。
///
/// 不持有状态——`new` 返回 sender/receiver 对（Rust 惯例：通道由两端持有，
/// 构造类型只做命名空间）；无界通道（UI 展示行，产出速率 = 人/审计 tail
/// 级，无需背压策略——引入 bounded 会发明未定的阻塞策略）。
pub struct ChatEventBus;

impl ChatEventBus {
    /// 建通道：返回 (sender, receiver)。sender 可 Clone 多处持有（主循环 +
    /// AGT 审计 tail 线程）；receiver 单端 poll 消费（TUI 事件循环）。
    pub fn new() -> (ChatEventSender, ChatEventReceiver) {
        let (tx, rx) = mpsc::channel();
        (ChatEventSender { tx }, ChatEventReceiver { rx })
    }
}

/// 事件发送端：`Clone` 给治理内核多处持有（主循环 + planner AGT 审计 tail
/// 后台线程 + 治理环通知出口），`Send` 跨线程。
#[derive(Clone)]
pub struct ChatEventSender {
    tx: mpsc::Sender<ChatEvent>,
}

/// Debug：通道端点无观察值（`GovernanceContext` 派生 Debug 携带它，不泄内容）。
impl std::fmt::Debug for ChatEventSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatEventSender")
    }
}

impl ChatEventSender {
    /// 发一条事件（非阻塞）。
    ///
    /// 返回 `true` = 已入队；`false` = 接收端已 Drop（吞错不 panic——发送
    /// 线程存活优先，对齐 `emit_process_line` 的"坏管道不杀线程"语义）。
    pub fn send(&self, event: ChatEvent) -> bool {
        self.tx.send(event).is_ok()
    }
}

/// 事件接收端：TUI 事件循环 poll 消费（单端）。
pub struct ChatEventReceiver {
    rx: mpsc::Receiver<ChatEvent>,
}

impl ChatEventReceiver {
    /// 非阻塞收一条。
    ///
    /// - `Ok(event)`：取到（FIFO 全序，多 sender 也保序）。
    /// - `Err(Empty)`：空通道（TUI 继续下一帧 poll）。
    /// - `Err(Disconnected)`：缓冲已取尽且发送端全 Drop（内核已退，TUI 可
    ///   收尾）。缓冲消息先取尽才报 Disconnected（std mpsc 语义）。
    pub fn try_recv(&self) -> Result<ChatEvent, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::{ActionKind, ChatEvent, ChatEventBus, ErrorStream};
    use alfred_core::governance::EscalationSource;
    use std::sync::mpsc::TryRecvError;

    /// 八变体逐一构造 + FIFO 收发（覆盖面烟测：全部变体可表达——Error 双流
    /// 标记各一条、source 用 alfred_core 枚举真源，按发送序取回；ShellNotice
    /// 与 OrchestratorNotice 分职后各一条，前缀分职契约锁定）。
    #[test]
    fn send_recv_fifo_all_variants() {
        let (tx, rx) = ChatEventBus::new();
        let sent = vec![
            ChatEvent::OwnerEcho("新需求：写 hello.txt".into()),
            ChatEvent::PiAction {
                kind: ActionKind::Probe,
                detail: "omp --help".into(),
            },
            ChatEvent::PiReply("计划分两步：先建图再执行。".into()),
            ChatEvent::OrchestratorNotice("当前状态: planning".into()),
            ChatEvent::ShellNotice("已受理：写 hello.txt".into()),
            ChatEvent::EscalationPrompt {
                reason: "计划审查意见（打回）：需求不可验收。".into(),
                source: EscalationSource::PlanReview,
            },
            ChatEvent::Error {
                message: "操作失败：容器不可用".into(),
                stream: ErrorStream::Stdout,
            },
            ChatEvent::Error {
                message: "行编辑初始化失败，回退裸读。".into(),
                stream: ErrorStream::Stderr,
            },
        ];
        for event in &sent {
            assert!(tx.send(event.clone()), "接收端存活 → 入队成功");
        }
        for expect in &sent {
            assert_eq!(rx.try_recv().as_ref(), Ok(expect));
        }
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    /// try_recv 空轮询语义：空通道 → `Empty`（非阻塞，逐帧 drain 后复归
    /// `Empty`——TUI 循环"收尽即走"依赖此语义，不会挂在空通道上）。
    #[test]
    fn try_recv_empty_and_drained() {
        let (tx, rx) = ChatEventBus::new();
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
        tx.send(ChatEvent::PiAction {
            kind: ActionKind::Read,
            detail: "/ws/Cargo.toml".into(),
        });
        assert_eq!(
            rx.try_recv().unwrap(),
            ChatEvent::PiAction {
                kind: ActionKind::Read,
                detail: "/ws/Cargo.toml".into()
            }
        );
        // 取尽后复归 Empty（非 Disconnected——sender 仍存活）。
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    /// 多 sender + 跨线程：Clone 出的句柄交后台线程（AGT 审计 tail 语义，
    /// 实证 `Send`），与主循环发的消息进同一通道，无丢无重（跨线程先后
    /// 不定 → 集合断言；单线程内保序）。
    #[test]
    fn multiple_senders_and_threads() {
        let (tx, rx) = ChatEventBus::new();
        let tail = tx.clone();
        let handle = std::thread::spawn(move || {
            assert!(tail.send(ChatEvent::PiAction {
                kind: ActionKind::Blocked,
                detail: "cat ~/.omp/runs".into(),
            }));
        });
        // 主循环侧先发两条（线程并发启动前后不定，但不影响集合断言）。
        assert!(tx.send(ChatEvent::OrchestratorNotice("执行中（沙箱容器）…".into())));
        assert!(tx.send(ChatEvent::PiReply("r1".into())));
        handle.join().unwrap();

        let mut got: Vec<String> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            got.push(format!("{event:?}"));
        }
        let mut expect = vec![
            format!(
                "{:?}",
                ChatEvent::PiAction {
                    kind: ActionKind::Blocked,
                    detail: "cat ~/.omp/runs".into(),
                }
            ),
            format!("{:?}", ChatEvent::OrchestratorNotice("执行中（沙箱容器）…".into())),
            format!("{:?}", ChatEvent::PiReply("r1".into())),
        ];
        got.sort();
        expect.sort();
        assert_eq!(got, expect, "三事件全收齐，无丢无重");
    }

    /// 断连语义：接收端先退 → `send` 吞错返回 `false` 不 panic（内核/tail
    /// 线程不能因 TUI 先退而崩）；发送端全退 → 缓冲取尽后 `Disconnected`
    /// （TUI 判内核已退收尾）。
    #[test]
    fn disconnect_semantics() {
        // 接收端先 Drop：send 返回 false（对齐 emit_process_line 吞错语义）。
        let (tx, rx) = ChatEventBus::new();
        drop(rx);
        assert!(!tx.send(ChatEvent::Error {
            message: "TUI 已退".into(),
            stream: ErrorStream::Stderr,
        }));

        // 发送端全 Drop：缓冲消息先取尽，再 Disconnected。
        let (tx, rx) = ChatEventBus::new();
        tx.send(ChatEvent::OwnerEcho("last".into()));
        drop(tx);
        assert_eq!(rx.try_recv().unwrap(), ChatEvent::OwnerEcho("last".into()));
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Disconnected);
    }
}
