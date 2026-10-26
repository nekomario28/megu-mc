//! Persistence tests run in a fresh process: a loader touches many global stores and
//! derived indexes, so saving only a subset of them is not a reversible fixture.

/// The files `tools/vanilla_data/gen_vanilla_data.sh` writes. Nothing in the tree
/// carries them, so a fresh clone has none of them until the generator has run.
const GENERATED: &[&str] = &[
    "assets/block_states_1211.json",
    "datapacks/vanilla/data/minecraft/worldgen/noise_settings/overworld.json",
    "datapacks/vanilla/biome_climate_tree.json",
    "tests/data/biome_probe_1211.tsv",
    "tests/data/vanilla_vectors_1211.tsv",
];

/// Fail with the generator's own command unless the generated vanilla data is there.
///
/// Every test that reads that data calls this first, before it takes a global lock: a
/// panic while holding one poisons it, and the next test then fails with `PoisonError`
/// instead of the reason, which reads as dozens of unrelated failures.
pub(crate) fn require_vanilla_data() {
    if GENERATED.iter().any(|p| !std::path::Path::new(p).exists()) {
        panic!("{}", crate::worldgen::VANILLA_DATA_MISSING);
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
