//! alfred-executor 黑盒测试：宿主侧 Python 解析链（P1 恢复 + codux PTY 根治）。
//!
//! `python_binary()` 的可观测行为：
//!   1. `ALFRED_PYTHON` 环境变量优先，原样返回；
//!   2. 未设且 `current_exe()` 上两级定位的仓根下 `.plans/r0-lab/venv/bin/python`
//!      存在 → 返回其绝对路径（与 cwd 无关——cwd 是任意项目目录也命中）；
//!   3. 两者皆无 → PATH 上的 `python3`。
//!
//! env / cwd 是进程全局可变状态，本文件全部测试串行化防并行竞态。
//!
//! 测试 bin 位于 `<workspace>/target/debug/deps/`（或 release 同构）——测试 exe
//! 与 alfred bin 同居 `target/<profile>/deps/`，向上两级即达 workspace 根，与
//! 生产行为同构（alfred bin 在 `target/<profile>/`）。两例对照：默认（测试 exe
//! 的仓根 = 本 workspace，通常无 venv → PATH 回退）+ 显式在 workspace 根造 venv
//! → exe 相对解析命中（cwd 故意放在别处，证明 cwd 无关）。

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

/// 在 root 下造一个伪 venv python 文件（`is_file` 只看存在性，内容无所谓）。
/// 返回 (python 路径, 造出的 .plans 根)——清理只删 .plans 子树（root 本身是
/// workspace 根/target，绝不可删）。
fn make_venv_python(root: &Path) -> (PathBuf, PathBuf) {
    let plans = root.join(".plans");
    let py = plans.join("r0-lab/venv/bin/python");
    std::fs::create_dir_all(py.parent().expect("venv bin parent")).expect("create venv bin dir");
    std::fs::write(&py, "#!/bin/sh\nexit 0\n").expect("write fake venv python");
    (py, plans)
}

#[test]
fn alfred_python_env_override_wins() {
    let _g = lock();
    let _e = EnvGuard::set("ALFRED_PYTHON", "/opt/venv/bin/python");
    assert_eq!(python_binary(), "/opt/venv/bin/python");
}

#[test]
fn falls_back_to_exe_relative_repo_venv_python() {
    let _g = lock();
    let _e = EnvGuard::unset("ALFRED_PYTHON");
    let root = exe_repo_root();
    let (py, plans) = make_venv_python(&root);
    // cwd 故意放在与仓根无关的临时目录——解析必须与 cwd 无关。
    let cwd = temp_dir("unrelated-cwd");
    let _c = CwdGuard::enter(&cwd);

    let got = python_binary();
    let got_path = Path::new(&got);
    assert!(
        got_path.is_absolute(),
        "venv 分支须返回绝对路径（spawn cwd=run_dir）：{got}"
    );
    assert!(
        got_path.ends_with(".plans/r0-lab/venv/bin/python"),
        "应命中 exe 相对定位的 venv python：{got}"
    );
    // canonicalize 消掉 /var → /private/var 类符号链接差异后逐字节一致。
    assert_eq!(
        std::fs::canonicalize(&got).expect("canonicalize got"),
        std::fs::canonicalize(&py).expect("canonicalize py"),
        "解析出的 venv python 即伪 venv python 文件"
    );

    let _ = std::fs::remove_dir_all(&plans);
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn falls_back_to_path_python3_when_no_venv() {
    let _g = lock();
    let _e = EnvGuard::unset("ALFRED_PYTHON");
    let root = exe_repo_root();
    let marker = root.join(".plans/r0-lab/venv/bin/python");
    // 本 workspace 的 target/debug(deps) 上两级就是本仓——若仓里真有 venv（开发
    // 机常态），PATH 回退分支无法在此进程内验证；此时跳过（exe 相对分支已由
    // falls_back_to_exe_relative_repo_venv_python 以受控伪 venv 覆盖）。
    if marker.is_file() {
        return;
    }
    let cwd = temp_dir("empty");
    let _c = CwdGuard::enter(&cwd);

    assert_eq!(python_binary(), "python3");

    let _ = std::fs::remove_dir_all(&cwd);
}

/// 测试 exe 定位的"仓根"（与 python_binary() 的 venv 解析同一推导：current_exe
/// 上两级）。测试 bin 在 `<workspace>/target/<profile>/deps/` → 上两级 =
/// `<workspace>/target`——venv 落点即 `<workspace>/target/.plans/...`。生产行为
/// 的 alfred bin 在 `<workspace>/target/<profile>/` → 上两级 = workspace 根；
/// 两者同构（venv 都在"exe 上两级 /.plans"下），测试无需造三层嵌套目录。
fn exe_repo_root() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("test exe lives at <root>/target/<profile>/deps/")
        .to_path_buf()
}
