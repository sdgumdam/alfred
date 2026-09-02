//! alfred-executor：执行侧（R1）。
//!
//! 职责（实施计划 R1 交付，三容器 Inspect 统一管）：
//! 1. `task_gen`：Rust 生成 Inspect 容器管理驱动脚本（`driver.py`，非 eval Task）。
//! 2. `compose_gen`：按 run 目录生成沙箱 compose（network none + 挂载面矩阵）。
//! 3. `driver`：spawn `python3 driver.py`（Inspect 容器管理接口：docker compose
//!    起容器 + sandbox_agent_bridge 代发 + exec_remote 驱动 pi）→ 轮询
//!    `driver.done.json` done 记录。
//! 4. `artifact`：容器 workspace 卷的文件比对（前后快照 diff）。
//! 5. `config`：R1 最小 executor 配置（roles.executor → 模型 → provider）。
//!
//! P1/P2 审计约束（R0报告）：
//! - 执行驱动进程只注入 executor 一个 provider 凭据（经 env：`{PROVIDER}_API_KEY`
//!   + `ALFRED_EXEC_API_KEY`，env_clear + 白名单，不进 argv），容器内经桥只能
//!   解析 executor 模型。
//! - forward_generation_config 默认 False；宿主 Model config 未显式配置 maxTokens
//!   时缺省 8192（glm-5.2 是 reasoning 模型，4096 会被思考吃光），显式配置的值
//!   （含小值）原样尊重。
//! - 完成判定看 driver.done.json done 记录；进程消失无 done = crash。

pub mod artifact;
pub mod compose_gen;
pub mod config;
pub mod driver;
pub mod run;
pub mod task_gen;
