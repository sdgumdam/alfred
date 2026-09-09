//! alfred 编排器状态机驱动库。
//!
//! 属主 08-31：删/回滚 alfred CLI 六命令（run/decide/status/plan-review/exec-review/
//! panel 不再作为 owner 交互入口）；保留编排器状态机（`governance.rs` 核心）为库。
//! ② 形态修复（2026-09-02）：owner 交互入口 = `alfred chat` 持续会话（`chat.rs`，
//! codux 调度的常驻 REPL，复用 `run_governance_loop` / `feed_owner_message` 库，
//! 确定性循环壳不发明治理机制）；`alfred` bin 另保留 run/feed/status 技术
//! driver 子命令（脚本/e2e 接口）。

pub mod governance;
pub mod governance_intent;
pub mod chat_events;
