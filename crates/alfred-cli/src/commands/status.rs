//! `alfred status` 子命令（§3.2 属主视角看文本输出）。
//!
//! 只读：读治理环 state.json，打印当前状态、重跑预算、计划/执行审查结论、
//! 会话文档摘要。不做看板。

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use super::governance::{load_governance_run, state_label};

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// 治理环运行目录（含 state.json）。
    #[arg(long)]
    pub run_dir: PathBuf,
}

pub fn status(args: StatusArgs) -> Result<()> {
    let run = load_governance_run(&args.run_dir)?;
    println!("run_id        : {}", run.run_id);
    println!("state         : {}", state_label(run.state()));
    println!(
        "attempts      : {}/{} (mechanical rerun budget)",
        run.attempts_used, run.mechanical_budget
    );
    println!("request       : {} — {}", run.request.id, run.request.title);
    if let Some(dag) = &run.dagspec {
        println!(
            "dagspec       : {} nodes ({})",
            dag.nodes.len(),
            dag.nodes
                .iter()
                .map(|n| format!("{}:{}", n.id, n.summary))
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        println!("dagspec       : (none)");
    }
    if let Some(v) = run.plan_verdicts.last() {
        println!(
            "plan verdict  : {} ({})",
            if v.pass { "PASS" } else { "FAIL" },
            truncate(&v.reason, 200)
        );
    }
    if let Some(v) = run.exec_verdicts.last() {
        println!(
            "exec verdict  : {:?} (failure_class={:?})",
            v.value, v.failure_class
        );
        println!("  rationale  : {}", truncate(&v.explanation, 200));
    }
    println!(
        "session doc   : {} files, {} conclusions, {} review summaries",
        run.session_doc.key_file_paths.len(),
        run.session_doc.key_conclusions.len(),
        run.session_doc.review_summary.len()
    );
    if let Some(m) = &run.owner_message {
        println!("owner message : {}", truncate(m, 120));
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
