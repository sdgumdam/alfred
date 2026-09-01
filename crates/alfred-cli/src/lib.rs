//! alfred 编排器状态机驱动库。
//!
//! 属主 08-31：删/回滚 alfred CLI 六命令（run/decide/status/plan-review/exec-review/
//! panel 不再作为 owner 交互入口）；保留编排器状态机（`governance.rs` 核心）为库，
//! 供 `alfred` bin（codux 可调度 CLI driver：run/feed/status）驱动治理环
//! （`run_governance_loop` / `feed_owner_message`）。

pub mod governance;
