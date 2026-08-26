//! `alfred panel` 子命令（R4/P4：决策面板 RPC）。
//!
//! 属主会话的 pi 是**宿主直连**进程（与容器执行者 pi 互不相干）。升级
//! （PlanRejected / Escalated 挂起态）时起动本命令：
//!
//! 1. 读 `state.json`（须处于挂起态，否则拒绝）；
//! 2. 生成 pi 面板扩展 `run_dir/panel-extension.ts`（provider 凭证/模型从
//!    `~/.config/alfred/config.yml` 唯一真源嵌入）；
//! 3. spawn `pi --mode rpc --no-session -e <ext>`（隔离：`--no-extensions
//!    --no-skills --no-context-files --no-prompt-templates`）；
//! 4. 注入升级上下文 prompt → 驱动 pi 调 `alfred_panel_decision` 工具 →
//!    `ctx.ui.select` 发三选项决策卡（RPC 侧 = `extension_ui_request`
//!    `{method:"select", id, title, options[]}`）；
//! 5. 终端渲染三选项（数字选择，codux 接管前的最小形态）→ 回
//!    `extension_ui_response {id, value}`（id 关联、value 精确匹配选项）；
//! 6. 从 `tool_execution_end` 取工具结果（`OWNER_DECISION:<value>`，结构化）
//!    → 映射 OwnerDecision → 调 `alfred decide` 续跑。
//!
//! 全部收/发消息落 `run_dir/panel-session.jsonl`（`{dir:"recv"|"send",
//! msg:{...}}`），e2e 从结构化消息流断言（非 stdout 文本猜测）。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use alfred_core::governance::{GovernanceRun, GovernanceState};
use alfred_core::util::now_rfc3339;
use alfred_executor::config::load_executor_model;
use clap::Args;
use serde_json::{json, Value};

use super::decide::{choice_label, DecideChoice};
use super::governance::{load_governance_run, state_label};

/// pi 面板扩展（TypeScript 资产，生成时替换占位符）。
const PANEL_EXTENSION_TS: &str = include_str!("panel_extension.ts");

/// 决策工具名（pi 扩展注册；`tool_execution_end` 按此关联）。
const PANEL_TOOL: &str = "alfred_panel_decision";
/// 工具结果前缀（`OWNER_DECISION:<value>`，value 精确匹配选项）。
const OWNER_DECISION_PREFIX: &str = "OWNER_DECISION:";

/// 三选项决策卡（P4/P5：重跑 / 改契约 / 放弃，不可省略）。
const OPTION_RETRY: &str = "重跑";
const OPTION_REVISE: &str = "改契约";
const OPTION_ABANDON: &str = "放弃";

/// pi 子进程就绪后的 prompt 命令 id。
const PROMPT_REQ_ID: &str = "req-1";

#[derive(Args, Debug)]
pub struct PanelArgs {
    /// 治理环运行目录（含 state.json，须处于挂起态）。
    #[arg(long)]
    pub run_dir: PathBuf,

    /// pi 会话模型 id（默认 `ALFRED_PANEL_MODEL` env，否则复用 executor 模型）。
    #[arg(long)]
    pub panel_model: Option<String>,

    /// 等待属主拍板超时（秒）。
    #[arg(long, default_value_t = 300)]
    pub timeout: u32,

    /// 结构化会话日志路径（默认 run_dir/panel-session.jsonl）。
    #[arg(long)]
    pub session_log: Option<PathBuf>,

    /// 只渲染决策卡并输出选择，不调用 `alfred decide` 续跑。
    #[arg(long)]
    pub no_decide: bool,

    /// revise（改契约）时预填的属主消息文件；缺省则交互式从 stdin 读取。
    #[arg(long)]
    pub message: Option<PathBuf>,
}

/// 读线程派发的事件。
enum PanelEvent {
    /// 决策卡请求（extension_ui_request method=select）。
    UiSelect {
        id: String,
        title: String,
        options: Vec<String>,
    },
    /// 决策工具结果（tool_execution_end → OWNER_DECISION:<value>）。
    Decision(String),
    /// pi 会话完全落定。
    Settled,
    /// pi 错误（进程/读流）。
    PiError(String),
}

pub fn panel(args: PanelArgs) -> Result<()> {
    let run_dir = &args.run_dir;
    let run = load_governance_run(run_dir)?;
    let state = run.state();
    if !state.is_suspended() {
        bail!(
            "panel 不适用于当前状态 {:?}（仅挂起态 PlanRejected/Escalated 可拍板）",
            state
        );
    }

    // 模型：`--panel-model` > `ALFRED_PANEL_MODEL` env > executor 模型。
    // provider/base_url/api_key 沿用 executor 的 provider（config 唯一真源）。
    let base = load_executor_model()?;
    let model_id = args
        .panel_model
        .clone()
        .or_else(|| std::env::var("ALFRED_PANEL_MODEL").ok())
        .unwrap_or_else(|| base.model.clone());
    if base.raw_id {
        bail!("executor 模型 {:?} 是 inspect 内建（raw_id），不能用于 pi 属主会话", base.model);
    }

    // 生成 pi 扩展（嵌入凭证/模型）。
    let ext_path = run_dir.join("panel-extension.ts");
    let ext = render_extension(&base.provider, &base.base_url, &base.api_key, &model_id, base.max_tokens);
    std::fs::write(&ext_path, ext).context("write run_dir/panel-extension.ts")?;

    // 会话日志（结构化消息流）。
    let session_log = args
        .session_log
        .clone()
        .unwrap_or_else(|| run_dir.join("panel-session.jsonl"));
    log_meta(&session_log, "panel_started", &json!({
        "run_dir": run_dir.display().to_string(),
        "state": state_label(state),
        "model": model_id,
        "provider": base.provider,
        "pid": std::process::id(),
    }))?;

    // 升级上下文（注入 pi 属主会话）。
    let context = build_context(&run, state);

    // spawn pi --mode rpc（属主会话，宿主直连）。
    let mut child = Command::new("pi")
        .arg("--mode")
        .arg("rpc")
        .arg("--no-session")
        .arg("-e")
        .arg(&ext_path)
        .arg("--no-extensions")
        .arg("--no-skills")
        .arg("--no-context-files")
        .arg("--no-prompt-templates")
        .arg("--model")
        .arg(&model_id)
        .arg("--provider")
        .arg(&base.provider)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn `pi --mode rpc`（确保 pi 已安装）"))?;

    let stdin = child.stdin.take().context("pi stdin")?;
    let stdout = child.stdout.take().context("pi stdout")?;
    let mut stderr = child.stderr.take().context("pi stderr")?;

    // stderr → 会话日志（非致命；pi 错误信息调试用）。
    let err_log = session_log.clone();
    thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        if let Ok(_) = stderr.read_to_string(&mut buf) {
            if !buf.trim().is_empty() {
                let _ = log_json(&err_log, "stderr", &buf);
            }
        }
    });

    // 通道 + stdin 写锁（只有主线程写，读线程只派发）。
    let (tx, rx): (Sender<PanelEvent>, Receiver<PanelEvent>) = mpsc::channel();
    let stdin_writer: Arc<Mutex<ChildStdin>> = Arc::new(Mutex::new(stdin));
    // 读线程：stdout JSONL（LF 唯一切分，Node readline 不合规）→ 日志 + 派发。
    let reader_log = session_log.clone();
    let reader_tx = tx.clone();
    thread::spawn(move || {
        let stdout = stdout;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF（pi 退出）
                Ok(_) => {}
                Err(e) => {
                    let _ = reader_tx.send(PanelEvent::PiError(format!("read pi stdout: {e}")));
                    break;
                }
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let _ = log_json(&reader_log, "recv", trimmed);
            let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                continue; // 非 JSON 行（pi banner）
            };
            let kind = msg.get("type").and_then(|t| t.as_str());
            match kind {
                Some("extension_ui_request") => {
                    if msg.get("method").and_then(|m| m.as_str()) == Some("select") {
                        let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                        let title = msg
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let options = msg
                            .get("options")
                            .and_then(|o| o.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let _ = reader_tx.send(PanelEvent::UiSelect { id, title, options });
                    }
                }
                Some("tool_execution_end") => {
                    if msg.get("toolName").and_then(|t| t.as_str()) == Some(PANEL_TOOL) {
                        if let Some(text) = msg
                            .pointer("/result/content/0/text")
                            .and_then(|v| v.as_str())
                        {
                            if let Some(rest) = text.strip_prefix(OWNER_DECISION_PREFIX) {
                                let _ = reader_tx.send(PanelEvent::Decision(rest.to_string()));
                            }
                        }
                    }
                }
                Some("agent_settled") => {
                    let _ = reader_tx.send(PanelEvent::Settled);
                }
                _ => {}
            }
        }
        // 读线程结束（EOF）——若主线程还在等事件，通知它断开。
        let _ = reader_tx.send(PanelEvent::PiError("pi 进程已退出（stdout EOF）".into()));
    });

    // 注入升级上下文 prompt。
    let prompt = json!({
        "id": PROMPT_REQ_ID,
        "type": "prompt",
        "message": context,
    });
    write_pi(&stdin_writer, &session_log, &prompt)?;

    // 主循环：处理决策卡 → 读属主选择 → 回 response；直到决策/落定/超时。
    let deadline = Instant::now() + Duration::from_secs(args.timeout as u64);
    let decision = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "panel 超时：{}s 内未收到属主选择（pi 会话仍存活）",
                args.timeout
            );
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(PanelEvent::UiSelect { id, title, options }) => {
                let value = render_and_pick(&title, &options)?;
                let resp = json!({
                    "type": "extension_ui_response",
                    "id": id,
                    "value": value,
                });
                write_pi(&stdin_writer, &session_log, &resp)?;
            }
            Ok(PanelEvent::Decision(v)) => break v,
            Ok(PanelEvent::Settled) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("pi 会话提前落定（agent_settled）但未收到决策工具结果");
            }
            Ok(PanelEvent::PiError(e)) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("panel 失败：{e}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("pi 会话结束（事件通道断开）");
            }
        }
    };

    // 收尾：关闭 pi。
    drop(stdin_writer);
    let _ = child.kill();
    let _ = child.wait();

    // 决策 → OwnerDecision。
    let choice = choice_from_option(&decision)?;
    println!("[alfred panel] 属主选择：{decision}（{}）", choice_label(choice));
    log_meta(&session_log, "owner_decision", &json!({
        "option": decision,
        "choice": choice_label(choice),
    }))?;
    // 落盘决策（traceability；e2e 可读）。
    std::fs::write(
        run_dir.join("panel-decision.json"),
        serde_json::to_string_pretty(&json!({
            "option": decision,
            "choice": choice_label(choice),
            "ts": now_rfc3339(),
        }))
        .context("serialize panel-decision.json")?,
    )
    .context("write panel-decision.json")?;

    if args.no_decide {
        println!(
            "[alfred panel] --no-decide：不调用 decide。请运行：\n  alfred decide --run-dir {} --decision {}",
            run_dir.display(),
            choice_label(choice)
        );
        return Ok(());
    }

    // 调 decide 续跑（继承 env：ALFRED_OFFLINE 等）。
    invoke_decide(&args, choice)?;
    Ok(())
}

/// 生成 pi 扩展内容（占位符替换；字符串字段用 JSON 字面量，数字字段裸值）。
fn render_extension(
    provider: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    max_tokens: u32,
) -> String {
    PANEL_EXTENSION_TS
        .replace("__PROVIDER_JSON__", &serde_json::to_string(provider).unwrap_or_default())
        .replace("__BASE_URL_JSON__", &serde_json::to_string(base_url).unwrap_or_default())
        .replace("__API_KEY_JSON__", &serde_json::to_string(api_key).unwrap_or_default())
        .replace("__MODEL_JSON__", &serde_json::to_string(model).unwrap_or_default())
        .replace("__MAX_TOKENS__", &max_tokens.to_string())
}

/// 升级上下文（注入 pi 属主会话的 prompt）。
fn build_context(run: &GovernanceRun, state: GovernanceState) -> String {
    let mut lines = vec![
        format!(
            "你是 alfred 治理属主会话。当前治理环进入挂起态（{}），需要属主拍板。",
            state_label(state)
        ),
        format!("请求：{}", run.request.title),
        format!(
            "请调用 alfred_panel_decision 工具向属主展示决策卡。title：'治理升级决策'，options：['{}','{}','{}']。",
            OPTION_RETRY, OPTION_REVISE, OPTION_ABANDON
        ),
        "调用工具后，把属主选择的选项原样作为你的回复返回。".to_string(),
    ];
    match state {
        GovernanceState::PlanRejected => {
            if let Some(v) = run.plan_verdicts.last() {
                lines.push(format!("背景：计划审查打回，理由：{}", truncate(&v.reason, 240)));
            }
        }
        GovernanceState::Escalated => {
            if let Some(v) = run.exec_verdicts.last() {
                lines.push(format!(
                    "背景：执行审查升级（{:?}，failure_class={:?}），理由：{}",
                    v.value,
                    v.failure_class,
                    truncate(&v.explanation, 240)
                ));
            }
        }
        _ => {}
    }
    lines.join("\n")
}

/// 终端渲染三选项（数字选择）→ 精确返回所选选项串。
fn render_and_pick(title: &str, options: &[String]) -> Result<String> {
    if options.is_empty() {
        bail!("决策卡 options 为空（pi 工具参数异常）");
    }
    println!();
    println!("=============================================");
    println!("[alfred panel] {title}");
    for (i, opt) in options.iter().enumerate() {
        println!("  {}. {opt}", i + 1);
    }
    print!("请选择 (1-{}): ", options.len());
    std::io::stdout().flush()?;
    let mut line = String::new();
    let n = std::io::stdin().read_line(&mut line)?;
    if n == 0 {
        bail!("stdin EOF：未读到属主选择");
    }
    let idx: usize = line
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("无效输入（需 1-{} 的数字）：{}", options.len(), line.trim()))?;
    if idx == 0 || idx > options.len() {
        bail!("选择越界：{idx}（1-{}）", options.len());
    }
    Ok(options[idx - 1].clone())
}

/// 选项串 → 属主决策。
fn choice_from_option(opt: &str) -> Result<DecideChoice> {
    match opt {
        OPTION_RETRY => Ok(DecideChoice::Retry),
        OPTION_REVISE => Ok(DecideChoice::Revise),
        OPTION_ABANDON => Ok(DecideChoice::Abandon),
        other => bail!("属主选择了未知选项：{other}（面板选项不匹配）"),
    }
}

/// 调 `alfred decide` 续跑（用当前可执行文件，继承 env）。
fn invoke_decide(args: &PanelArgs, choice: DecideChoice) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe（alfred 二进制）")?;
    let mut cmd = Command::new(&exe);
    cmd.arg("decide")
        .arg("--run-dir")
        .arg(&args.run_dir)
        .arg("--decision")
        .arg(choice_label(choice));
    if choice == DecideChoice::Revise {
        // 改契约：需属主补充新需求（--message 文件 或 交互式 stdin）。
        let msg = if let Some(p) = &args.message {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("read message {}", p.display()))?;
            text.trim().to_string()
        } else {
            print!("请输入修改后的需求（改契约后重新规划）：");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        };
        if msg.is_empty() {
            bail!("改契约需要属主提供新需求（--message 文件或交互式输入）");
        }
        let msg_path = args.run_dir.join("panel-owner-message.txt");
        std::fs::write(&msg_path, &msg).context("write panel-owner-message.txt")?;
        cmd.arg("--message").arg(&msg_path);
    }
    let status = cmd.status().context("run `alfred decide`")?;
    if !status.success() {
        bail!("`alfred decide` 退出码非 0：{status}");
    }
    Ok(())
}

/// 写一行 JSON 到 pi stdin + 落 send 日志。
fn write_pi(stdin: &Arc<Mutex<ChildStdin>>, session_log: &Path, msg: &Value) -> Result<()> {
    let text = serde_json::to_string(msg).context("serialize pi stdin msg")?;
    {
        let mut w = stdin.lock().map_err(|_| anyhow::anyhow!("poisoned stdin lock"))?;
        w.write_all(text.as_bytes())
            .and_then(|_| w.write_all(b"\n"))
            .and_then(|_| w.flush())
            .context("write pi stdin")?;
    }
    log_json(session_log, "send", &text)
}

/// 会话日志：事件元数据行。
fn log_meta(session_log: &Path, event: &str, data: &Value) -> Result<()> {
    let line = json!({ "dir": "meta", "event": event, "data": data });
    append_line(session_log, &line)
}

/// 会话日志：收发 JSON 消息行（原始文本）。
fn log_json(session_log: &Path, dir: &str, raw: &str) -> Result<()> {
    // 尝试结构化（JSON 则落 msg 对象；非 JSON 落 raw 字符串）。
    match serde_json::from_str::<Value>(raw) {
        Ok(msg) => {
            let line = json!({ "dir": dir, "msg": msg });
            append_line(session_log, &line)
        }
        Err(_) => {
            let line = json!({ "dir": dir, "raw": raw });
            append_line(session_log, &line)
        }
    }
}

fn append_line(path: &Path, line: &Value) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open session log {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("append session log {}", path.display()))?;
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
