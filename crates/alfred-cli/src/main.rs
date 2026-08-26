//! alfred CLI 入口（R1：`alfred run` 雏形；R2：`alfred plan-review`；R3：
//! 治理环 `run` 完整化 + `decide` 续跑 + `status`）。

mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "alfred",
    version,
    about = "alfred 治理骨架命令行",
    long_about = "alfred：阻力最小路径治理骨架。R3 实现治理环闭环：`alfred run`（request→规划→计划审查→执行→执行审查→分级路由→挂起态）、\
                  `alfred decide`（属主拍板后从 state.json 续跑）、`alfred status`（只读看状态）。"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 运行治理环（request→规划→审查→执行→路由→挂起/完成）
    Run(commands::run::RunArgs),
    /// 计划审查：判 DagSpec 是否忠实于 OwnerRequest（PlanVerdict 落 run 目录）
    PlanReview(commands::plan_review::PlanReviewArgs),
         /// 属主拍板（retry/revise/abandon）并从挂起态续跑
     Decide(commands::decide::DecideArgs),
     /// 决策面板 RPC：属主会话 pi 发三选项决策卡 → 终端拍板 → 调 decide 续跑
     Panel(commands::panel::PanelArgs),
     /// 只读查看治理环状态
     Status(commands::status::StatusArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run(args) => commands::run::run(args),
        Commands::PlanReview(args) => commands::plan_review::plan_review(args),
                 Commands::Decide(args) => commands::decide::decide(args),
         Commands::Panel(args) => commands::panel::panel(args),
         Commands::Status(args) => commands::status::status(args),
    }
}
