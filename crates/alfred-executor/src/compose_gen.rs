//! 沙箱 compose 生成（per-run）。
//!
//! 环境事实（实施计划 §三 E1/E3）：
//! - `docker run -v` 相对路径会被静默解析为 named volume → 挂载路径必须先
//!   canonicalize 成绝对路径。
//! - macOS `/tmp` 是软链、colima 只共享 `~` → run 目录必须位于 `~` 之下，
//!   canonicalize 后路径才与 docker/报错信息一致。
//!
//! R6a（三容器挂载矩阵）：`generate_executor_compose` 落矩阵 §1.1 executor 行——
//! `workspace_subdirs` 子集投影（非空挂载，R6e 块B：空声明防御性报错）+ 参考卷 ro
//! + AGT 挂载（planner/reviewer 已宿主 pi 化，无 compose；reviewer 模板句由
//!   reviewer 线处置）。

use std::path::{Path, PathBuf};

use alfred_core::contract::VolumeMount;
use anyhow::{bail, Context, Result};
use serde::Serialize;

#[derive(Serialize)]
struct ComposeFile {
    services: Services,
}

#[derive(Serialize)]
struct Services {
    default: Service,
}

#[derive(Serialize)]
struct Service {
    image: String,
    command: String,
    init: bool,
    network_mode: String,
    stop_grace_period: String,
    volumes: Vec<String>,
}

/// 容器内工作区路径（固定，执行驱动专用挂载）。
pub const CONTAINER_WORKSPACE_DIR: &str = "/workspace";

/// Nodes may use only the network already bound by the owner. `false`
/// always means network none; it is never an alias for a task bridge.
pub fn validate_task_network(network: bool, compose: Option<&Path>) -> Result<()> {
    let enabled = if let Some(path) = compose {
        let doc: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;
        let service = doc.get("services").and_then(|v| v.get("default"))
            .and_then(|v| v.as_mapping())
            .context("task env compose: services.default mapping missing")?;
        match service.get(&serde_yaml::Value::String("network_mode".into())) {
            None => true, // Compose default bridge, including sidecar DNS.
            Some(serde_yaml::Value::String(mode)) if !mode.contains("${") => mode != "none",
            _ => bail!("task env compose: network_mode must be a literal string"),
        }
    } else {
        false
    };
    if network != enabled {
        bail!("sandbox.network={network} conflicts with owner-bound environment (network enabled={enabled}); revise the plan, never silently widen permissions");
    }
    Ok(())
}

/// executor 容器挂载参数（R6a：矩阵 §1.1 executor 行落码）。
///
/// - `workspace_subdirs`：契约声明的工作区子目录（相对持久 ws）。**非空必挂**
///   （R6e 块B：executor ws 挂载非空保证）；空 = 防御性报错（计划审查应打回
///   重规划，executor 不静默跳过、不静默挂全量）。
/// - `ref_volumes`：只读参考卷（`SandboxProfile.volumes`，档案声明）。
/// - `agt_dir`：AGT 策略目录（policy.json/agt-policy.ts，挂到 `/tmp/.agt`，ro）。
/// - `agt_audit_dir`：AGT 审计输出子目录（挂到 `/tmp/.agt/audit`，rw——
///   审计 JSONL 落此）。策略与审计拆开挂载：agent 可写审计但不可改策略（R6a）。
#[derive(Debug, Clone, Default)]
pub struct ExecutorMounts {
    pub workspace_subdirs: Vec<String>,
    pub ref_volumes: Vec<VolumeMount>,
    pub agt_dir: Option<PathBuf>,
    pub agt_audit_dir: Option<PathBuf>,
    /// Native pi sessions, retained independently of workspace and AGT policy.
    pub sessions_dir: Option<PathBuf>,
}

/// 校验 workspace subdir 声明：必须相对、非空、不含 `.`/`..`（防 rw 挂载逃逸持久 ws，
/// 把 rw 挂载静默换基到宿主任意目录）。`generate_executor_compose` 与 run.rs 预建
/// 子目录共用（单一真源，代码质量红线 1）。
pub fn validate_workspace_subdir(sub: &str) -> Result<()> {
    let sub_path = Path::new(sub);
    if sub.is_empty() || sub_path.is_absolute() {
        bail!("workspace subdir must be a relative path, got: '{sub}'（绝对/空路径禁止）");
    }
    if sub_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
    {
        bail!("workspace subdir must not contain '..' or '.': '{sub}'（越界路径禁止）");
    }
    Ok(())
}

/// 校验 workspace_subdirs 列表声明：逐项 [`validate_workspace_subdir`] + 首子目录名
/// 重复声明拒绝（A4 挂载锚窄边界）。
///
/// 挂载语义（task_gen::mount_anchor_prompt）：workspace_subdirs[0] 直接挂为执行者
/// /workspace 根，锚断言「/workspace 下不存在同名嵌套子目录」。首名若在其余位
/// 重复声明，compose 会把同一宿主目录双挂载（/workspace 与 /workspace/<同名>
/// 指向同一宿主目录）——锚断言与执行者所见矛盾（执行者透过挂载看见
/// /workspace/<同名>/）。显式报错防重复挂载，不静默去重（重复声明是计划缺陷，
/// 计划审查应打回重规划）。`validate_executor_sandbox` 与
/// `generate_executor_compose` 共用（单一真源，代码质量红线 1）。
pub fn validate_workspace_subdirs(subs: &[String]) -> Result<()> {
    for sub in subs {
        validate_workspace_subdir(sub)?;
    }
    if let Some(first) = subs.first() {
        if subs.iter().skip(1).any(|s| s == first) {
            bail!(
                "workspace_subdirs 首子目录名 '{first}' 在其余位重复声明：重复项与首挂载（/workspace 根）指向同一宿主目录造成双挂载，且与挂载锚「/workspace 下不存在同名嵌套子目录」矛盾（拒绝重复声明，不静默去重）"
            );
        }
    }
    Ok(())
}

/// 校验只读参考卷声明（9/3 方案②，放行条件真源——`validate_executor_sandbox`
/// 与 `generate_executor_compose` 共用，单一真源）：
///
/// - `mode` 必须为 `"ro"`：参考卷一律只读（契约 §2.5"一律只读、不 cp 进工作区"；
///   缺省 ro 由 serde default 保证，显式非 ro 值在此拒绝）。
/// - `host_path` 必须绝对且存在于宿主：相对路径被 docker 静默变 named volume
///   （E1 防呆）；宿主材料不存在 = 计划缺陷（防御性报错，不静默跳过挂载——
///   防"申请的参考材料没生效"）。
/// - `container_path` 必须绝对、非空、不含 `..`/`.`（挂载点合法性；禁 `/workspace`
///   与 `/tmp/.agt` 保留挂载点冲突——工作区 rw 面与 AGT 策略面不被参考卷遮蔽）。
pub fn validate_ref_volume(vol: &VolumeMount) -> Result<()> {
    if vol.mode != "ro" {
        bail!(
            "ref volume mode must be 'ro' (参考卷一律只读), got: '{}'",
            vol.mode
        );
    }
    let host = Path::new(&vol.host_path);
    if !host.is_absolute() {
        bail!(
            "ref volume host_path must be absolute: {} (E1: relative -v silently becomes a named volume)",
            vol.host_path
        );
    }
    if !host.exists() {
        bail!(
            "ref volume host_path does not exist on host: {}（宿主参考材料缺失 = 计划缺陷，拒绝静默跳过）",
            vol.host_path
        );
    }
    let target = Path::new(&vol.container_path);
    if vol.container_path.is_empty() || !target.is_absolute() {
        bail!(
            "ref volume container_path must be an absolute path, got: '{}'",
            vol.container_path
        );
    }
    if target
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir))
    {
        bail!(
            "ref volume container_path must not contain '..' or '.': '{}'",
            vol.container_path
        );
    }
    for reserved in ["/workspace", "/tmp/.agt", "/tmp/.alfred-sessions"] {
        if target == Path::new(reserved) || target.starts_with(reserved) {
            bail!(
                "ref volume container_path '{}' conflicts with reserved mount point '{reserved}'",
                vol.container_path
            );
        }
    }
    Ok(())
}

/// 生成 executor 容器 compose：network none + `workspace_subdirs` 投影 + 参考卷 ro
/// + AGT 挂载（R6a：矩阵 §1.1 executor 行落码）。
///
/// 挂载布局（对齐方案 v2 §二.1 executor 示例）：
/// - 首个 subdir → `/workspace`（rw，契约声明的子目录即执行者工作区根）；
/// - 其余 subdir → `/workspace/<subdir>`（rw）；
/// - 参考卷 → `<container_path>`（ro，按 `VolumeMount` 声明）；
/// - AGT 策略目录 → `/tmp/.agt`（ro，agent 不可改策略）；
/// - AGT 审计子目录 → `/tmp/.agt/audit`（rw，审计 JSONL 落此）。
/// 空 subdirs → 防御性报错（R6e 块B：executor ws 挂载非空保证——空声明是计划
/// 缺陷，计划审查应打回重规划；executor 不静默跳过、不静默回退挂全量）。
/// 首名重复声明 → 显式报错（A4 挂载锚：防双挂载同宿主目录 + 锚断言矛盾）。
pub fn generate_executor_compose(
    workspace_host_dir: &Path,
    image: &str,
    mounts: &ExecutorMounts,
) -> Result<String> {
    let volumes = alfred_mount_volumes(workspace_host_dir, mounts)?;
    let compose = ComposeFile {
        services: Services {
            default: Service {
                image: image.to_string(),
                command: "tail -f /dev/null".to_string(),
                init: true,
                network_mode: "none".to_string(),
                stop_grace_period: "1s".to_string(),
                volumes,
            },
        },
    };
    serde_yaml::to_string(&compose).context("serialize compose yaml")
}

fn alfred_mount_volumes(
    workspace_host_dir: &Path,
    mounts: &ExecutorMounts,
) -> Result<Vec<String>> {
    let abs = canonicalize_workspace(workspace_host_dir)?;
    let mut volumes: Vec<String> = Vec::new();
    if mounts.workspace_subdirs.is_empty() {
        // R6e 块B：executor 挂载非空保证——空声明是计划缺陷（计划审查应打回
        // 重规划），executor 侧防御性失败：不静默跳过挂载、不静默回退挂全量。
        bail!(
            "executor 沙箱 workspace_subdirs 为空：计划审查应拦截，executor 挂载不能为空（拒绝空声明，不挂全量）"
        );
    }
    // R6a + A4：逐项相对/越界 + 首名重复声明拒绝（列表级校验与 run.rs
    // `validate_executor_sandbox` 共用 `validate_workspace_subdirs`，单一真源）。
    validate_workspace_subdirs(&mounts.workspace_subdirs)?;
    for (i, sub) in mounts.workspace_subdirs.iter().enumerate() {
        let host = abs.join(sub);
        if !host.exists() {
            bail!(
                "workspace subdir '{}' does not exist under {} (declared in workspace_subdirs)",
                sub,
                abs.display()
            );
        }
        let target = if i == 0 {
            CONTAINER_WORKSPACE_DIR.to_string()
        } else {
            format!("{}/{}", CONTAINER_WORKSPACE_DIR, sub)
        };
        volumes.push(format!("{}:{}:rw", host.display(), target));
    }
    for vol in &mounts.ref_volumes {
        // 9/3 方案②：逐卷校验真源（mode ro + host 存在 + container 合法）与
        // validate_executor_sandbox 共用（单一真源）。
        validate_ref_volume(vol)?;
        let host_abs = Path::new(&vol.host_path)
            .canonicalize()
            .with_context(|| format!("canonicalize ref volume host {}", vol.host_path))?;
        volumes.push(format!("{}:{}:ro", host_abs.display(), vol.container_path));
    }
    if let Some(agt) = &mounts.agt_dir {
        let agt_abs = agt
            .canonicalize()
            .with_context(|| format!("canonicalize agt dir {}", agt.display()))?;
        // R6a：策略目录只读——agent 不可改策略文件。
        volumes.push(format!("{}:/tmp/.agt:ro", agt_abs.display()));
    }
    if let Some(audit) = &mounts.agt_audit_dir {
        let audit_abs = audit
            .canonicalize()
            .with_context(|| format!("canonicalize agt audit dir {}", audit.display()))?;
        // R6a：审计输出子目录 rw——agent 可写审计但不可改策略（拆开挂载）。
        volumes.push(format!("{}:/tmp/.agt/audit:rw", audit_abs.display()));
    }
    if let Some(sessions) = &mounts.sessions_dir {
        let sessions_abs = sessions
            .canonicalize()
            .with_context(|| format!("canonicalize sessions dir {}", sessions.display()))?;
        volumes.push(format!("{}:/tmp/.alfred-sessions:rw", sessions_abs.display()));
    }
    Ok(volumes)
}

/// canonicalize 工作区宿主目录；存在性/可访问性校验。
pub fn canonicalize_workspace(dir: &Path) -> Result<std::path::PathBuf> {
    if !dir.exists() {
        bail!(
            "workspace host dir does not exist: {} (must be created before compose)",
            dir.display()
        );
    }
    let abs = dir
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {}", dir.display()))?;
    // E1/E3 防呆：拒绝不在 HOME 下的 run 目录（colima 只共享 ~，挂不进去）
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    if !abs.starts_with(&home) {
        bail!(
            "workspace host dir {} is outside HOME ({}) — colima 只共享 ~，挂载会静默失败",
            abs.display(),
            home
        );
    }
    Ok(abs)
}

/// 任务环境 compose 生成（G1 native_inspect 真实环境，per-run）。
///
/// 输入 = 外层实验传入的原任务 compose（`--env-compose`，绝对路径）。
/// 输出在 `<exec>/executor.compose.yaml`，由 driver 的 Inspect `sample_init`
/// 起容器（compose up --wait + 健康检查——sidecar 不健康即失败，不静默
/// 放行）与 `sample_cleanup` 回收（生命周期不变，仍是 Inspect）。
///
/// 复用语义（不丢原任务环境事实）：
/// - `services.default`（agent 服务）：**镜像**换成执行镜像（`image` =
/// 原任务依赖镜像 + node/pi，Main 构建）；`volumes` 追加 alfred 挂载面
/// （workspace 子目录 rw + 只读参考卷 + AGT 策略/审计 + 原生 session）；
/// `stop_grace_period` 钉 1s（快速回收）；**其余字段逐字保留**——
/// `command`/`init`/`working_dir`/`mem_limit`/`extra_hosts`（原任务网络
/// 限制：参考域名钉 127.0.0.1）/`environment`（原任务 env 注入，如 DB 连接
/// 坐标）/`depends_on`（sidecar 健康门）/`x-local` 等。
/// - 其余服务（sidecar，如 mysql）：**逐字复制**（镜像 digest、healthcheck、
/// init SQL bind mount、`${SAMPLE_METADATA_*}` 引用原样——由 driver 的
/// sample_init 用 `--env-metadata` 的键值解析，与原任务装载同一链）。
/// - 网络：不注入 `network_mode: none`——原任务环境的网络形态（compose
/// 自建 bridge + extra_hosts 钉参考域名）原样保留（sidecar 服务名可解析）。
///
/// 防呆（fail-closed，不静默放行）：
/// - default 无 `image`（换不了执行镜像）/带 `build` 段（2026-09-25 Main
///   裁决：build+image 并存导致 tag 身份漂移）/带 `container_name`
///   （Inspect 拒绝）/无 `command`（容器必须常驻）→ 报错。`command` 支持
///   非空字符串或非空字符串列表；列表的空参数和 argv 边界原样保留。
/// - default 自带 `volumes`（与 alfred 挂载面冲突面未定义）→ 报错
///   （本仓任务 compose 的 default 一律无 volumes——workspace 由
///   Sample.files 注入；sidecar 的 volumes 不在此列，逐字保留）。
/// - default `depends_on` 引用的服务不存在，或 `condition: service_healthy`
///   引用的 sidecar 无 `healthcheck` → 报错（无健康门的 sidecar = 放行
///   条件缺失）。
pub fn generate_task_env_compose(
    orig_compose: &Path,
    workspace_host_dir: &Path,
    image: &str,
    mounts: &ExecutorMounts,
) -> Result<String> {
    fn skey(s: &str) -> serde_yaml::Value {
        serde_yaml::Value::String(s.to_string())
    }
    let orig_abs = orig_compose
        .canonicalize()
        .with_context(|| format!("canonicalize task env compose {}", orig_compose.display()))?;
    let text = std::fs::read_to_string(&orig_abs)
        .with_context(|| format!("read task env compose {}", orig_abs.display()))?;
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&text).context("parse task env compose yaml")?;
    let services = doc
        .get("services")
        .and_then(|v| v.as_mapping())
        .with_context(|| "task env compose: top-level 'services' mapping missing")?;
    let default = services
        .get(&skey("default"))
        .and_then(|v| v.as_mapping())
        .with_context(|| "task env compose: services.default mapping missing")?;

    // default 服务防呆（见函数级文档）。
    let dep_image = default
        .get(&skey("image"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .with_context(|| "task env compose: services.default.image missing（执行镜像需替换原任务依赖镜像）")?
        .to_string();
    if default.get(&skey("build")).is_some() {
        bail!("task env compose services.default 带 build 段（预构建镜像形态，build+image 并存会 tag 身份漂移）: {dep_image}");
    }
    if default.get(&skey("container_name")).is_some() {
        bail!("task env compose services.default 带 container_name（Inspect 拒绝：多 epoch 容器名冲突）");
    }
    // Compose `command` 的合法形态 = 非空字符串 或 非空字符串序列（argv 形
    // 态；元素逐字保留、不做 shell join，字符串形态也保持原语义不被拆分）。
    // 缺失/空串/纯空白/空序列/全空串元素/非字符串元素一律按缺 command 拒
    // 绝（fail-closed 防呆语义不变：执行容器必须常驻）。
    let command_present = match default.get(&skey("command")) {
        Some(serde_yaml::Value::String(s)) => !s.trim().is_empty(),
        Some(serde_yaml::Value::Sequence(seq)) => !seq.is_empty()
            && seq.iter().all(|item| item.as_str().is_some())
            && seq
                .iter()
                .any(|item| item.as_str().is_some_and(|s| !s.trim().is_empty())),
        _ => false,
    };
    if !command_present {
        bail!("task env compose services.default 缺 command（执行容器必须常驻，如 tail -f /dev/null；合法形态=非空字符串或非空字符串序列）");
    }
    if default.get(&skey("volumes")).is_some() {
        bail!(
            "task env compose services.default 自带 volumes（与 alfred 挂载面冲突面未定义，拒绝合并；本仓任务 workspace 由 Sample.files 注入，default 不应有 volumes）"
        );
    }

    // sidecar 健康门：default depends_on 引用的服务必须存在；
    // condition: service_healthy 的 sidecar 必须定义 healthcheck（无健康
    // 门 = 放行条件缺失，显式拒绝——docker compose 也会失败，这里给早错）。
    if let Some(depends_on) = default.get(&skey("depends_on")) {
        let entries: Vec<(String, Option<serde_yaml::Value>)> = match depends_on {
            serde_yaml::Value::Sequence(seq) => seq
                .iter()
                .filter_map(|v| v.as_str().map(|s| (s.to_string(), None)))
                .collect(),
            serde_yaml::Value::Mapping(map) => map
                .iter()
                .filter_map(|(k, v)| {
                    k.as_str().map(|s| (s.to_string(), Some(v.clone())))
                })
                .collect(),
            _ => bail!("task env compose services.default.depends_on 形态不支持（序列或映射）"),
        };
        for (svc, cond) in entries {
            let sidecar = services
                .get(&skey(&svc))
                .with_context(|| format!("task env compose services.default.depends_on 引用不存在的服务 '{svc}'"))?;
            let healthy = matches!(&cond, Some(serde_yaml::Value::Mapping(m))
                if m.get(&skey("condition")).and_then(|v| v.as_str()) == Some("service_healthy"));
            if healthy {
                let has_healthcheck = sidecar
                    .as_mapping()
                    .map(|m| m.contains_key(&skey("healthcheck")))
                    .unwrap_or(false);
                if !has_healthcheck {
                    bail!(
                        "task env compose sidecar '{svc}' 被 depends_on(service_healthy) 引用但无 healthcheck（无健康门的 sidecar = 放行条件缺失）"
                    );
                }
            }
        }
    }

    // alfred 挂载面（与内置 compose 共用单一真源 alfred_mount_volumes）。
    let volumes = alfred_mount_volumes(workspace_host_dir, mounts)?;

    // default 服务：镜像换执行镜像 + volumes 注入 + stop_grace_period 钉 1s，
    // 其余字段（command/init/working_dir/mem_limit/extra_hosts/environment/
    // depends_on/x-local…）逐字保留。
    let mut out_default = default.clone();
    out_default.insert(skey("image"), serde_yaml::Value::String(image.to_string()));
    out_default.insert(skey("stop_grace_period"), skey("1s"));
    let volume_values: Vec<serde_yaml::Value> =
        volumes.into_iter().map(serde_yaml::Value::String).collect();
    out_default.insert(
        skey("volumes"),
        serde_yaml::Value::Sequence(volume_values),
    );

    let mut out_services = serde_yaml::Mapping::new();
    for (name, svc) in services {
        if name.as_str() == Some("default") {
            out_services.insert(
                name.clone(),
                serde_yaml::Value::Mapping(std::mem::take(&mut out_default)),
            );
        } else {
            out_services.insert(name.clone(), svc.clone());
        }
    }
    // Preserve named networks/volumes and other top-level task constraints.
    let mut out = doc.as_mapping().context("task env compose must be a mapping")?.clone();
    out.insert(skey("services"), serde_yaml::Value::Mapping(out_services));
    let rendered = serde_yaml::to_string(&serde_yaml::Value::Mapping(out))
        .context("serialize task env compose yaml")?;
    Ok(rendered
        + &format!(
            "# task environment compose from {}\n# executor image: {image} (dependency: {dep_image})\n",
            orig_abs.display()
        ))
}
