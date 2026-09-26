//! 非交互孤儿恢复驱动（公共生命周期运维工具）。
//!
//! `alfred run` 总是全新 run（`init_governance_run` 不读 state.json）；
//! `alfred chat` 的 state.json 恢复需要 TTY。宿主批处理里被取消/被杀的原生
//! run（case02-a2-r1 类：治理进程死在 poll 中途，executor 驱动与容器成孤儿、
//! 无终态）缺一个非交互续跑入口。本 example 逐字复用 chat 的恢复序列
//! （`load_governance_run` → `run_governance_loop` → `persist_governance_run`），
//! 不发明治理机制：execution_step 的孤儿检测先按孤儿目录里的
//! driver.project.json 精确回收 compose 项目，再按 crashed/cancelled 语义路由
//! 终态（升级/耗尽不自动重跑取消）。
//!
//! 用法：`cargo run -p alfred-cli --example resume_run -- <run_dir>`
//! （模型配置经 ALFRED_CONFIG/环境解析，与 chat 同一 `build_governance_context`。）

use anyhow::{Context, Result};

use alfred_cli::governance::{
    build_governance_context, load_governance_run, persist_governance_run, run_governance_loop,
    state_label,
};

fn main() -> Result<()> {
    let run_dir = std::env::args()
        .nth(1)
        .context("usage: resume_run <run_dir>")?;
    let run_dir = std::path::PathBuf::from(run_dir);
    let mut run = load_governance_run(&run_dir)
        .with_context(|| format!("resume run {}", run_dir.display()))?;
    let ctx = build_governance_context(&run_dir)?;
    run_governance_loop(&mut run, &ctx)?;
    persist_governance_run(&run_dir, &run)?;
    println!(
        "[resume] state={} attempts={}/{} execution_count={}",
        state_label(run.state()),
        run.attempts_used,
        run.mechanical_budget,
        run.execution_count
    );
    Ok(())
}
