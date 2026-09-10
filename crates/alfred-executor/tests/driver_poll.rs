//! alfred-executor 黑盒测试：G1 驱动子进程死亡路由（poll 期进程存活检测）。
//!
//! 可观测行为（真 spawn + 真 poll，不起容器——驱动脚本只模拟死亡/落 done）：
//!   1. 驱动子进程被 SIGKILL（无 done）→ [`DriverOutcome::Crashed(None)`] 快速
//!      返回（不等满超时）——进程存活检测（Child try_wait 收割，B6 范式）的
//!      回归锁：driver 静默死亡无 done 不得演成"等满超时"或永等。
//!   2. 驱动子进程带退出码死亡（无 done）→ `Crashed(Some(code))`。
//!   3. 驱动子进程写 done 记录 → [`DriverOutcome::Done`]（status 透传）。
//!
//! 治理侧消费语义（Crashed → fail_run 落 state.json=crashed → 机械重跑路由）
//! 由 governance 侧单测与 e2e r3（机械重跑面）覆盖。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alfred_executor::config::ExecutorModel;
use alfred_executor::driver::{poll_container_driver, spawn_container_driver, DriverOutcome};

/// 占位模型（raw 内建形态，无 key/base_url 注入——poll 路径不消费模型）。
fn model() -> ExecutorModel {
    ExecutorModel {
        provider: "p".into(),
        model: "mockllm/model".into(),
        base_url: String::new(),
        api_key: String::new(),
        max_tokens: 8192,
        raw_id: true,
    }
}

fn temp_work_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "alfred-driver-poll-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 生成驱动脚本并 spawn + poll（黑盒 seam：与 execute_run 同一生命周期）。
fn spawn_and_poll(label: &str, script: &str, timeout_secs: u64) -> (DriverOutcome, Duration) {
    let dir = temp_work_dir(label);
    let driver_py = dir.join("driver.py");
    std::fs::write(&driver_py, script).unwrap();
    let mut launch =
        spawn_container_driver(&driver_py, &model(), &dir).expect("spawn driver child");
    let started = Instant::now();
    let outcome = poll_container_driver(&mut launch, timeout_secs).expect("poll driver");
    (outcome, started.elapsed())
}

#[test]
fn driver_sigkilled_without_done_routes_crashed_fast() {
    // G1：驱动被 kill -9、无 done → Crashed(None)（信号终止无退出码），且远在
    // 超时死线前返回（进程存活检测生效，不是等满 60s）。
    let (outcome, elapsed) = spawn_and_poll(
        "sigkill",
        "import os, signal\nos.kill(os.getpid(), signal.SIGKILL)\n",
        60,
    );
    assert_eq!(outcome, DriverOutcome::Crashed(None), "SIGKILL 无 done = Crashed(None)");
    assert!(
        elapsed < Duration::from_secs(30),
        "进程死亡必须在死线前被检出（耗时 {elapsed:?}）"
    );
}

#[test]
fn driver_exit_code_without_done_routes_crashed() {
    // 带退出码死亡（无 done）→ Crashed(Some(code))。
    let (outcome, elapsed) = spawn_and_poll("exitcode", "import os\nos._exit(9)\n", 60);
    assert_eq!(
        outcome,
        DriverOutcome::Crashed(Some(9)),
        "非零退出无 done = Crashed(Some(9))"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "进程死亡必须在死线前被检出（耗时 {elapsed:?}）"
    );
}

#[test]
fn driver_done_record_routes_done() {
    // 正常收尾：写 done 记录 → Done（status 透传给 execute_run 消费）。
    let dir = temp_work_dir("done");
    let script = format!(
        "import json\nwith open({done:?}, \"w\") as f:\n    json.dump({{\"event\": \"done\", \"status\": \"success\"}}, f)\n",
        done = dir.join("driver.done.json").to_string_lossy(),
    );
    std::fs::write(dir.join("driver.py"), script).unwrap();
    let mut launch =
        spawn_container_driver(&dir.join("driver.py"), &model(), &dir).expect("spawn driver child");
    let outcome = poll_container_driver(&mut launch, 60).expect("poll driver");
    assert_eq!(
        outcome,
        DriverOutcome::Done(alfred_executor::driver::DriverDone {
            status: "success".into(),
            error: None,
        }),
        "done 记录 = Done（status 透传）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
