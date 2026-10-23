//! Persistence tests run in a fresh process: a loader touches many global stores and
//! derived indexes, so saving only a subset of them is not a reversible fixture.

pub(crate) fn isolated_world_test() -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("test harness thread has a name");
    if std::env::var("MC_SPINE_TEST_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env("MC_SPINE_TEST_CHILD", name)
        .spawn()
        .expect("start isolated world fixture");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("wait for isolated fixture") {
            assert!(
                status.success(),
                "isolated world fixture {name} failed: {status}"
            );
            return true;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated world fixture {name} exceeded 60 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
