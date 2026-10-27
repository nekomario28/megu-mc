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
