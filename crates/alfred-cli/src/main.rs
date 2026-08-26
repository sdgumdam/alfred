//! alfred CLI 入口（R1：`alfred run` 雏形；R2：`alfred plan-review`）。

mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "alfred",
    version,
    about = "alfred 治理骨架命令行",
    long_about = "alfred：阻力最小路径治理骨架。R1 实现执行侧：`alfred run --request <request.json>` \
                  在真容器里跑 pi 并采集产物；R2 加执行审查（ExecVerdict scorer）+ \
                  `alfred plan-review`（计划忠实度审查）。"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 运行一个执行节点（真容器 pi，产物落 run 目录；含执行审查 scorer）
    Run(commands::run::RunArgs),
    /// 计划审查：判 DagSpec 是否忠实于 OwnerRequest（PlanVerdict 落 run 目录）
    PlanReview(commands::plan_review::PlanReviewArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run(args) => commands::run::run(args),
        Commands::PlanReview(args) => commands::plan_review::plan_review(args),
    }
}
