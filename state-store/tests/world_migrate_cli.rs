// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for the `mc-spine-world-migrate` binary: real process
//! invocations against real files, asserting stdout content and exit codes
//! (`0` success, `2` refused downgrade / bad args, `1` file errors).

use mc_spine_core::version::TargetVersion;
use mc_spine_state_store::{
    decode_snapshot, encode_data_value_to_nbt, encode_snapshot, gzip_encode_stored,
    inspect_level_dat, parse_data_json, BlockCoord, ChunkCoord, DataValue, ServerContext,
};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mc-spine-world-migrate"))
}

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-world-migrate-cli-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to launch mc-spine-world-migrate")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A two-dimension context exercising every counted record kind.
fn fixture_context() -> ServerContext {
    let ctx = ServerContext::new();
    let overworld = ctx.ensure_dimension("minecraft:overworld");
    overworld.set_block(BlockCoord::new(8, 64, -8), 42);
    overworld.set_block(BlockCoord::new(9, 64, -8), 43);
    overworld.set_block(BlockCoord::new(-33, 200, 8), 7);
    overworld.with_chunks_write(&[ChunkCoord::new(0, -1)], |set| {
        set.get_mut(ChunkCoord::new(0, -1))
            .unwrap()
            .set_block_entity((8, 64, 8), b"{id:\"minecraft:chest\"}".to_vec());
    });
    let nether = ctx.ensure_dimension("minecraft:the_nether");
    nether.set_block(BlockCoord::new(0, 32, 0), 5);
    nether.set_block(BlockCoord::new(0, 33, 0), 5);
    ctx
}

fn stv1(dir: &TestDir, name: &str, target: TargetVersion) -> PathBuf {
    dir.write(name, &encode_snapshot(&fixture_context(), target))
}

const LEVEL_DAT_3955: &str = r#"{
    "DataVersion": 3955,
    "Data": {
        "DataVersion": 3955,
        "Version": {"Id": 3955, "Name": "1.21.1", "Snapshot": false}
    },
    "inventory": [
        {"id": "minecraft:boat", "count": 1,
         "components": {"minecraft:enchantments": {"minecraft:sharpness": 5}}}
    ]
}"#;

#[test]
fn help_and_usage_errors() {
    for args in [&["--help"][..], &["-h"][..], &["help"][..]] {
        let out = run(args);
        assert_eq!(code(&out), 0, "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).contains("USAGE"), "{args:?}");
    }
    for args in [
        &[][..],
        &["bogus"][..],
        &["inspect"][..],
        &["inspect", "a", "b"][..],
        &["migrate", "--in", "a"][..],
        &["migrate", "--in", "a", "--out", "b"][..],
        &["migrate", "--in", "a", "--out", "b", "--to", "1.21"][..],
        &["migrate-json", "--in", "a", "--out", "b", "--to", "26.3", "--wat"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}

#[test]
fn inspect_text_and_json() {
    let dir = TestDir::new("inspect");
    let file = stv1(&dir, "world.stv1", TargetVersion::V1_21_1);
    let file_str = file.to_str().unwrap().to_string();

    let out = run(&["inspect", &file_str]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("minecraft 1.21.1 snapshot"), "{text}");
    assert!(text.contains("protocol 767, data version 3955"), "{text}");
    assert!(text.contains("2 dimensions, 3 chunks, 5 block overrides, 1 block entity"), "{text}");
    assert!(text.contains("minecraft:the_nether"), "{text}");

    let out = run(&["inspect", &file_str, "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["mc_version"], DataValue::Str("1.21.1".to_string()));
    assert_eq!(m["protocol_version"], DataValue::Int(767));
    assert_eq!(m["data_version"], DataValue::Int(3955));
    assert_eq!(m["dimension_count"], DataValue::Int(2));
    assert_eq!(m["chunk_count"], DataValue::Int(3));
    assert_eq!(m["block_override_count"], DataValue::Int(5));
    assert_eq!(m["block_entity_count"], DataValue::Int(1));
    assert_eq!(
        m["dimensions"],
        DataValue::List(vec![
            DataValue::Str("minecraft:overworld".to_string()),
            DataValue::Str("minecraft:the_nether".to_string()),
        ])
    );
}

#[test]
fn inspect_corrupt_file_and_missing_file_exit_1() {
    let dir = TestDir::new("inspect-bad");
    // One flipped body byte: the CRC32 seal refuses it.
    let mut bytes = encode_snapshot(&fixture_context(), TargetVersion::V1_21_1);
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    let bad = dir.write("corrupt.stv1", &bytes);
    let out = run(&["inspect", bad.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(stderr(&out).contains("CRC32"), "{}", stderr(&out));

    let out = run(&["inspect", dir.path("missing.stv1").to_str().unwrap()]);
    assert_eq!(code(&out), 1);
}

#[test]
fn migrate_upgrade_and_idempotent() {
    let dir = TestDir::new("migrate");
    let input = stv1(&dir, "in.stv1", TargetVersion::V1_21_1);
    let output = dir.path("out.stv1");

    let out = run(&[
        "migrate",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("1.21.1 -> 26.3"), "{text}");
    assert!(text.contains("3955 -> 5023"), "{text}");

    // The written file is a sealed 26.3 snapshot with the same content.
    let written = std::fs::read(&output).unwrap();
    let (ctx, target) = decode_snapshot(&written).unwrap();
    assert_eq!(target, TargetVersion::V26_3);
    assert_eq!(ctx.dimension_names(), fixture_context().dimension_names());

    // Idempotent: 26.3 -> 26.3 reports already_at_target and byte-identical output.
    let out = run(&[
        "migrate",
        "--in",
        output.to_str().unwrap(),
        "--out",
        dir.path("out2.stv1").to_str().unwrap(),
        "--to",
        "26.3",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["already_at_target"], DataValue::Bool(true));
    assert_eq!(m["from_version"], DataValue::Str("26.3".to_string()));
    assert_eq!(m["data_version"], DataValue::Int(5023));
    assert_eq!(
        std::fs::read(dir.path("out2.stv1")).unwrap(),
        written,
        "same-target migration must reproduce the canonical bytes"
    );
}

#[test]
fn migrate_downgrade_refused_then_allowed() {
    let dir = TestDir::new("migrate-down");
    let input = stv1(&dir, "new.stv1", TargetVersion::V26_3);
    let output = dir.path("old.stv1");

    // Without the flag: refused with exit 2 and no output file created.
    let out = run(&[
        "migrate",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "1.21.1",
    ]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert!(stderr(&out).contains("--allow-downgrade"), "{}", stderr(&out));
    assert!(!output.exists(), "refused migration must not write output");

    // With the flag: sealed 1.21.1 snapshot comes out.
    let out = run(&[
        "migrate",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "1.21.1",
        "--allow-downgrade",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let (_, target) = decode_snapshot(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(target, TargetVersion::V1_21_1);
}

#[test]
fn migrate_corrupt_input_exits_1() {
    let dir = TestDir::new("migrate-bad");
    let mut bytes = encode_snapshot(&fixture_context(), TargetVersion::V26_3);
    bytes[7] ^= 0x01; // inside the header — CRC catches it
    let input = dir.write("corrupt.stv1", &bytes);
    let output = dir.path("out.stv1");
    let out = run(&[
        "migrate",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "1.21.1",
        "--allow-downgrade",
    ]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(!output.exists(), "corrupt input must never produce output");
}

#[test]
fn migrate_json_upgrade_downgrade_and_reports() {
    let dir = TestDir::new("migrate-json");
    let input = dir.write("level.json", LEVEL_DAT_3955.as_bytes());
    let output = dir.path("level-26.3.json");

    let out = run(&[
        "migrate-json",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("3955 -> 5023"), "{}", stdout(&out));

    // The output is deterministic sorted-key JSON at data version 5023.
    let doc = parse_data_json(&std::fs::read_to_string(&output).unwrap()).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["DataVersion"], DataValue::Int(5023));
    assert_eq!(
        m["Data"].as_map().unwrap()["Version"].as_map().unwrap()["Name"],
        DataValue::Str("26.3".to_string())
    );
    let item = match &m["inventory"] {
        DataValue::List(v) => v[0].as_map().unwrap(),
        _ => panic!(),
    };
    assert_eq!(item["id"], DataValue::Str("minecraft:oak_boat".to_string()));

    // Downgrade without opt-in: exit 2.
    let back = dir.path("level-back.json");
    let out = run(&[
        "migrate-json",
        "--in",
        output.to_str().unwrap(),
        "--out",
        back.to_str().unwrap(),
        "--to",
        "1.21.1",
    ]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert!(!back.exists());

    // With opt-in: 3955 again, and the boat id is restored.
    let out = run(&[
        "migrate-json",
        "--in",
        output.to_str().unwrap(),
        "--out",
        back.to_str().unwrap(),
        "--to",
        "1.21.1",
        "--allow-downgrade",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let report = parse_data_json(&stdout(&out)).unwrap();
    let rm = report.as_map().unwrap();
    assert_eq!(rm["final_data_version"], DataValue::Int(3955));
    assert!(matches!(rm["applied_steps"], DataValue::List(_)));
    let doc = parse_data_json(&std::fs::read_to_string(&back).unwrap()).unwrap();
    assert_eq!(
        doc.as_map().unwrap()["DataVersion"],
        DataValue::Int(3955)
    );
}

// ---------------------------------------------------------- inspect-anvil

fn dv_map(pairs: Vec<(&str, DataValue)>) -> DataValue {
    DataValue::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<std::collections::BTreeMap<_, _>>(),
    )
}

/// The smallest `level.dat` document that still looks like a real save.
fn level_dat_nbt(data_version: i64, level_name: &str, seed: i64) -> Vec<u8> {
    encode_data_value_to_nbt(
        "",
        &dv_map(vec![(
            "Data",
            dv_map(vec![
                ("DataVersion", DataValue::Int(data_version)),
                ("LevelName", DataValue::Str(level_name.to_string())),
                ("RandomSeed", DataValue::Int(seed)),
                (
                    "Version",
                    dv_map(vec![
                        ("Id", DataValue::Int(data_version)),
                        (
                            "Name",
                            DataValue::Str(if data_version == 3955 {
                                "1.21.1"
                            } else {
                                "26.3"
                            }
                            .to_string()),
                        ),
                    ]),
                ),
                (
                    "DataPacks",
                    dv_map(vec![(
                        "Enabled",
                        DataValue::List(vec![DataValue::Str("vanilla".to_string())]),
                    )]),
                ),
            ]),
        )]),
    )
    .unwrap()
}

/// A region file with one populated chunk per (slot, compression) pair.
fn mca_bytes(chunks: &[(usize, u8)]) -> Vec<u8> {
    let mut bytes = vec![0u8; 8192];
    for (i, &(slot, compression)) in chunks.iter().enumerate() {
        let offset = 2 + i as u32; // one sector per chunk, packed from 2
        let sectors = 1u8;
        bytes[slot * 4] = ((offset >> 16) & 0xff) as u8;
        bytes[slot * 4 + 1] = ((offset >> 8) & 0xff) as u8;
        bytes[slot * 4 + 2] = (offset & 0xff) as u8;
        bytes[slot * 4 + 3] = sectors;
        let start = offset as usize * 4096;
        let end = start + sectors as usize * 4096;
        bytes.resize(bytes.len().max(end), 0);
        bytes[start..start + 4].copy_from_slice(&20u32.to_be_bytes());
        bytes[start + 4] = compression;
    }
    bytes
}

/// A synthetic Anvil world: gzip `level.dat` at `data_version`, populated
/// chunks in all three dimension region dirs.
fn anvil_world(dir: &TestDir, data_version: i64) -> PathBuf {
    let world = dir.path("world");
    std::fs::create_dir_all(&world).unwrap();
    std::fs::write(
        world.join("level.dat"),
        gzip_encode_stored(&level_dat_nbt(data_version, "CLI World", 99)),
    )
    .unwrap();
    for (rel, file, chunks) in [
        ("region", "r.0.0.mca", &[(0usize, 2u8), (5, 1)][..]),
        ("region", "r.-1.2.mca", &[(1usize, 1u8)][..]),
        ("DIM-1/region", "r.0.0.mca", &[(0usize, 2u8)][..]),
        ("DIM1/region", "r.0.0.mca", &[(0usize, 3u8), (9, 4)][..]),
    ] {
        let rdir = world.join(rel);
        std::fs::create_dir_all(&rdir).unwrap();
        std::fs::write(rdir.join(file), mca_bytes(chunks)).unwrap();
    }
    world
}

#[test]
fn inspect_anvil_text_and_json() {
    let dir = TestDir::new("anvil");
    let world = anvil_world(&dir, 3955);
    let world_str = world.to_str().unwrap().to_string();

    let out = run(&["inspect-anvil", &world_str]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("minecraft 1.21.1"), "{text}");
    assert!(text.contains("\"CLI World\""), "{text}");
    assert!(text.contains("data version 3955"), "{text}");
    assert!(text.contains("seed 99"), "{text}");
    assert!(text.contains("4 region file(s), 6 populated chunk(s)"), "{text}");
    assert!(text.contains("nether"), "{text}");

    let out = run(&["inspect-anvil", &world_str, "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["data_version"], DataValue::Int(3955));
    assert_eq!(m["detected_target"], DataValue::Str("1.21.1".to_string()));
    assert_eq!(m["compatibility"], DataValue::Str("supported".to_string()));
    assert_eq!(m["level_name"], DataValue::Str("CLI World".to_string()));
    assert_eq!(m["populated_chunks"], DataValue::Int(6));
    assert_eq!(m["region_file_count"], DataValue::Int(4));
    let dims = match &m["dimensions"] {
        DataValue::List(v) => v.clone(),
        _ => panic!("dimensions must be a list"),
    };
    assert_eq!(dims.len(), 3);
    assert_eq!(
        dims[0].as_map().unwrap()["dimension"],
        DataValue::Str("overworld".to_string())
    );
    assert_eq!(dims[0].as_map().unwrap()["populated_chunks"], DataValue::Int(3));
    assert_eq!(dims[2].as_map().unwrap()["populated_chunks"], DataValue::Int(2));
    let ld = m["level_dat"].as_map().unwrap();
    assert_eq!(ld["compression"], DataValue::Str("gzip".to_string()));
    assert_eq!(ld["random_seed"], DataValue::Int(99));
}

#[test]
fn inspect_anvil_raw_level_dat_and_unsupported_version() {
    let dir = TestDir::new("anvil-raw");
    let world = dir.path("world");
    std::fs::create_dir_all(world.join("region")).unwrap();
    // Raw (uncompressed) NBT level.dat at 26.3's DataVersion.
    std::fs::write(
        world.join("level.dat"),
        level_dat_nbt(5023, "Raw World", -7),
    )
    .unwrap();
    std::fs::write(world.join("region/r.0.0.mca"), mca_bytes(&[(0, 2)])).unwrap();

    let out = run(&["inspect-anvil", world.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("minecraft 26.3"), "{text}");
    assert!(text.contains("uncompressed data"), "{text}");

    // A DataVersion between the targets still inspects fine — the world is
    // valid, it is just not a version this build serves.
    let world2 = anvil_world(&dir, 4500);
    let out = run(&["inspect-anvil", world2.to_str().unwrap(), "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(
        m["compatibility"],
        DataValue::Str("between_targets".to_string())
    );
    assert_eq!(m["detected_target"], DataValue::Null);
}

#[test]
fn inspect_anvil_fail_closed_and_arg_errors() {
    let dir = TestDir::new("anvil-bad");

    // Missing directory / missing level.dat / corrupt level.dat all exit 1.
    let out = run(&["inspect-anvil", dir.path("nope").to_str().unwrap()]);
    assert_eq!(code(&out), 1);

    let world = dir.path("world");
    std::fs::create_dir_all(&world).unwrap();
    let out = run(&["inspect-anvil", world.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(stderr(&out).contains("level.dat"), "{}", stderr(&out));

    std::fs::write(world.join("level.dat"), b"definitely not nbt").unwrap();
    let out = run(&["inspect-anvil", world.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));

    // Truncated .mca inside an otherwise valid world still exits 1.
    let world = anvil_world(&dir, 3955);
    std::fs::write(world.join("region/r.9.9.mca"), vec![0u8; 512]).unwrap();
    let out = run(&["inspect-anvil", world.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(stderr(&out).contains("r.9.9.mca"), "{}", stderr(&out));

    // Bad command lines exit 2.
    for args in [
        &["inspect-anvil"][..],
        &["inspect-anvil", "a", "b"][..],
        &["inspect-anvil", "a", "--wat"][..],
        &["inspect-anvil", "--json"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}

#[test]
fn migrate_json_fail_closed_on_malformed_input() {
    let dir = TestDir::new("migrate-json-bad");
    let input = dir.write("bad.json", b"{\"DataVersion\": 3955, oops");
    let out = run(&[
        "migrate-json",
        "--in",
        input.to_str().unwrap(),
        "--out",
        dir.path("out.json").to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(!dir.path("out.json").exists());

    // Valid JSON but a DataVersion no pipeline covers.
    let input = dir.write("newer.json", b"{\"DataVersion\": 6000}");
    let out = run(&[
        "migrate-json",
        "--in",
        input.to_str().unwrap(),
        "--out",
        dir.path("out.json").to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
}

// ---------------------------------------------------------- migrate-level

/// A gzip `level.dat` at `data_version` written to `dir/name`.
fn level_dat_file(dir: &TestDir, name: &str, data_version: i64) -> PathBuf {
    dir.write(
        name,
        &gzip_encode_stored(&level_dat_nbt(data_version, "Level", 7)),
    )
}

#[test]
fn migrate_level_upgrade_backup_old_and_json() {
    let dir = TestDir::new("level-up");
    let input = level_dat_file(&dir, "level.dat", 3955);
    let output = dir.path("out.dat");
    // A pre-existing output the backup must preserve byte-for-byte.
    let prior = gzip_encode_stored(&level_dat_nbt(3955, "Prior", 1));
    std::fs::write(&output, &prior).unwrap();

    let out = run(&[
        "migrate-level",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "26.3",
        "--backup-old",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["original_data_version"], DataValue::Int(3955));
    assert_eq!(m["final_data_version"], DataValue::Int(5023));
    assert_eq!(m["target_version"], DataValue::Str("26.3".to_string()));
    assert_eq!(m["already_at_target"], DataValue::Bool(false));
    assert_eq!(
        m["backup_file"],
        DataValue::Str(format!("{}_old", output.display()))
    );
    assert!(matches!(m["applied_steps"], DataValue::List(_)));
    assert_eq!(m["warnings"], DataValue::List(vec![]));

    // The backup is an exact copy of the previous output — the vanilla
    // level.dat_old convention — and the swapped-in output is a verified
    // gzip level.dat at 5023 with the world's metadata intact.
    let backup = dir.path("out.dat_old");
    assert_eq!(std::fs::read(&backup).unwrap(), prior);
    let written = std::fs::read(&output).unwrap();
    assert_ne!(written, prior);
    let s = inspect_level_dat(&written).unwrap();
    assert_eq!(s.compression.label(), "gzip");
    assert_eq!(s.data_version, 5023);
    assert_eq!(s.version_name.as_deref(), Some("26.3"));
    assert_eq!(s.level_name.as_deref(), Some("Level"));
    assert_eq!(s.random_seed, Some(7));
}

#[test]
fn migrate_level_refused_downgrade_touches_nothing() {
    let dir = TestDir::new("level-down-ref");
    let input = level_dat_file(&dir, "new.dat", 5023);
    let output = dir.path("level.dat");
    let prior = gzip_encode_stored(&level_dat_nbt(5023, "Keep", 2));
    std::fs::write(&output, &prior).unwrap();

    // Refused without the flag: exit 2, output untouched, no backup taken.
    let out = run(&[
        "migrate-level",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "1.21.1",
        "--backup-old",
    ]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert!(stderr(&out).contains("--allow-downgrade"), "{}", stderr(&out));
    assert_eq!(std::fs::read(&output).unwrap(), prior);
    assert!(!dir.path("level.dat_old").exists());

    // With the flag the downgrade lands: gzip level.dat back at 3955.
    let out = run(&[
        "migrate-level",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "1.21.1",
        "--allow-downgrade",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let s = inspect_level_dat(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(s.data_version, 3955);
    assert_eq!(s.version_name.as_deref(), Some("1.21.1"));
}

#[test]
fn migrate_level_corrupt_input_exits_1() {
    let dir = TestDir::new("level-bad");
    let mut bad = gzip_encode_stored(&level_dat_nbt(3955, "W", 0));
    let n = bad.len();
    bad[n - 5] ^= 0xff; // corrupt the CRC32 trailer
    let input = dir.write("corrupt.dat", &bad);
    let output = dir.path("out.dat");
    let out = run(&[
        "migrate-level",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(!output.exists(), "corrupt input must never produce output");

    // A missing input file is also exit 1, not a usage error.
    let out = run(&[
        "migrate-level",
        "--in",
        dir.path("missing.dat").to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 1);
    assert!(!output.exists());
}

#[test]
fn migrate_level_already_at_target_and_arg_errors() {
    let dir = TestDir::new("level-noop");
    let input = level_dat_file(&dir, "done.dat", 5023);
    let output = dir.path("out.dat");
    let out = run(&[
        "migrate-level",
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--to",
        "26.3",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("already at 26.3"), "{}", stdout(&out));
    let s = inspect_level_dat(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(s.data_version, 5023);
    // No --backup-old flag means no _old file is ever created.
    assert!(!dir.path("out.dat_old").exists());

    for args in [
        &["migrate-level"][..],
        &["migrate-level", "--in", "a"][..],
        &["migrate-level", "--in", "a", "--out", "b"][..],
        &["migrate-level", "--in", "a", "--out", "b", "--to", "1.21"][..],
        &["migrate-level", "--in", "a", "--out", "b", "--to", "26.3", "--wat"][..],
        // --backup-old does not belong to the other migrate subcommands.
        &["migrate", "--in", "a", "--out", "b", "--to", "26.3", "--backup-old"][..],
        &["migrate-json", "--in", "a", "--out", "b", "--to", "26.3", "--backup-old"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}
