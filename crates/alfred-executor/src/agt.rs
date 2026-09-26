//! AGT 拦写层共享真源（属主拍板：**默认启用**）。
//!
//! 三容器（executor/planner/reviewer）同一机制、角色策略不同：
//! `prepare_agt_work` 落策略到 `<work>/agt/`（策略 ro + 审计子目录 rw 由 compose
//! 生成层挂 `/tmp/.agt`），容器内 pi 经 `-e` 加载扩展拦 `tool_call`（越界写 /
//! rm -rf / sudo / 秘密读取）并落审计 JSONL（宿主可见）。
//!
//! 解析语义（[`resolve_agt_source`]，三容器共用）：
//! 1. `ALFRED_AGT_DISABLE=1` → 关（opt-out，测试/特殊场景；优先级最高）；
//! 2. `ALFRED_AGT_DIR=<dir>` → 显式策略目录（含 agt-policy.ts + policy.json，
//!    沿用原 opt-in 覆盖面，e2e 显式设目录的用例不受影响）；
//! 3. 都未设 → **内置默认策略**（`docker/agt/<role>/policy.json` + 共享
//!    `docker/agt/agt-policy.ts` 编译期内嵌，随二进制分发——生产跑不需要外部目录）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 内置默认资产（单一真源：`docker/agt/`，编译期内嵌）。
pub mod assets {
    /// pi 扩展（三角色共用同一求值核心 + tool_call 拦截）。
    pub const EXTENSION_TS: &str = include_str!("../../../docker/agt/agt-policy.ts");
    /// executor 沙箱边界策略（workspace-write-only / no-sudo / recursive-delete /
    /// host-secret-read / no-host-path-touch）。
    pub const EXECUTOR_POLICY: &str = include_str!("../../../docker/agt/executor/policy.json");
    /// planner 拦写策略（写 /workspace 拒、/outputs 放行）。
    pub const PLANNER_POLICY: &str = include_str!("../../../docker/agt/planner/policy.json");
    /// reviewer 拦写策略（写 /workspace 拒、/outputs 放行）。
    pub const REVIEWER_POLICY: &str = include_str!("../../../docker/agt/reviewer/policy.json");
}

/// AGT 源（三容器 resolve 语义单一真源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgtSource {
    /// 显式关闭（`ALFRED_AGT_DISABLE=1`）：不挂 AGT、不加载扩展。
    Off,
    /// 内置默认策略（`docker/agt/` 编译期内嵌，随二进制分发）。
    Builtin,
    /// 显式策略目录（`ALFRED_AGT_DIR`，含 agt-policy.ts + policy.json）。
    Dir(PathBuf),
}

/// AGT 源解析（三容器共用语义）：
/// `ALFRED_AGT_DISABLE=1` 显式关闭（优先级最高，压过显式目录）；
/// 其次 `ALFRED_AGT_DIR`（非空，空串等价未设）显式目录；
/// 都未设 → 内置默认策略（属主拍板：AGT 默认启用）。
pub fn resolve_agt_source() -> AgtSource {
    if std::env::var("ALFRED_AGT_DISABLE").as_deref() == Ok("1") {
        return AgtSource::Off;
    }
    match std::env::var("ALFRED_AGT_DIR") {
        Ok(dir) if !dir.is_empty() => AgtSource::Dir(PathBuf::from(dir)),
        _ => AgtSource::Builtin,
    }
}

/// 执行侧 AGT 源（`--executor-agt-dir` 专用作用域，入 `GovernanceOptions`
/// 落 state.json）：`Some(dir)` → executor 持该 run-scoped Dir 策略，**不**改
/// planner/reviewer 的 `resolve_agt_source()`（A1/A2 消融下 reviewer 保持内
/// 置源，原守卫强制不变；消融链因此可同时满足 executor 边界与 reviewer
/// 消融强制）。`None` → 回退共享 env 解析（无旗标旧调用行为不变）。
///
/// `ALFRED_AGT_DISABLE=1` 全局关闭优先级不变：显式目录与之冲突 = 显式
/// 拒绝（fail-closed），旗标不得绕过全局关闭。
pub fn executor_agt_source(explicit_dir: Option<&Path>) -> anyhow::Result<AgtSource> {
    if let Some(dir) = explicit_dir {
        if std::env::var("ALFRED_AGT_DISABLE").as_deref() == Ok("1") {
            anyhow::bail!(
                "executor AGT dir {} 与 ALFRED_AGT_DISABLE=1 冲突（全局关闭优先；\
                 拒绝经 --executor-agt-dir 绕过）",
                dir.display()
            );
        }
        return Ok(AgtSource::Dir(dir.to_path_buf()));
    }
    Ok(resolve_agt_source())
}

/// AGT 拦写层准备（三容器同一范式）：把策略 + 扩展落到 `<work>/agt/`，建审计
/// 子目录 `audit/`（rw 挂载源，审计 JSONL 落宿主）。
///
/// - [`AgtSource::Off`] → `Ok(None)`（不建目录、不挂载）；
/// - [`AgtSource::Builtin`] → 写内嵌默认资产（`builtin_policy` 为角色策略，
///   扩展共用 [`assets::EXTENSION_TS`]）；
/// - [`AgtSource::Dir`] → 从显式目录拷贝 `agt-policy.ts` + `policy.json`。
pub fn prepare_agt_work(
    work: &Path,
    source: &AgtSource,
    builtin_policy: &str,
) -> Result<Option<PathBuf>> {
    let AgtSource::Dir(src) = source else {
        if *source == AgtSource::Off {
            return Ok(None);
        }
        return stage_builtin(work, builtin_policy);
    };
    let dest = work.join("agt");
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("create agt dir {}", dest.display()))?;
    std::fs::create_dir_all(dest.join("audit"))
        .with_context(|| format!("create agt audit dir {}", dest.join("audit").display()))?;
    std::fs::copy(src.join("agt-policy.ts"), dest.join("agt-policy.ts")).with_context(|| {
        format!(
            "copy agt extension {} -> {}",
            src.join("agt-policy.ts").display(),
            dest.join("agt-policy.ts").display()
        )
    })?;
    std::fs::copy(src.join("policy.json"), dest.join("policy.json")).with_context(|| {
        format!(
            "copy agt policy {} -> {}",
            src.join("policy.json").display(),
            dest.join("policy.json").display()
        )
    })?;
    Ok(Some(dest))
}

/// 内置默认资产落盘（Builtin 分支）：写内嵌策略 + 共享扩展到 `<work>/agt/`。
fn stage_builtin(work: &Path, builtin_policy: &str) -> Result<Option<PathBuf>> {
    let dest = work.join("agt");
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("create agt dir {}", dest.display()))?;
    std::fs::create_dir_all(dest.join("audit"))
        .with_context(|| format!("create agt audit dir {}", dest.join("audit").display()))?;
    std::fs::write(dest.join("policy.json"), builtin_policy)
        .with_context(|| format!("write builtin agt policy {}", dest.join("policy.json").display()))?;
    std::fs::write(dest.join("agt-policy.ts"), assets::EXTENSION_TS).with_context(|| {
        format!(
            "write builtin agt extension {}",
            dest.join("agt-policy.ts").display()
        )
    })?;
    Ok(Some(dest))
}
