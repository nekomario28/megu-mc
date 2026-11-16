//! Persistence tests run in a fresh process: a loader touches many global stores and
//! derived indexes, so saving only a subset of them is not a reversible fixture.

/// The data files this branch ships in the tree. `tools/vanilla_data/gen_vanilla_data.sh`
/// can rewrite them from a 1.21.1 jar, but they are committed here on purpose: the suite
/// reads them, so a checkout without them is a broken checkout, not a normal state.
const REQUIRED_DATA: &[&str] = &[
    "assets/blocks_1211.json",
    "assets/pblocks.json",
    "assets/properties.json",
    "datapacks/vanilla/multi_noise_biome_tree.json",
    "datapacks/vanilla/data/minecraft/worldgen/noise_settings/overworld.json",
];

/// Fail with a clear message unless the data a world test reads is there.
///
/// Every test that reads it calls this first, before it takes a global lock: a panic
/// while holding one poisons it, and the next test then fails with `PoisonError` instead
/// of the reason, which reads as dozens of unrelated failures.
pub(crate) fn require_vanilla_data() {
    if REQUIRED_DATA
        .iter()
        .any(|p| !std::path::Path::new(p).exists())
    {
        panic!("{}", crate::worldgen::VANILLA_DATA_MISSING);
    }
    // Present is not enough: a `datapacks/vanilla/` copied from an older pack uses another
    // schema (`material_rule`, `aquifers`, no `surface_rule`), loads without an error and
    // then shifts every value the frozen baselines pin, which reads as a code regression.
    let current_schema = std::fs::read_to_string(
        "datapacks/vanilla/data/minecraft/worldgen/noise_settings/overworld.json",
    )
    .is_ok_and(|s| s.contains("\"surface_rule\""));
    if !current_schema {
        panic!(
            "Stale vanilla data: {} ",
            crate::worldgen::VANILLA_DATA_MISSING
        );
    }
}

/// The name libtest gave this test, which is also the selector `--exact` needs.
fn harness_name() -> String {
    std::thread::current()
        .name()
        .expect("test harness thread has a name")
        .to_string()
}

/// Where the child leaves the proof that it entered its body. `--exact` on a name libtest
/// cannot select runs zero tests and exits 0, which would read exactly like a passing fixture,
/// so the parent requires this receipt and not just a zero exit.
fn receipt_path(name: &str) -> std::path::PathBuf {
    let tag: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("mc-spine-receipt-{}-{tag}", std::process::id()))
}

/// True when this process *is* the isolated child for the test it is running. It writes the
/// receipt the parent checks, so "ran and panicked" and "never started" stay distinguishable.
pub(crate) fn is_isolated_child() -> bool {
    let name = harness_name();
    if std::env::var("MC_SPINE_TEST_CHILD").as_deref() != Ok(name.as_str()) {
        return false;
    }
    if let Ok(path) = std::env::var("MC_SPINE_TEST_RECEIPT") {
        let _ = std::fs::write(path, &name);
    }
    true
}

/// Add a line to the receipt. A fixture that destroys process state appends what it saw after
/// writing, so the parent can tell "the child wrote and then panicked" from "the child did not
/// run": both leave the parent's own scene untouched.
pub(crate) fn child_note(note: &str) {
    if let Ok(path) = std::env::var("MC_SPINE_TEST_RECEIPT") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(file, "{note}");
        }
    }
}

/// Run exactly `name` in a child and return its status with the receipt it left. Bounded: a
/// fixture that hangs is a failure of that fixture, not a hung suite.
fn run_child(name: &str) -> (std::process::ExitStatus, String) {
    let receipt = receipt_path(name);
    let _ = std::fs::remove_file(&receipt);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env("MC_SPINE_TEST_CHILD", name)
        .env("MC_SPINE_TEST_RECEIPT", &receipt)
        .spawn()
        .expect("start isolated world fixture");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for isolated fixture") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated world fixture {name} exceeded 60 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let body = std::fs::read_to_string(&receipt).unwrap_or_default();
    let _ = std::fs::remove_file(&receipt);
    (status, body)
}

/// The same runner for a fixture whose *panic* is the observation. The caller decides what the
/// status means; the receipt and the bound are not weakened for it.
pub(crate) fn run_failing_child(name: &str) -> (std::process::ExitStatus, String) {
    run_child(name)
}

pub(crate) fn isolated_world_test() -> bool {
    if is_isolated_child() {
        return false;
    }
    let name = harness_name();
    let (status, body) = run_child(&name);
    assert_eq!(
        body, name,
        "the isolated fixture process never entered {name}; a child that selects no test exits 0"
    );
    assert!(
        status.success(),
        "isolated world fixture {name} failed: {status}"
    );
    true
}
