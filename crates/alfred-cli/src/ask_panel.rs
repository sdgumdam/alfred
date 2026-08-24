//! ask panel：run 升级退出（PlanRejected / Escalated）时自动 spawn pi 交互式 TUI
//! 会话，通过 pi 原生 ask 工具渲染三按钮卡片（重跑 / 改契约 / 放弃），属主键盘
//! 选择后回调 alfred decide，使升级→决策在同一次 `alfred run` 内闭环。
//!
//! 触发条件（run.rs 调用方判定）：
//!   - 状态到达挂起态（PlanRejected / Escalated）
//!   - 非离线模式（offline_mode == false，离线 = 无 LLM，pi 无意义）
//!   - ALFRED_NO_ASK_PANEL != "1"（e2e/CI 逃生舱，显式跳过）
//! 任一不满足时 run 直接 EXIT_ESCALATED（exit 1），保持现有行为。
//!
//! pi 是 TUI 应用，依赖 PTY；本模块用 Command 继承 stdio，让 pi 拿到调用方
//! 终端的 PTY。pi 内置 ask 工具会渲染 2-5 选项卡片，属主用方向键+回车选择，
//! 选择结果出现在 pi 的最终文本输出里。本模块用关键词匹配解析该输出：
//!   retry / 重跑 → Retry
//!   revise / 改契约 → ReviseContract
//!   abandon / 放弃 → Abandon
//! 无法解析时返回 AskPanelError，run 回退到提示手动 alfred decide + exit 1。

use alfred_core::{OwnerDecision, State};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// ask panel 的错误：pi 启动失败、退出码非零、或输出无法解析为三选一。
#[derive(Debug, PartialEq)]
pub struct AskPanelError(String);

impl std::fmt::Display for AskPanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AskPanelError {}

/// 是否应触发 ask panel。run.rs 在到达挂起态后调用本函数判定。
/// 三条都满足才触发：挂起态、非离线、未显式禁用。
/// 离线模式本身就是"无 LLM / 无 Docker"的逃生机制，pi 无意义；
/// ALFRED_NO_ASK_PANEL=1 是真实路径下的额外逃生舱（CI 不能弹 TUI）。
pub fn should_trigger(state: State) -> bool {
    if !matches!(state, State::Escalated | State::PlanRejected) {
        return false;
    }
    if alfred_core::offline_mode() {
        return false;
    }
    !no_ask_panel()
}

/// ALFRED_NO_ASK_PANEL=1 跳过 pi 卡片。
fn no_ask_panel() -> bool {
    std::env::var("ALFRED_NO_ASK_PANEL")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// spawn pi 交互式会话，注入升级上下文，要求属主用 ask 工具做三选一决策。
/// 解析 pi 最终输出里的选择关键词，映射到 OwnerDecision。
///
/// 入参 verdict_json：从 verdicts.jsonl 读出的最后一条裁决（plan 或 exec），
/// 用于在 system prompt 里说明为什么升级。
pub fn spawn_ask_panel(
    state: State,
    verdict_json: &Value,
    run_dir: &Path,
    attempts: u32,
    budget: u32,
) -> Result<OwnerDecision, AskPanelError> {
    let system_prompt = build_system_prompt(state, verdict_json, run_dir, attempts, budget);
    let output = run_pi(&system_prompt)?;
    parse_decision(&output.stdout)
}

/// 构造注入 pi 的 system prompt：升级上下文 + 要求调 ask 工具给三选项。
fn build_system_prompt(
    state: State,
    verdict_json: &Value,
    run_dir: &Path,
    attempts: u32,
    budget: u32,
) -> String {
    let state_name = crate::store::state_name(state);
    // verdict 的 reason / explanation 字段；plan 用 reason，exec 用 explanation。
    let reason = verdict_json
        .get("verdict")
        .and_then(|v| {
            v.get("reason")
                .or_else(|| v.get("explanation"))
                .and_then(|r| r.as_str())
        })
        .unwrap_or("(no reason recorded)");
    let run_dir_display = run_dir.display();
    let verdict_summary = serde_json::to_string_pretty(verdict_json).unwrap_or_default();
    format!(
        "你是 alfred 治理环的属主决策助手。当前 run 已升级到挂起态，需要属主拍板。\n\
         ## 升级上下文\n\
         - 当前状态：{state_name}\n\
         - run 目录：{run_dir_display}\n\
         - mechanical 重试预算：{attempts}/{budget}（已用/上限）\n\
         - 最近裁决（来自 verdicts.jsonl）：\n\
         ```json\n{verdict_summary}\n```\n\
         - 裁决理由：{reason}\n\
         ## 任务\n\
         请调用你的 ask 工具，向属主呈现三选一卡片：\n\
         1. 重跑（retry）：同契约重新执行\n\
         2. 改契约（revise-contract）：回到计划阶段重写契约\n\
         3. 放弃（abandon）：终止本次 run\n\
         属主选择后，请直接回复对应的关键词之一：retry、revise-contract 或 abandon。\n\
         不要做其他事情，不要调用其他工具。"
    )
}
/// spawn pi 交互式：--append-system-prompt 注入上下文，用户 prompt 触发决策。
/// stdin/stderr 继承调用方终端的 PTY，让 pi 原生 ask 工具渲染交互卡片；
/// stdout 管道捕获 pi 最终文本输出（模型选择后回复的选择关键词），供解析。
/// pi 的 TUI 渲染走直接 TTY 写入或 stderr，不污染 stdout 的文本响应。
fn run_pi(system_prompt: &str) -> Result<std::process::Output, AskPanelError> {
    Command::new("pi")
        .arg("--append-system-prompt")
        .arg(system_prompt)
        .arg("--no-session")
        .arg("请为升级做决策")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|err| AskPanelError(format!("failed to spawn pi: {err}")))
        .and_then(|output| {
            if !output.status.success() {
                return Err(AskPanelError(format!(
                    "pi exited with status {}",
                    output.status.code().unwrap_or(-1)
                )));
            }
            Ok(output)
        })
}

/// 解析 pi 输出里的选择关键词，映射到 OwnerDecision。
/// pi 的 ask 工具选择后，选择项文本会出现在最终输出里。
/// 同时识别中文与英文关键词，按 retry → revise → abandon 优先级匹配
/// （abandon 最"重"，放最后避免误匹配 retry/re-visit 等）。
fn parse_decision(stdout: &[u8]) -> Result<OwnerDecision, AskPanelError> {
    let text = String::from_utf8_lossy(stdout);
    let lower = text.to_lowercase();

    // retry：英文 retry / 重跑 / 重试
    if lower.contains("retry") || text.contains("重跑") || text.contains("重试") {
        return Ok(OwnerDecision::Retry);
    }
    // revise-contract：英文 revise / 改契约 / 修改契约
    if lower.contains("revise") || text.contains("改契约") || text.contains("修改契约") {
        return Ok(OwnerDecision::ReviseContract);
    }
    // abandon：英文 abandon / 放弃
    if lower.contains("abandon") || text.contains("放弃") {
        return Ok(OwnerDecision::Abandon);
    }

    Err(AskPanelError(format!(
        "could not parse owner decision from pi output; \
         expected one of retry/revise-contract/abandon (或 重跑/改契约/放弃)\n\
         pi output:\n{text}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_retry_english() {
        assert_eq!(parse_decision(b"Owner selected: retry"), Ok(OwnerDecision::Retry));
    }

    #[test]
    fn parse_retry_chinese() {
        assert_eq!(parse_decision("属主选择了：重跑".as_bytes()), Ok(OwnerDecision::Retry));
    }

    #[test]
    fn parse_retry_chinese_variant() {
        assert_eq!(parse_decision("重试".as_bytes()), Ok(OwnerDecision::Retry));
    }

    #[test]
    fn parse_revise_english() {
        assert_eq!(
            parse_decision(b"Owner selected: revise-contract"),
            Ok(OwnerDecision::ReviseContract)
        );
    }

    #[test]
    fn parse_revise_chinese() {
        assert_eq!(
            parse_decision("属主选择了：改契约".as_bytes()),
            Ok(OwnerDecision::ReviseContract)
        );
    }

    #[test]
    fn parse_revise_chinese_variant() {
        assert_eq!(
            parse_decision("修改契约".as_bytes()),
            Ok(OwnerDecision::ReviseContract)
        );
    }

    #[test]
    fn parse_abandon_english() {
        assert_eq!(parse_decision(b"Owner selected: abandon"), Ok(OwnerDecision::Abandon));
    }

    #[test]
    fn parse_abandon_chinese() {
        assert_eq!(parse_decision("属主选择了：放弃".as_bytes()), Ok(OwnerDecision::Abandon));
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse_decision(b"RETRY"), Ok(OwnerDecision::Retry));
        assert_eq!(parse_decision(b"ABANDON"), Ok(OwnerDecision::Abandon));
    }

    #[test]
    fn parse_unknown_returns_error() {
        let result = parse_decision(b"pi crashed, no selection made");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("could not parse"));
    }

    #[test]
    fn parse_empty_output_returns_error() {
        assert!(parse_decision(b"").is_err());
    }

    #[test]
    fn parse_retry_takes_precedence_over_abandon_in_combined_text() {
        // "retry" 应优先于包含 "abandon" 的长文本
        let output = b"Previous attempt was abandoned, now retry";
        assert_eq!(parse_decision(output), Ok(OwnerDecision::Retry));
    }

    #[test]
    fn should_trigger_returns_false_for_non_suspended_state() {
        // 非挂起态提前返回 false，与 offline/no-ask-panel env 无关。
        std::env::set_var("ALFRED_OFFLINE", "1");
        assert!(!should_trigger(State::Completed));
        assert!(!should_trigger(State::Planning));
        assert!(!should_trigger(State::Executing));
        assert!(!should_trigger(State::PlanReviewing));
        assert!(!should_trigger(State::ExecReviewing));
    }

    #[test]
    fn should_trigger_returns_false_in_offline_mode() {
        // 离线模式 = 无 LLM，pi 无意义，挂起态也不触发。
        std::env::set_var("ALFRED_OFFLINE", "1");
        assert!(!should_trigger(State::Escalated));
        assert!(!should_trigger(State::PlanRejected));
    }

    #[test]
    fn should_trigger_returns_false_when_no_ask_panel_set() {
        // ALFRED_NO_ASK_PANEL=1 显式跳过，与 offline 无关（两个条件都返回 false）。
        std::env::set_var("ALFRED_OFFLINE", "1");
        std::env::set_var("ALFRED_NO_ASK_PANEL", "1");
        assert!(!should_trigger(State::Escalated));
        assert!(!should_trigger(State::PlanRejected));
        std::env::remove_var("ALFRED_NO_ASK_PANEL");
    }
}
