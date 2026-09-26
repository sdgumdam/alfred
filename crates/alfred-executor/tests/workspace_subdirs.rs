//! alfred-executor 集成测试：workspace_subdirs 列表级校验（A4 挂载锚窄边界）。
//!
//! 可观测行为（黑盒，经公共 API）：
//!   1. `compose_gen::validate_workspace_subdirs`（放行条件单一真源）：
//!      首子目录名在其余位重复声明 → 显式拒绝（防 compose 双挂载同宿主目录 +
//!      task_gen::mount_anchor_prompt「/workspace 下不存在同名嵌套子目录」锚断言
//!      矛盾）；互异声明放行；逐项相对/越界规则照旧生效。
//!   2. `generate_executor_compose`：重复声明 → Err（compose 生成不静默双挂载）；
//!      互异声明 → 首子目录挂 /workspace 根、其余按名挂 /workspace/<sub>，同一
//!      宿主目录恰一条挂载行（双挂载防御的可观测正路径）。
//!
//! 注意：`validate_executor_sandbox` 是 run.rs 私有函数（经 execute_run 全链
//! 生效）；单测层钉死放行条件真源本身（ref_volumes.rs 同范式）。

use std::fs;
use std::path::PathBuf;

use alfred_executor::compose_gen::{
    generate_executor_compose, validate_workspace_subdirs, ExecutorMounts,
};

/// compose 生成的 canonicalize 校验要求目录在 HOME 下（E3：colima 只共享 ~），
/// 测试目录不能放 /tmp（ref_volumes.rs 同范式）。
fn temp_dir_under_home(label: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join(format!(
        ".alfred-ws-subdirs-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 最小工作区（含契约声明的 subdir，非空挂载保证）。
fn make_ws(label: &str) -> PathBuf {
    let base = temp_dir_under_home(label);
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("src")).unwrap();
    ws
}

fn mounts(subs: &[&str]) -> ExecutorMounts {
    ExecutorMounts {
        workspace_subdirs: subs.iter().map(|s| (*s).to_string()).collect(),
        ref_volumes: vec![],
        agt_dir: None,
        agt_audit_dir: None,
        sessions_dir: None,
    }
}

// ---------------------------------------------------------------------------
// 放行条件真源：validate_workspace_subdirs
// ---------------------------------------------------------------------------

#[test]
fn first_name_repeated_in_rest_rejected() {
    // 缺陷场景（PostFixAudit P3）：["src","src"] 原样放行 → compose 双挂载同宿主目录。
    let err = validate_workspace_subdirs(&["src".into(), "src".into()]).unwrap_err();
    assert!(err.to_string().contains("重复声明"), "{err}");
    // 重复项不必相邻：首名出现在任意其余位都拒绝。
    let err = validate_workspace_subdirs(&["src".into(), "lib".into(), "src".into()]).unwrap_err();
    assert!(err.to_string().contains("重复声明"), "{err}");
}

#[test]
fn distinct_subdirs_pass() {
    validate_workspace_subdirs(&["src".into()]).unwrap();
    validate_workspace_subdirs(&["src".into(), "lib".into()]).unwrap();
}

#[test]
fn per_item_rules_still_apply() {
    // 列表级真源不放松单项规则（相对/非空/不越界，经 validate_workspace_subdir）。
    assert!(validate_workspace_subdirs(&["".into()]).is_err());
    assert!(validate_workspace_subdirs(&["/abs".into()]).is_err());
    assert!(validate_workspace_subdirs(&["..".into()]).is_err());
}

// ---------------------------------------------------------------------------
// compose 生成：重复声明拒绝 + 互异声明挂载布局
// ---------------------------------------------------------------------------

#[test]
fn compose_rejects_duplicate_declaration_not_silent() {
    let ws = make_ws("dup");
    let err = generate_executor_compose(&ws, "img:latest", &mounts(&["src", "src"])).unwrap_err();
    assert!(err.to_string().contains("重复声明"), "{err}");
}

#[test]
fn compose_with_distinct_subdirs_mounts_first_as_root() {
    let base = temp_dir_under_home("distinct");
    let ws = base.join("ws");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(ws.join("lib")).unwrap();
    let yaml = generate_executor_compose(&ws, "img:latest", &mounts(&["src", "lib"])).unwrap();
    // 挂载锚语义：首子目录挂为 /workspace 根，其余按名挂 /workspace/<sub>。
    assert!(
        yaml.contains(&format!("{}:/workspace:rw", ws.join("src").display())),
        "{yaml}"
    );
    assert!(
        yaml.contains(&format!("{}:/workspace/lib:rw", ws.join("lib").display())),
        "{yaml}"
    );
    // 同一宿主目录恰一条挂载行（双挂载防御的可观测正路径）。
    assert_eq!(
        yaml.lines()
            .filter(|l| l.contains(&ws.join("src").display().to_string()))
            .count(),
        1,
        "{yaml}"
    );
}

#[test]
fn task_network_requires_truthful_node_permission_and_keeps_restrictions() {
    use alfred_executor::compose_gen::{generate_task_env_compose, validate_task_network};
    let ws = make_ws("task-network");
    let original = ws.parent().unwrap().join("task.yaml");
    fs::write(&original, r#"
services:
  default:
    image: dependency:fixed
    command: tail -f /dev/null
    extra_hosts: ["github.com:127.0.0.1"]
    networks: [task]
    depends_on:
      mysql:
        condition: service_healthy
  mysql:
    image: mysql:fixed
    healthcheck:
      test: [CMD, mysqladmin, ping]
    networks: [task]
networks:
  task:
    internal: true
"#).unwrap();
    assert!(validate_task_network(false, Some(&original)).is_err());
    validate_task_network(true, Some(&original)).unwrap();
    assert!(validate_task_network(true, None).is_err());
    validate_task_network(false, None).unwrap();
    let rendered = generate_task_env_compose(&original, &ws, "executor:fixed", &mounts(&["src"])).unwrap();
    let before: serde_yaml::Value = serde_yaml::from_str(&fs::read_to_string(&original).unwrap()).unwrap();
    let after: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
    assert_eq!(before["networks"], after["networks"]);
    assert_eq!(before["services"]["mysql"], after["services"]["mysql"]);
    assert_eq!(before["services"]["default"]["extra_hosts"], after["services"]["default"]["extra_hosts"]);
    fs::write(&original, "services:\n  default:\n    network_mode: none\n").unwrap();
    assert!(validate_task_network(true, Some(&original)).is_err());
    validate_task_network(false, Some(&original)).unwrap();
}

#[test]
fn task_env_command_accepts_compose_string_or_argv_list() {
    // compose_gen 372-379 防呆只认 as_str：合法 Compose command 序列（argv
    // 形态，Bandit/ALFWorld 等任务 compose）被误报「缺 command」。契约：
    // 字符串与字符串序列两形态都放行；command 值逐字透传（不做 shell
    // join、字符串形态不被拆分）；缺失/空串/纯空白/空序列/全空串元素/
    // 非字符串元素仍按缺 command fail-closed。
    use alfred_executor::compose_gen::generate_task_env_compose;

    let ws = make_ws("command-forms");
    let original = ws.parent().unwrap().join("task.yaml");

    // argv 形态（非空字符串序列）→ 放行，渲染输出 command 逐字保留。
    fs::write(
        &original,
        "services:\n  default:\n    image: dependency:fixed\n    command: [\"tail\", \"-f\", \"/dev/null\"]\n",
    )
    .unwrap();
    let rendered =
        generate_task_env_compose(&original, &ws, "executor:fixed", &mounts(&["src"])).unwrap();
    let after: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
    assert_eq!(
        after["services"]["default"]["command"],
        serde_yaml::Value::Sequence(vec!["tail".into(), "-f".into(), "/dev/null".into()])
    );

    // 序列含空串元素（argv 合法边界）：有实际命令内容 → 放行且逐字透传。
    fs::write(
        &original,
        "services:\n  default:\n    image: dependency:fixed\n    command: [\"sh\", \"-c\", \"\"]\n",
    )
    .unwrap();
    let rendered =
        generate_task_env_compose(&original, &ws, "executor:fixed", &mounts(&["src"])).unwrap();
    let after: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
    assert_eq!(
        after["services"]["default"]["command"],
        serde_yaml::Value::Sequence(vec!["sh".into(), "-c".into(), "".into()])
    );

    // 缺 command 字段 → 拒绝（只断言行为，不匹配实现措辞）。
    fs::write(&original, "services:\n  default:\n    image: dependency:fixed\n").unwrap();
    assert!(
        generate_task_env_compose(&original, &ws, "executor:fixed", &mounts(&["src"])).is_err()
    );

    // null / 数字 / 映射 / 空序列 / 全空串元素 / 混合类型 / 纯空白字符串 → 拒绝。
    for bad in [
        "command:",
        "command: 42",
        "command: {sh: -c}",
        "command: []",
        "command: [\"\", \"\"]",
        "command: [1, \"tail\"]",
        "command: \"   \"",
    ] {
        fs::write(
            &original,
            format!("services:\n  default:\n    image: dependency:fixed\n    {bad}\n"),
        )
        .unwrap();
        assert!(
            generate_task_env_compose(&original, &ws, "executor:fixed", &mounts(&["src"]))
                .is_err(),
            "command {bad:?} should be rejected"
        );
    }
}
