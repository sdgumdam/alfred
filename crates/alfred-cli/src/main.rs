mod decide;
mod plan;
mod run;
mod status;
mod store;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

/// 退出码约定：0 = Completed 或 decide 成功；1 = PlanRejected / Escalated 挂起，需属主拍板；
/// 2 = 参数错误、非法状态或链路错误。
pub const EXIT_OK: u8 = 0;
pub const EXIT_ESCALATED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;

#[derive(Debug, Parser)]
#[command(name = "alfred", about = "Alfred governance skeleton CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Plan a DagSpec from an OwnerRequest JSON file
    Plan {
        request: PathBuf,
        #[arg(long, default_value = "run-latest")]
        out_dir: PathBuf,
    },
    /// Run the full governance loop for an OwnerRequest, persisting state in the run dir
    Run {
        request: PathBuf,
        /// Run directory; defaults to run-<request_id> (run-<timestamp> when request_id is empty)
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Print the persisted state of a run directory
    Status {
        /// Run directory containing state.json
        run_dir: PathBuf,
    },
    /// Apply an owner decision to a suspended run (plan_rejected / escalated)
    Decide {
        /// Run directory containing state.json
        run_dir: PathBuf,
        /// retry | revise-contract | abandon
        decision: String,
    },
}

fn main() -> ExitCode {
    match Cli::parse() {
        Cli { command: Command::Plan { request, out_dir } } => plan::run(&request, &out_dir),
        Cli { command: Command::Run { request, out_dir } } => {
            run::run(&request, out_dir.as_deref())
        }
        Cli { command: Command::Status { run_dir } } => status::run(&run_dir),
        Cli { command: Command::Decide { run_dir, decision } } => decide::run(&run_dir, &decision),
    }
}
