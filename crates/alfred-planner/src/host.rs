//! planner 宿主 pi 驱动（宿主 pi 化：converse 去容器）。
//!
//! 架构演进（属主拍板，`.plans/架构演进-planner-reviewer宿主pi.md`）：planner 不再
//! 是容器 pi（run ws 空基线 + 挂载面），而是**宿主 pi agent**——cwd=治理对象项目根，
//! 每轮 `-p` 单次短会话（无状态），按 §2.4 两分支产出到 `<run>/planner/outputs/`
//! （`instructions.json` | `reply.txt` 二选一），宿主 settle 后收割。
//!
//! spawn 形态（PiHostFormCheck 实测，pi 0.80.10 宿主）：
//! ```text
//! pi -p --no-session -nc \
//!   --system-prompt "<CONVERSE_SYSTEM_PROMPT + 产出路径规则>" \
//!   -e <run>/planner/agt/agt-policy.ts \
//!   --provider <roles.planner 解析> --model <同> \
//!   # prompt 经 stdin（会话文档投影 + owner 消息 + 产出路径规则；JSON 安全，不经 argv）
//! ```
//! - **模型单一真源**：config.yml roles.planner 经 `load_planner_model()` 解析成
//!   [`ExecutorModel`]（编排器持有）；宿主 pi 的 provider/model 经
//!   `PI_CODING_AGENT_DIR=<run>/planner/pi-config` 指向编排器生成的 run 级
//!   `models.json`（providers.baseUrl/apiKey + models[].id）——config.yml 仍是
//!   唯一真源，pi "只认 models.json" 的特性被编排器收编。
//! - **不可知隔离**（三道防线之文件面/工具面）：AGT 扩展经 env
//!   `AGT_POLICY_PATH/AGT_AUDIT_PATH/AGT_WORKSPACE_DIR` 注入 planner 不可知策略
//!   （拦写项目根 + 拦读 run 治理产物 + 中性拒绝反馈，见
//!   `docker/agt/planner/policy.json`）；`-nc` 防 pi 加载宿主 context files。
//! - **llm-calls 审计**（P9 证据链延续）：request=prompt 全文、response=产出
//!   文件内容、model=inspect_model_id，落盘由调用方（converse.rs）统一做——
//!   与离线/容器时代同构。
//!
//! 产出收割：两分支候选文件恰好一个非空（多/零显式报错，无静默出口）；跨轮
//! 残留在 spawn 前清空（容器时代 run_planner_container 的确定性修复语义平移）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use alfred_core::request::OwnerRequest;
use alfred_core::session::SessionDoc;
use alfred_executor::agt::{resolve_agt_source, AgtSource};
use alfred_executor::config::ExecutorModel;
use anyhow::{bail, Context, Result};
use serde::Serialize;

/// planner 宿主驱动工作目录名（`<run_dir>/planner/`；沿用容器时代布局）。
pub const PLANNER_WORK_DIR: &str = "planner";
/// 产出目录（`<run_dir>/planner/outputs/`——planner 唯一可写白名单）。
pub const OUTPUTS_DIR: &str = "outputs";
/// 建图指令产出文件名（§2.4 指令分支）。
pub const CONVERSE_OUTPUT_FILE: &str = "instructions.json";
/// 属主答复产出文件名（§2.4 答复分支）。
pub const CONVERSE_REPLY_FILE: &str = "reply.txt";
/// run 级 pi 配置目录名（`<run_dir>/planner/pi-config`，内含 `agent/models.json`）。
const PI_CONFIG_DIR: &str = "pi-config";

/// planner 宿主驱动选项（编排器从 `GovernanceOptions` + config 解析面派生）。
#[derive(Debug, Clone)]
pub struct PlannerHostOptions {
    /// 治理 run 目录（`<run>/planner/` 是本驱动工作区；llm-calls/ 由调用方落）。
    pub run_dir: PathBuf,
    /// 治理对象项目根（planner pi 的 cwd=工作区；AGT_WORKSPACE_DIR 注入值）。
    pub project_root: PathBuf,
    /// planner 单轮时间上限（秒）。
    pub time_limit_secs: u32,
    /// AGT 源（默认内置策略；`ALFRED_AGT_DIR` 覆盖；`ALFRED_AGT_DISABLE=1` 关）。
    pub agt: AgtSource,
}

impl PlannerHostOptions {
    /// 从治理环运行选项派生宿主驱动选项。
    ///
    /// `project_root` = 当前进程 cwd（治理对象项目根：alfred run 由项目根发起，
    /// 容器时代 ws 基线同样取发起侧语义）。
    pub fn from_governance(
        run_dir: PathBuf,
        opts: &alfred_core::governance::GovernanceOptions,
    ) -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            run_dir,
            project_root,
            time_limit_secs: opts.planner_time_limit_secs,
            agt: resolve_agt_source(),
        }
    }
}

/// 宿主 converse 单轮结果（分支由 `produced_file` 区分，语义同容器时代
/// `ContainerRunOutput`；字段收窄到宿主路径）。
#[derive(Debug, Clone)]
pub struct HostRunOutput {
    /// 产出原始文本（建图指令 JSON 数组 或 属主答复）。
    pub output_text: String,
    /// 实际产出的产出文件名（`instructions.json` | `reply.txt`）。
    pub produced_file: String,
}

/// pi models.json 的 provider 段（run 级生成，模型单一真源的编排器投影）。
#[derive(Serialize)]
struct PiProvider<'a> {
    #[serde(rename = "baseUrl")]
    base_url: &'a str,
    api: &'static str,
    #[serde(rename = "apiKey")]
    api_key: &'a str,
    models: Vec<PiModel<'a>>,
}

/// pi models.json 的 model 段。
#[derive(Serialize)]
struct PiModel<'a> {
    id: &'a str,
    #[serde(rename = "maxTokens")]
    max_tokens: u32,
}

/// 生成 run 级 pi 配置（`<run>/planner/pi-config/agent/models.json` + `auth.json`）。
///
/// 模型单一真源：provider/model 从 config.yml 解析面（[`ExecutorModel`]）投影——
/// pi 只认 models.json，编排器按 config 生成即收编。`auth.json` 空对象占位
/// （PiHostFormCheck：无此文件偶发 "No API key found"）。
fn write_pi_config(pi_config_dir: &Path, model: &ExecutorModel) -> Result<()> {
    let agent_dir = pi_config_dir.join("agent");
    std::fs::create_dir_all(&agent_dir)
        .with_context(|| format!("create pi config dir {}", agent_dir.display()))?;
    if !pi_config_dir.join("auth.json").exists() {
        std::fs::write(pi_config_dir.join("auth.json"), "{}")
            .with_context(|| format!("write pi auth {}", pi_config_dir.join("auth.json").display()))?;
    }
    let models_json = serde_json::to_string_pretty(&serde_json::json!({
        "providers": {
            model.provider.clone(): PiProvider {
                base_url: &model.base_url,
                api: "openai-completions",
                api_key: &model.api_key,
                models: vec![PiModel {
                    id: &model.model,
                    max_tokens: model.max_tokens,
                }],
            }
        }
    }))
    .context("serialize pi models.json")?;
    let path = agent_dir.join("models.json");
    std::fs::write(&path, models_json)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// converse 宿主驱动的产出路径规则（追加到 system prompt；容器时代
/// CONVERSE_DRIVER_PROMPT 的宿主形态：产出写文件而非聊天回复）。
fn output_paths_section(outputs_dir: &Path) -> String {
    let out = outputs_dir.display();
    format!(
        r#"

产出方式（强制）：
- 本轮产出必须是文件，不是聊天回复。产出目录（唯一可写位置）：{out}
- 若产出建图指令序列：把 JSON 数组用 write 工具写入 {out}/instructions.json。
- 若产出给属主的答复：把答复文本用 write 工具写入 {out}/reply.txt。
- 只能写其中一个文件；不要写产出目录之外的任何文件（会被拒绝）。
- 产出只用 write 工具；禁止用 bash 重定向（> >> tee 等）写任何文件。
写完即结束。"#
    )
}

/// converse 宿主驱动：会话文档投影 + 属主消息 + request → §2.4 两分支产出。
///
/// prompt（stdin）= 会话文档投影（净化，`project_session_doc`）+ 属主本轮消息 +
/// 产出路径规则；system prompt = 建图 schema（+ codux append 注入）+ 产出规则。
///
/// spawn `pi -p` 单次，settle 后收割恰好一个产出文件。
pub fn run_converse_on_host(
    opts: &PlannerHostOptions,
    model: &ExecutorModel,
    request: &OwnerRequest,
    doc: &SessionDoc,
    owner_message: &str,
    append_system_prompt: &str,
) -> Result<HostRunOutput> {
    if model.raw_id {
        bail!(
            "planner host pi requires an openai-compatible provider model, got raw builtin model '{}'",
            model.model
        );
    }

    let work = opts.run_dir.join(PLANNER_WORK_DIR);
    let outputs_dir = work.join(OUTPUTS_DIR);
    std::fs::create_dir_all(&outputs_dir)
        .with_context(|| format!("create planner outputs dir {}", outputs_dir.display()))?;

    // 跨轮残留清理（容器时代确定性修复语义平移）：两分支候选恰好一个——上一轮
    // reply.txt 残留 + 本轮 instructions.json 并存会被收割判"both"→ 显式报错。
    for f in [CONVERSE_OUTPUT_FILE, CONVERSE_REPLY_FILE] {
        match std::fs::remove_file(outputs_dir.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("remove stale planner output {}", f))
            }
        }
    }

    // AGT 拦截层（默认启用）：策略 + 扩展落 `<work>/agt/`（宿主形态无挂载，
    // 直接文件路径经 -e / AGT_POLICY_PATH 注入）。
    let agt_ext = prepare_host_agt(&work, &opts.agt, &outputs_dir, &opts.project_root)?;

    // run 级 pi 配置（模型单一真源投影）。
    let pi_config_dir = work.join(PI_CONFIG_DIR);
    write_pi_config(&pi_config_dir, model)?;

    // system prompt = 建图 schema（+ codux append）+ 产出路径规则。
    let system_prompt = format!(
        "{}{}",
        crate::converse::converse_system_prompt(append_system_prompt),
        output_paths_section(&outputs_dir)
    );

    // prompt（stdin）= 会话文档投影 + 属主本轮消息 + request id。
    let projection = crate::converse::project_session_doc(doc);
    let session = serde_json::to_string_pretty(&projection)
        .context("serialize projected SessionDoc")?;
    let prompt = format!(
        "需求 id：{}\n\n会话文档（记忆）：\n{session}\n\n属主本轮消息：\n{owner_message}",
        request.id
    );

    // spawn：pi -p --no-session -nc --system-prompt <sys> -e <agt> --provider --model
    let mut cmd = Command::new("pi");
    cmd.arg("-p")
        .arg("--no-session")
        .arg("-nc")
        .arg("--system-prompt")
        .arg(&system_prompt)
        .arg("--provider")
        .arg(&model.provider)
        .arg("--model")
        .arg(&model.model)
        .current_dir(&opts.project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(ext) = &agt_ext {
        cmd.arg("-e").arg(ext);
        cmd.env("AGT_POLICY_PATH", work.join("agt/policy.json"));
        cmd.env("AGT_AUDIT_PATH", work.join("agt/audit/audit.jsonl"));
    }
    // PI_CODING_AGENT_DIR = 配置目录本身（models.json 直接在其下；pi 默认 ~/.pi/agent）。
    cmd.env("PI_CODING_AGENT_DIR", pi_config_dir.join("agent"));
    cmd.env("AGT_WORKSPACE_DIR", &opts.project_root);

    let mut child = cmd
        .spawn()
        .with_context(|| "spawn host pi for planner converse (is `pi` on PATH?)")?;
    // prompt 经 stdin（JSON 会话文档不经 argv——ps 不可见且免引号转义）。
    use std::io::Write;
    child
        .stdin
        .take()
        .context("pi stdin")?
        .write_all(prompt.as_bytes())
        .context("write planner converse prompt to pi stdin")?;
    // stdin 落 drop 即 EOF → pi 处理完 prompt 退出。

    // 自限时：time_limit_secs 到点 kill（容器时代 driver anyio.fail_after 的宿主
    // 形态）。轮询收割 child，避免僵尸；超时先 terminate 再等待回收。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(opts.time_limit_secs as u64);
    let output = loop {
        match child.try_wait()? {
            Some(status) => {
                break std::process::Output {
                    status,
                    stdout: child.stdout.take().map(|mut s| { let mut b = Vec::new(); use std::io::Read; let _ = s.read_to_end(&mut b); b }).unwrap_or_default(),
                    stderr: child.stderr.take().map(|mut s| { let mut b = Vec::new(); use std::io::Read; let _ = s.read_to_end(&mut b); b }).unwrap_or_default(),
                };
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "planner host pi timed out after {}s",
                        opts.time_limit_secs
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    };
    if !output.status.success() {
        bail!(
            "planner host pi exited with {} (stderr: {})",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim().chars().take(2000).collect::<String>()
        );
    }

    // 收割：两分支候选恰好一个非空（多/零显式报错，无静默出口）。
    let mut produced: Vec<(&str, String)> = Vec::new();
    for f in [CONVERSE_OUTPUT_FILE, CONVERSE_REPLY_FILE] {
        if let Ok(text) = std::fs::read_to_string(outputs_dir.join(f)) {
            if !text.trim().is_empty() {
                produced.push((f, text));
            }
        }
    }
    let (produced_file, output_text) = match produced.as_slice() {
        [(f, text)] => ((*f).to_string(), text.clone()),
        [] => bail!(
            "planner host pi produced none of {CONVERSE_OUTPUT_FILE}, {CONVERSE_REPLY_FILE} \
             (pi stdout: {})",
            String::from_utf8_lossy(&output.stdout).trim().chars().take(2000).collect::<String>()
        ),
        _ => bail!(
            "planner host pi produced multiple outputs: 两分支只能二选一"
        ),
    };
    if output_text.trim().is_empty() {
        bail!("planner host pi produced empty output in {produced_file}");
    }

    Ok(HostRunOutput {
        output_text,
        produced_file,
    })
}

/// 宿主侧 AGT 层准备：策略 + 扩展落 `<work>/agt/`，并把 planner 不可知策略的
/// 占位符按本 run 渲染（`__WORKSPACE_DIR__`/`__OUTPUTS_DIR__`/… → 绝对/相对路径）。
///
/// 容器时代 `prepare_agt_work` 只搬文件（占位符由挂载语义隐含）；宿主形态 cwd=
/// 项目根，策略需要 run 产物的绝对前缀 + 相对前缀双形态（模型可能给相对路径）。
/// 返回扩展文件路径（`AgtSource::Off` → None，不加载扩展）。
fn prepare_host_agt(
    work: &Path,
    source: &AgtSource,
    outputs_dir: &Path,
    project_root: &Path,
) -> Result<Option<PathBuf>> {
    if let AgtSource::Off = source {
        return Ok(None);
    }
    let dest = stage_agt_dir(work)?;
    let run_dir = work
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // 策略文法：Builtin = 内嵌不可知策略（占位符按 run 渲染）；Dir = 显式目录
    // 策略原样沿用（e2e 显式目录自带占位符或具体路径，不做二次渲染）。
    let policy_text = match source {
        AgtSource::Builtin => render_planner_policy(
            alfred_executor::agt::assets::PLANNER_POLICY,
            &run_dir,
            outputs_dir,
            project_root,
        )?,
        AgtSource::Dir(dir) => std::fs::read_to_string(dir.join("policy.json"))
            .with_context(|| format!("read agt policy {}", dir.join("policy.json").display()))?,
        AgtSource::Off => unreachable!("handled above"),
    };
    std::fs::write(dest.join("policy.json"), policy_text)
        .with_context(|| format!("write planner agt policy {}", dest.join("policy.json").display()))?;
    let ext_src = match source {
        AgtSource::Dir(dir) => dir.join("agt-policy.ts"),
        _ => return Ok(Some(install_builtin_ext(&dest))),
    };
    std::fs::copy(&ext_src, dest.join("agt-policy.ts")).with_context(|| {
        format!(
            "copy agt extension {} -> {}",
            ext_src.display(),
            dest.join("agt-policy.ts").display()
        )
    })?;
    Ok(Some(dest.join("agt-policy.ts")))
}

/// 内置扩展落盘（Builtin 分支；编译期内嵌，单一真源 `docker/agt/agt-policy.ts`）。
fn install_builtin_ext(dest: &Path) -> PathBuf {
    std::fs::write(dest.join("agt-policy.ts"), alfred_executor::agt::assets::EXTENSION_TS)
        .expect("write builtin agt extension");
    dest.join("agt-policy.ts")
}

/// 非 Off 分支公共落盘：建 `<work>/agt[/audit]` 目录。
fn stage_agt_dir(work: &Path) -> Result<PathBuf> {
    let dest = work.join("agt");
    std::fs::create_dir_all(&dest).with_context(|| format!("create agt dir {}", dest.display()))?;
    std::fs::create_dir_all(dest.join("audit"))
        .with_context(|| format!("create agt audit dir {}", dest.join("audit").display()))?;
    Ok(dest)
}

/// 把 planner 不可知策略占位符按本 run 渲染。
///
/// 双形态前缀：绝对（pi 工具参数常见绝对路径）+ 相对（cwd=项目根时模型可能给
/// `.alfred/runs/...` 形态）。相对形态 = 绝对路径剥项目根前缀（无前导 ./，与
/// 策略 startswith 条件一致）。
fn render_planner_policy(
    policy: &str,
    run_dir: &Path,
    outputs_dir: &Path,
    project_root: &Path,
) -> Result<String> {
    let ws = absolut(project_root)?;
    let runs_abs = absolut(&project_root.join(".alfred/runs"))?;
    let out_abs = absolut(outputs_dir)?;
    // 相对形态 = 绝对剥 `<项目根>/`（产出目录恒在项目根之下：run 目录由编排器
    // 建在治理对象项目根内）。剥不动（run 目录在项目根外）→ 相对条件退化为
    // 绝对串（恒不命中，安全侧：拦写仍由绝对前缀规则覆盖）。
    let ws_prefix = format!("{ws}/");
    let out_rel = out_abs.strip_prefix(&ws_prefix).unwrap_or(&out_abs).to_string();
    let agt_work = absolut(&run_dir.join(PLANNER_WORK_DIR).join("agt"))?;
    Ok(policy
        .replace("__WORKSPACE_DIR__", &ws)
        .replace("__OUTPUTS_DIR__", &out_abs)
        .replace("__OUTPUTS_DIR_REL__", &out_rel)
        .replace("__RUNS_DIR__", &runs_abs)
        .replace("__RUNS_DIR_REL__", ".alfred/runs")
        .replace("__AGT_WORK__", &agt_work))
}

/// 路径绝对化（不要求存在；不解析 symlink，与 pi 工具参数字面语义一致）。
fn absolut(p: &Path) -> Result<String> {
    Ok(alfred_executor::driver::absolutize_cwd(p)
        .display()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths_section_mentions_both_candidates() {
        let s = output_paths_section(Path::new("/run/planner/outputs"));
        assert!(s.contains("/run/planner/outputs/instructions.json"));
        assert!(s.contains("/run/planner/outputs/reply.txt"));
    }

    #[test]
    fn pi_config_shape_roundtrip() {
        let dir = std::env::temp_dir().join(format!("alfred-pi-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let model = ExecutorModel {
            provider: "zhipucoding".into(),
            model: "glm-5.2".into(),
            base_url: "https://example.invalid/v4".into(),
            api_key: "sk-test".into(),
            max_tokens: 8192,
            raw_id: false,
        };
        write_pi_config(&dir, &model).unwrap();
        let models: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("agent/models.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(models["providers"]["zhipucoding"]["baseUrl"], "https://example.invalid/v4");
        assert_eq!(models["providers"]["zhipucoding"]["models"][0]["id"], "glm-5.2");
        assert_eq!(models["providers"]["zhipucoding"]["models"][0]["maxTokens"], 8192);
        assert_eq!(std::fs::read_to_string(dir.join("auth.json")).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
