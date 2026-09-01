//! alfred-executor 黑盒测试：宿主侧 Python 解析链（P1 恢复）。
//!
//! `python_binary()` 的可观测行为：
//!   1. `ALFRED_PYTHON` 环境变量优先，原样返回；
//!   2. 未设且 cwd 下 `.plans/r0-lab/venv/bin/python` 存在 → 返回其绝对路径；
//!   3. 两者皆无 → PATH 上的 `python3`。
//!
//! env / cwd 是进程全局可变状态，本文件全部测试串行化防并行竞态。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use alfred_executor::driver::python_binary;

static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvGuard { key, prev }
    }
    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        EnvGuard { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

struct CwdGuard {
    prev: PathBuf,
}

impl CwdGuard {
    fn enter(dir: &Path) -> Self {
        let prev = std::env::current_dir().expect("read cwd");
        std::fs::create_dir_all(dir).expect("create temp dir");
        std::env::set_current_dir(dir).expect("set cwd");
        CwdGuard { prev }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
    }
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "alfred-python-binary-{label}-{}",
        std::process::id()
    ))
}

/// 造一个伪 venv python 文件（`is_file` 只看存在性，内容无所谓）。
fn make_venv_python(root: &Path) -> PathBuf {
    let py = root.join(".plans/r0-lab/venv/bin/python");
    std::fs::create_dir_all(py.parent().expect("venv bin parent"))
        .expect("create venv bin dir");
    std::fs::write(&py, "#!/bin/sh\nexit 0\n").expect("write fake venv python");
    py
}

#[test]
fn alfred_python_env_override_wins() {
    let _g = lock();
    let _e = EnvGuard::set("ALFRED_PYTHON", "/opt/venv/bin/python");
    assert_eq!(python_binary(), "/opt/venv/bin/python");
}

#[test]
fn falls_back_to_repo_venv_python_abs() {
    let _g = lock();
    let _e = EnvGuard::unset("ALFRED_PYTHON");
    let root = temp_dir("venv");
    let py = make_venv_python(&root);
    let _c = CwdGuard::enter(&root);

    let got = python_binary();
    let got_path = Path::new(&got);
    assert!(
        got_path.is_absolute(),
        "venv 分支须返回绝对路径（spawn cwd=run_dir）：{got}"
    );
    assert!(
        got_path.ends_with(".plans/r0-lab/venv/bin/python"),
        "应命中 cwd 下 venv python：{got}"
    );
    // canonicalize 消掉 /var → /private/var 类符号链接差异后逐字节一致。
    assert_eq!(
        std::fs::canonicalize(&got).expect("canonicalize got"),
        std::fs::canonicalize(&py).expect("canonicalize py"),
        "解析出的 venv python 即伪 venv python 文件"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn falls_back_to_path_python3_when_no_venv() {
    let _g = lock();
    let _e = EnvGuard::unset("ALFRED_PYTHON");
    let root = temp_dir("empty");
    let _c = CwdGuard::enter(&root);

    assert_eq!(python_binary(), "python3");

    let _ = std::fs::remove_dir_all(&root);
}
