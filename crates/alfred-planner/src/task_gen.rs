//! planner Inspect Task 定义生成（Rust 生成 Python 文件）。
//!
//! 复用 executor 的 token 替换机制：字符串值经 JSON 编码注入（值语义一致），
//! 数字 token 注入裸数字。模板见 templates/planner_task.py.tmpl。

use anyhow::{Context, Result};

/// 内嵌的 planner pi 任务模板（见 templates/planner_task.py.tmpl）。
const PLANNER_TASK_TEMPLATE: &str = include_str!("../templates/planner_task.py.tmpl");

/// 生成参数（全部经 token 替换注入模板）。
pub struct PlannerTaskGenParams {
    /// 沙箱 compose 文件绝对路径。
    pub compose_file: String,
    /// "converse" | "maintain"（容器内 pi 的任务模式）。
    pub mode: String,
    /// 容器侧 system prompt（converse 建图 schema / maintain 会话文档维护，逐字搬自 converse.rs / maintain.rs）。
    pub system_prompt: String,
    /// 容器侧 driver prompt（读 /inputs → 按 SYSTEM_PROMPT 规则 → 写 /outputs/<file>）。
    pub driver_prompt: String,
    /// 容器内主产出文件绝对路径（"/outputs/instructions.json" 或 "/outputs/session.json"）。
    pub output_file: String,
    /// 容器内次产出文件绝对路径（converse §2.4 两分支答复侧 "/outputs/reply.txt"）；
    /// 空串 = 单文件（maintain）。
    pub output_file_alt: String,
    /// AGT 扩展路径（"/tmp/.agt/agt-policy.ts"）；空串 = 不加载。
    pub agt_ext: String,
    /// AGT 策略文件容器内路径（"/tmp/.agt/policy.json"）；与 `agt_ext` 同空。
    pub agt_policy_path: String,
    /// AGT 审计文件容器内路径（"/tmp/.agt/audit/audit.jsonl"）；与 `agt_ext` 同空。
    pub agt_audit_path: String,
    /// 桥代理端口基数（每样本自增）。
    pub port_base: u32,
    /// pi 模型（provider/model 形态，如 "inspect-bridge/inspect"）。
    pub pi_model: String,
    /// 容器内工作区路径（"/workspace"）。
    pub workspace_dir: String,
    /// 容器内执行用户（"root"）。
    pub sandbox_user: String,
    /// 样本 id（run id）。
    pub run_id: String,
    /// settled 后的宽限秒数（进程未在 EOF 退出则 kill）。
    pub settle_grace_seconds: f64,
}

/// 生成 task.py 内容。
pub fn generate_planner_task_py(params: &PlannerTaskGenParams) -> Result<String> {
    let mut out = PLANNER_TASK_TEMPLATE.to_string();

    let inject: &[(&str, String)] = &[
        ("__COMPOSE_FILE_JSON__", json(&params.compose_file)?),
        ("__MODE_JSON__", json(&params.mode)?),
        ("__SYSTEM_PROMPT_JSON__", json(&params.system_prompt)?),
        ("__DRIVER_PROMPT_JSON__", json(&params.driver_prompt)?),
        ("__OUTPUT_FILE_JSON__", json(&params.output_file)?),
        ("__OUTPUT_FILE_ALT_JSON__", json(&params.output_file_alt)?),
        ("__AGT_EXT_JSON__", json(&params.agt_ext)?),
        ("__AGT_POLICY_PATH_JSON__", json(&params.agt_policy_path)?),
        ("__AGT_AUDIT_PATH_JSON__", json(&params.agt_audit_path)?),
        ("__PORT_BASE__", params.port_base.to_string()),
        ("__PI_MODEL_JSON__", json(&params.pi_model)?),
        ("__WORKSPACE_DIR_JSON__", json(&params.workspace_dir)?),
        ("__SANDBOX_USER_JSON__", json(&params.sandbox_user)?),
        ("__RUN_ID_JSON__", json(&params.run_id)?),
        (
            "__SETTLE_GRACE_SECONDS__",
            format!("{}", params.settle_grace_seconds),
        ),
    ];
    for (token, value) in inject {
        if !out.contains(token) {
            anyhow::bail!("planner_task template missing token {token}");
        }
        out = out.replace(token, value);
    }

    // 防呆：替换后不得残留任何 __TOKEN__
    for token in [
        "__COMPOSE_FILE_JSON__",
        "__MODE_JSON__",
        "__SYSTEM_PROMPT_JSON__",
        "__DRIVER_PROMPT_JSON__",
        "__OUTPUT_FILE_JSON__",
        "__OUTPUT_FILE_ALT_JSON__",
        "__AGT_EXT_JSON__",
        "__AGT_POLICY_PATH_JSON__",
        "__AGT_AUDIT_PATH_JSON__",
        "__PORT_BASE__",
        "__PI_MODEL_JSON__",
        "__WORKSPACE_DIR_JSON__",
        "__SANDBOX_USER_JSON__",
        "__RUN_ID_JSON__",
        "__SETTLE_GRACE_SECONDS__",
    ] {
        if out.contains(token) {
            anyhow::bail!("planner_task token replacement incomplete: {token}");
        }
    }

    Ok(out)
}

fn json(s: &str) -> Result<String> {
    serde_json::to_string(s).context("json-encode template value")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(mode: &str) -> PlannerTaskGenParams {
        PlannerTaskGenParams {
            compose_file: "/run/planner/compose.yaml".into(),
            mode: mode.into(),
            system_prompt: "system 规则：建图指令序列。".into(),
            driver_prompt: "读 /inputs，写 /outputs/instructions.json".into(),
            output_file: "/outputs/instructions.json".into(),
            output_file_alt: "/outputs/reply.txt".into(),
            agt_ext: "/tmp/.agt/agt-policy.ts".into(),
            agt_policy_path: "/tmp/.agt/policy.json".into(),
            agt_audit_path: "/tmp/.agt/audit/audit.jsonl".into(),
            port_base: 13200,
            pi_model: "inspect-bridge/inspect".into(),
            workspace_dir: "/workspace".into(),
            sandbox_user: "root".into(),
            run_id: "run-planner-test".into(),
            settle_grace_seconds: 20.0,
        }
    }

    #[test]
    fn generates_valid_python_with_values() {
        let py = generate_planner_task_py(&params("converse")).unwrap();
        assert!(py.contains(r#"COMPOSE_FILE = "/run/planner/compose.yaml""#));
        assert!(py.contains(r#"MODE = "converse""#));
        assert!(py.contains(r#"RUN_ID = "run-planner-test""#));
        assert!(py.contains(r#"PORT_BASE = int(13200)"#));
        assert!(py.contains(r#"PI_MODEL = "inspect-bridge/inspect""#));
        assert!(py.contains(r#"OUTPUT_FILE_ALT = "/outputs/reply.txt""#));
        assert!(py.contains(r#"AGT_EXT = "/tmp/.agt/agt-policy.ts""#));
        assert!(py.contains(r#"AGT_EXT = "/tmp/.agt/agt-policy.ts""#));
        assert!(py.contains(r#"AGT_POLICY_PATH = "/tmp/.agt/policy.json""#));
        assert!(py.contains(r#"AGT_AUDIT_PATH = "/tmp/.agt/audit/audit.jsonl""#));
        // 不得残留 token
        assert!(!py.contains("__COMPOSE_FILE_JSON__"));
        assert!(!py.contains("__MODE_JSON__"));
        assert!(!py.contains("__SYSTEM_PROMPT_JSON__"));
        assert!(!py.contains("__DRIVER_PROMPT_JSON__"));
        assert!(!py.contains("__OUTPUT_FILE_JSON__"));
        assert!(!py.contains("__AGT_EXT_JSON__"));
        assert!(!py.contains("__AGT_POLICY_PATH_JSON__"));
        assert!(!py.contains("__AGT_AUDIT_PATH_JSON__"));
        assert!(!py.contains("__PORT_BASE__"));
    }

    #[test]
    fn maintain_mode_and_empty_agt() {
        let mut p = params("maintain");
        p.agt_ext = "".into();
        p.agt_policy_path = "".into();
        p.agt_audit_path = "".into();
        p.output_file = "/outputs/session.json".into();
        p.output_file_alt = "".into();
        let py = generate_planner_task_py(&p).unwrap();
        assert!(py.contains(r#"MODE = "maintain""#));
        assert!(py.contains(r#"OUTPUT_FILE = "/outputs/session.json""#));
        assert!(py.contains(r#"OUTPUT_FILE_ALT = """#));
        assert!(py.contains(r#"AGT_EXT = """#));
        assert!(py.contains(r#"AGT_POLICY_PATH = """#));
        assert!(py.contains(r#"AGT_AUDIT_PATH = """#));
        // 空 AGT_EXT 时模板的 `if AGT_EXT:` 分支保留（运行期跳过）
        assert!(py.contains("if AGT_EXT:"));
    }

    #[test]
    fn missing_token_errors() {
        // 模板缺 token → 显式报错（不静默产出坏 py）
        let mut p = params("converse");
        p.compose_file = "/x".into();
        let py = generate_planner_task_py(&p).unwrap();
        assert!(!py.contains("__SETTLE_GRACE_SECONDS__"));
        // SETTLE_GRACE_SECONDS 是数字 token：注入后必须可被 float() 解析
        // （Rust 格式化 20.0 输出 "20"——float(20) 是合法 Python 字面量）
        assert!(py.contains("SETTLE_GRACE_SECONDS = float(20)"));
    }

    #[test]
    fn prompt_with_quotes_and_newlines_survives() {
        // 字符串 token 注入的是 JSON 字面量（值语义一致），直接赋值即可
        let mut p = params("converse");
        p.system_prompt = "Say \"hi\"\nand 'bye'\n\\backslash {{escaped}}".into();
        let py = generate_planner_task_py(&p).unwrap();
        assert!(py.contains("SYSTEM_PROMPT = "));
        assert!(py.contains("{{escaped}}"));
    }
}
