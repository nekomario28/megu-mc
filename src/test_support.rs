//! Persistence tests run in a fresh process: a loader touches many global stores and
//! derived indexes, so saving only a subset of them is not a reversible fixture.

/// The data files this branch ships in the tree. `tools/vanilla_data/gen_vanilla_data.sh`
/// can rewrite them from a 1.21.1 jar, but they are committed here on purpose: the suite
/// reads them, so a checkout without them is a broken checkout, not a normal state.
const REQUIRED_DATA: &[&str] = &[
    "assets/blocks_1211.json",
    "assets/block_states_1211.json",
    "datapacks/vanilla/multi_noise_biome_tree.json",
    "datapacks/vanilla/data/minecraft/worldgen/noise_settings/overworld.json",
];

/// Fail with a clear message unless the data a world test reads is there.
///
/// Every test that reads it calls this first, before it takes a global lock: a panic
/// while holding one poisons it, and the next test then fails with `PoisonError` instead
/// of the reason, which reads as dozens of unrelated failures.
///
/// The check is anchored at `CARGO_MANIFEST_DIR`, not the process cwd: fixtures that
/// need a synthetic `datapacks/` move the cwd (`std::env::set_current_dir` is
/// process-global), and a relative `exists()` running beside one would report the
/// committed data as missing — a flake that reads like a broken checkout.
pub(crate) fn require_vanilla_data() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if REQUIRED_DATA.iter().any(|p| !root.join(p).exists()) {
        panic!("{}", crate::worldgen::VANILLA_DATA_MISSING);
    }
    // Present is not enough: a `datapacks/vanilla/` copied from an older pack uses another
    // schema (`material_rule`, `aquifers`, no `surface_rule`), loads without an error and
    // then shifts every value the frozen baselines pin, which reads as a code regression.
    let current_schema = std::fs::read_to_string(
        root.join("datapacks/vanilla/data/minecraft/worldgen/noise_settings/overworld.json"),
    )
    .is_ok_and(|s| s.contains("\"surface_rule\""));
    if !current_schema {
        panic!(
            "Stale vanilla data: {} ",
            crate::worldgen::VANILLA_DATA_MISSING
        );
    }
}

/// The one guard every test that touches process-global state must hold: env vars, the
/// working directory, the live world overlay, `TICK`/`TICK_IDX`, the client and mob
/// registries, the delta-WAL arm, connection counters, and every fixture that snapshots
/// one of them. It is the same mutex `world_guard()` and `WORLD_TEST_LOCK` already
/// serialize on, so a fixture added here cannot run beside one added there.
///
/// Poison-tolerant like the lock it wraps: a test that fails while holding it must fail
/// with its own reason, not turn every later test into a `PoisonError`.
pub(crate) fn global_world_test_guard() -> std::sync::MutexGuard<'static, ()> {
    crate::WORLD_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
    std::env::temp_dir().join(format!("megu-receipt-{}-{tag}", std::process::id()))
}

/// True when this process *is* the isolated child for the test it is running. It writes the
/// receipt the parent checks, so "ran and panicked" and "never started" stay distinguishable.
pub(crate) fn is_isolated_child() -> bool {
    let name = harness_name();
    if std::env::var("MEGU_TEST_CHILD").as_deref() != Ok(name.as_str()) {
        return false;
    }
    if let Ok(path) = std::env::var("MEGU_TEST_RECEIPT") {
        let _ = std::fs::write(path, &name);
    }
    true
}

/// Add a line to the receipt. A fixture that destroys process state appends what it saw after
/// writing, so the parent can tell "the child wrote and then panicked" from "the child did not
/// run": both leave the parent's own scene untouched.
pub(crate) fn child_note(note: &str) {
    if let Ok(path) = std::env::var("MEGU_TEST_RECEIPT") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(file, "{note}");
        }
    }
}

/// `MEGU_*` variables that in-process tests mutate. A child spawned while such a
/// mutation is live inherits it — `MEGU_ONLINE_MODE=1` mid-flip makes the child's own
/// loopback login demand an encryption response — so the child starts from defaults.
/// Harness variables are not here: `Command::env` sets them after this list is applied.
const CHILD_ENV_SCRUB: &[&str] = &[
    "MEGU_ALLOW_CAVE_MAP",
    "MEGU_ALLOW_ENTITY_RADAR",
    "MEGU_ALLOW_PLAYER_ADMIN_COMMANDS",
    "MEGU_ANTI_XRAY",
    "MEGU_ANVIL_WORLD_DIR",
    "MEGU_AUTO_UNLOAD_TICKS",
    "MEGU_BACKUP_MAX_KEEP",
    "MEGU_CONFIG",
    "MEGU_DATAPACK_CACHE_DIR",
    "MEGU_ENABLE_COMMAND_BLOCK",
    "MEGU_FAIR_PLAY",
    "MEGU_MC_VERSION",
    "MEGU_MODS_DIR",
    "MEGU_ONLINE_MODE",
    "MEGU_RESOURCE_PACK_REQUIRED",
    "MEGU_STRICT_WORLDGEN",
    "MEGU_TARGET_VERSION",
    "MEGU_VELOCITY_FORWARDING",
    "MEGU_VELOCITY_SECRET",
    "MEGU_WAL_PATH",
];

/// Run exactly `name` in a child and return its status with the receipt it left. Bounded: a
/// fixture that hangs is a failure of that fixture, not a hung suite. A caller whose fixture is
/// *expected* to fail decides what the status means; the receipt and the bound are not weaker for it.
pub(crate) fn run_isolated_child(name: &str) -> (std::process::ExitStatus, String) {
    let receipt = receipt_path(name);
    let _ = std::fs::remove_file(&receipt);
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env("MEGU_TEST_CHILD", name)
        .env("MEGU_TEST_RECEIPT", &receipt);
    for var in CHILD_ENV_SCRUB {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().expect("start isolated world fixture");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for isolated fixture") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated world fixture {name} exceeded 180 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let body = std::fs::read_to_string(&receipt).unwrap_or_default();
    let _ = std::fs::remove_file(&receipt);
    (status, body)
}

pub(crate) fn isolated_world_test() -> bool {
    if is_isolated_child() {
        return false;
    }
    let name = harness_name();
    let (status, body) = run_isolated_child(&name);
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
