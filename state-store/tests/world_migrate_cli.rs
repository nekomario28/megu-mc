// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for the `mc-spine-world-migrate` binary: real process
//! invocations against real files, asserting stdout content and exit codes
//! (`0` success, `2` refused downgrade / bad args, `1` file errors).

use mc_spine_core::version::TargetVersion;
use mc_spine_state_store::{
    decode_snapshot, encode_data_value_to_nbt, encode_snapshot, gzip_encode_stored,
    inspect_level_dat, parse_data_json, spiral_order, AuditAction, AuditLog, AuditRecord,
    BlockCoord, ChunkCoord, DataValue, PregenShape, ServerContext,
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
        &[
            "migrate-level",
            "--in",
            "a",
            "--out",
            "b",
            "--to",
            "26.3",
            "--wat",
        ][..],
        // --backup-old does not belong to the other migrate subcommands.
        &[
            "migrate",
            "--in",
            "a",
            "--out",
            "b",
            "--to",
            "26.3",
            "--backup-old",
        ][..],
        &[
            "migrate-json",
            "--in",
            "a",
            "--out",
            "b",
            "--to",
            "26.3",
            "--backup-old",
        ][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}

// ---------------------------------------------------------- audit-query

const OVERWORLD: &str = "minecraft:overworld";
const NETHER: &str = "minecraft:the_nether";

/// A real AUD1 WAL on disk: four records over two dimensions and four
/// actors, with a spread of ticks and positions for the filters to bite on.
fn audit_log_file(dir: &TestDir, name: &str) -> PathBuf {
    let path = dir.path(name);
    let mut log = AuditLog::new();
    let mut took = AuditRecord::new(
        11,
        "Bob",
        OVERWORLD,
        [-16, 70, 3],
        AuditAction::ContainerTake,
        5,
        5,
    );
    took.item_id = Some("minecraft:diamond".to_string());
    took.item_count = 3;
    for r in [
        AuditRecord::new(
            10,
            "Alice",
            OVERWORLD,
            [8, 64, 8],
            AuditAction::BlockPlace,
            0,
            42,
        ),
        took,
        AuditRecord::new(
            12,
            "Griefer",
            OVERWORLD,
            [4, 64, 8],
            AuditAction::BlockBreak,
            7,
            0,
        ),
        AuditRecord::new(
            30,
            "#tnt",
            NETHER,
            [200, 60, 200],
            AuditAction::Explosion,
            9,
            0,
        ),
    ] {
        log.append_to_file(&path, r).unwrap();
    }
    path
}

#[test]
fn audit_query_rows_filters_and_json() {
    let dir = TestDir::new("audit-query");
    let log = audit_log_file(&dir, "audit.aud1");
    let log_str = log.to_str().unwrap();

    // Full scan prints one greppable row per record, in seq order.
    let out = run(&["audit-query", "--log", log_str]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("4 audit record(s), 4 match"), "{text}");
    for needle in [
        "seq=1 tick=10 actor=Alice",
        "action=container_take",
        "item=minecraft:diamond x3",
        "action=explosion",
        "pos=(200,60,200)",
    ] {
        assert!(text.contains(needle), "{text} missing {needle:?}");
    }

    // --actor / --dim / --since-tick / --pos+--radius each cut the set.
    let out = run(&["audit-query", "--log", log_str, "--actor", "Griefer"]);
    let text = stdout(&out);
    assert!(text.contains("1 match"), "{text}");
    assert!(text.contains("actor=Griefer"), "{text}");
    assert!(!text.contains("actor=Alice"), "{text}");

    let out = run(&["audit-query", "--log", log_str, "--dim", NETHER]);
    assert!(stdout(&out).contains("1 match"), "{}", stdout(&out));

    // Ticks 12 and up: the Griefer break (12) and the TNT blast (30).
    let out = run(&["audit-query", "--log", log_str, "--since-tick", "12"]);
    assert!(stdout(&out).contains("2 match"), "{}", stdout(&out));

    // Radius 32 around the world origin keeps the three near records and
    // drops the nether explosion ~283 blocks out.
    let out = run(&[
        "audit-query",
        "--log",
        log_str,
        "--pos",
        "0,64,0",
        "--radius",
        "32",
    ]);
    let text = stdout(&out);
    assert!(text.contains("3 match"), "{text}");
    assert!(!text.contains("pos=(200,60,200)"), "{text}");

    // A negative radius is clamped to 0 by AuditQuery::within — only the
    // exact-position record survives, and the query still exits 0.
    let out = run(&[
        "audit-query",
        "--log",
        log_str,
        "--pos",
        "4,64,8",
        "--radius",
        "-1",
    ]);
    let text = stdout(&out);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(text.contains("1 match"), "{text}");
    assert!(text.contains("actor=Griefer"), "{text}");

    // An i32::MAX radius must answer instantly: the spatial index walks
    // occupied chunk buckets, not the coordinate range, and the squared
    // distance math is i128-safe at i32 extremes.
    let out = run(&[
        "audit-query",
        "--log",
        log_str,
        "--pos",
        "0,64,0",
        "--radius",
        "2147483647",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("4 match"), "{}", stdout(&out));

    // The opposite extreme: a huge-radius sphere around an i32-corner
    // center genuinely contains none of the records (their Euclidean
    // distance ~3e9 exceeds i32::MAX) — and still answers instantly.
    let out = run(&[
        "audit-query",
        "--log",
        log_str,
        "--pos",
        "2147483647,0,-2147483648",
        "--radius",
        "2147483647",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("0 match"), "{}", stdout(&out));

    // --limit caps the printed rows; the matched count still reflects the
    // full filter result.
    let out = run(&["audit-query", "--log", log_str, "--limit", "2"]);
    let text = stdout(&out);
    assert!(text.contains("4 match the filter (showing 2)"), "{text}");
    assert_eq!(text.matches("seq=").count(), 2, "{text}");

    // JSON: schema tag, recovery stats and fully-typed records.
    let out = run(&[
        "audit-query",
        "--log",
        log_str,
        "--actor",
        "Griefer",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(
        m["schema"],
        DataValue::Str("mc-spine-audit-query/1".to_string())
    );
    assert_eq!(m["total_records"], DataValue::Int(4));
    assert_eq!(m["truncated_tail_bytes"], DataValue::Int(0));
    assert_eq!(m["matched"], DataValue::Int(1));
    assert_eq!(m["returned"], DataValue::Int(1));
    let records = match &m["records"] {
        DataValue::List(v) => v.clone(),
        _ => panic!("records must be a list"),
    };
    assert_eq!(records.len(), 1);
    let r = records[0].as_map().unwrap();
    assert_eq!(r["seq"], DataValue::Int(3));
    assert_eq!(r["tick"], DataValue::Int(12));
    assert_eq!(r["actor"], DataValue::Str("Griefer".to_string()));
    assert_eq!(r["dimension"], DataValue::Str(OVERWORLD.to_string()));
    assert_eq!(r["action"], DataValue::Str("block_break".to_string()));
    assert_eq!(
        r["pos"],
        DataValue::List(vec![
            DataValue::Int(4),
            DataValue::Int(64),
            DataValue::Int(8)
        ])
    );
    assert_eq!(r["old_state"], DataValue::Int(7));
    assert_eq!(r["new_state"], DataValue::Int(0));
    assert_eq!(r["rolled_back"], DataValue::Bool(false));
}

#[test]
fn audit_query_recovers_torn_tail_and_bad_args() {
    let dir = TestDir::new("audit-tail");
    let log = audit_log_file(&dir, "audit.aud1");
    let log_str = log.to_str().unwrap().to_string();

    // A crash mid-frame: half a fifth record dangles off the committed log.
    let torn = AuditLog::encode_frame(&AuditRecord::new(
        31,
        "Alice",
        OVERWORLD,
        [0, 64, 0],
        AuditAction::BlockPlace,
        0,
        1,
    ));
    let torn = &torn[..torn.len() / 2];
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(torn).unwrap();
    }

    // Recovery keeps every intact frame, reports the dropped tail on stderr
    // and still exits 0 — the tail is data, not a fatal error.
    let out = run(&["audit-query", "--log", &log_str, "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stderr(&out).contains("trailing byte"), "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["total_records"], DataValue::Int(4));
    assert_eq!(m["truncated_tail_bytes"], DataValue::Int(torn.len() as i64));

    // A missing log file is a file error, not a usage error.
    let out = run(&[
        "audit-query",
        "--log",
        dir.path("missing.aud1").to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 1);

    for args in [
        &["audit-query"][..],
        &["audit-query", "--log", &log_str, "--pos", "0,64,0"][..],
        &["audit-query", "--log", &log_str, "--radius", "32"][..],
        &["audit-query", "--log", &log_str, "--pos", "0,64"][..],
        &["audit-query", "--log", &log_str, "--since-tick", "abc"][..],
        &["audit-query", "--log", &log_str, "--limit", "-1"][..],
        &["audit-query", "--log", &log_str, "--in", "x.stv1"][..],
        &["audit-query", "--log", &log_str, "--max-blocks", "5"][..],
        &["audit-query", "--log", &log_str, "--wat"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}

// ------------------------------------------------------------- rollback

/// The griefed world + its audit WAL: Alice's house (states 100/101/102)
/// was TNT'd and broken by Griefer at ticks 20/21 (records carry the
/// pre-grief `old_state`s), and Bob placed block 77 legitimately.
fn griefed_fixture(dir: &TestDir) -> (PathBuf, PathBuf) {
    let ctx = ServerContext::new();
    let dim = ctx.ensure_dimension(OVERWORLD);
    dim.set_block(BlockCoord::new(8, 64, 8), 999);
    dim.set_block(BlockCoord::new(9, 64, 8), 101);
    dim.set_block(BlockCoord::new(8, 64, 9), 0);
    dim.set_block(BlockCoord::new(9, 64, 9), 77);
    let input = dir.write("griefed.stv1", &encode_snapshot(&ctx, TargetVersion::V26_3));

    let log_path = dir.path("audit.aud1");
    let mut log = AuditLog::new();
    for r in [
        AuditRecord::new(
            10,
            "Alice",
            OVERWORLD,
            [8, 64, 8],
            AuditAction::BlockPlace,
            0,
            100,
        ),
        AuditRecord::new(
            10,
            "Alice",
            OVERWORLD,
            [9, 64, 8],
            AuditAction::BlockPlace,
            0,
            101,
        ),
        AuditRecord::new(
            10,
            "Alice",
            OVERWORLD,
            [8, 64, 9],
            AuditAction::BlockPlace,
            0,
            102,
        ),
        AuditRecord::new(
            20,
            "Griefer",
            OVERWORLD,
            [8, 64, 8],
            AuditAction::BlockPlace,
            100,
            999,
        ),
        AuditRecord::new(
            21,
            "Griefer",
            OVERWORLD,
            [8, 64, 9],
            AuditAction::BlockBreak,
            102,
            0,
        ),
        AuditRecord::new(
            22,
            "Bob",
            OVERWORLD,
            [9, 64, 9],
            AuditAction::BlockPlace,
            0,
            77,
        ),
    ] {
        log.append_to_file(&log_path, r).unwrap();
    }
    (input, log_path)
}

#[test]
fn rollback_restores_pre_grief_blocks_and_preserves_target() {
    let dir = TestDir::new("rollback");
    let (input, log_path) = griefed_fixture(&dir);
    let input_bytes = std::fs::read(&input).unwrap();
    let output = dir.path("restored.stv1");

    // Rolling back only Griefer's edits must restore Alice's originals while
    // Bob's edit — recorded in the same WAL — survives untouched.
    let out = run(&[
        "rollback",
        "--log",
        log_path.to_str().unwrap(),
        "--in",
        input.to_str().unwrap(),
        "--out",
        output.to_str().unwrap(),
        "--actor",
        "Griefer",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("6 audit record(s) recovered"), "{text}");
    assert!(text.contains("rolled back 2 block change(s)"), "{text}");

    // The restored STV1 decodes, keeps the input's exact target version and
    // holds the pre-grief block states at the griefed positions.
    let (restored, target) = decode_snapshot(&std::fs::read(&output).unwrap()).unwrap();
    assert_eq!(target, TargetVersion::V26_3);
    let rdim = restored.dimension(OVERWORLD).unwrap();
    assert_eq!(rdim.block(BlockCoord::new(8, 64, 8)), Some(100));
    assert_eq!(rdim.block(BlockCoord::new(8, 64, 9)), Some(102));
    assert_eq!(rdim.block(BlockCoord::new(9, 64, 8)), Some(101));
    assert_eq!(rdim.block(BlockCoord::new(9, 64, 9)), Some(77));

    // --in was read-only the whole time.
    assert_eq!(std::fs::read(&input).unwrap(), input_bytes);

    // The same rollback in --json mode reports the schema and counts.
    let output2 = dir.path("restored2.stv1");
    let out = run(&[
        "rollback",
        "--log",
        log_path.to_str().unwrap(),
        "--in",
        input.to_str().unwrap(),
        "--out",
        output2.to_str().unwrap(),
        "--actor",
        "Griefer",
        "--since-tick",
        "15",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(
        m["schema"],
        DataValue::Str("mc-spine-rollback/1".to_string())
    );
    assert_eq!(m["rolled_back_blocks"], DataValue::Int(2));
    assert_eq!(m["target_version"], DataValue::Str("26.3".to_string()));
    assert_eq!(m["total_records"], DataValue::Int(6));
    assert_eq!(m["truncated_tail_bytes"], DataValue::Int(0));
    assert_eq!(m["max_blocks"], DataValue::Int(10_000));
    let (restored2, _) = decode_snapshot(&std::fs::read(&output2).unwrap()).unwrap();
    let rdim2 = restored2.dimension(OVERWORLD).unwrap();
    assert_eq!(rdim2.block(BlockCoord::new(8, 64, 8)), Some(100));
    assert_eq!(rdim2.block(BlockCoord::new(8, 64, 9)), Some(102));
}

#[test]
fn rollback_over_budget_and_bad_args_leave_output_untouched() {
    let dir = TestDir::new("rollback-refuse");
    let (input, log_path) = griefed_fixture(&dir);
    let output = dir.path("restored.stv1");
    std::fs::write(&output, b"sentinel").unwrap();
    let (log_str, in_str, out_str) = (
        log_path.to_str().unwrap().to_string(),
        input.to_str().unwrap().to_string(),
        output.to_str().unwrap().to_string(),
    );

    // Two Griefer mutations match, --max-blocks 1: hard refusal, exit 2, and
    // the pre-existing output keeps every one of its bytes.
    let out = run(&[
        "rollback",
        "--log",
        &log_str,
        "--in",
        &in_str,
        "--out",
        &out_str,
        "--actor",
        "Griefer",
        "--max-blocks",
        "1",
    ]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert!(stderr(&out).contains("--max-blocks"), "{}", stderr(&out));
    assert_eq!(std::fs::read(&output).unwrap(), b"sentinel");

    // The hard cap itself cannot be raised past 100_000.
    let out = run(&[
        "rollback",
        "--log",
        &log_str,
        "--in",
        &in_str,
        "--out",
        &out_str,
        "--max-blocks",
        "100001",
    ]);
    assert_eq!(code(&out), 2, "{}", stdout(&out));
    assert_eq!(std::fs::read(&output).unwrap(), b"sentinel");

    // A corrupt input snapshot is a file error — and still writes nothing.
    let mut corrupt = std::fs::read(&input).unwrap();
    corrupt[7] ^= 0x01;
    let bad = dir.write("corrupt.stv1", &corrupt);
    let out = run(&[
        "rollback",
        "--log",
        &log_str,
        "--in",
        bad.to_str().unwrap(),
        "--out",
        &out_str,
    ]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert_eq!(std::fs::read(&output).unwrap(), b"sentinel");

    // A missing audit log is exit 1, not a usage error.
    let out = run(&[
        "rollback",
        "--log",
        dir.path("missing.aud1").to_str().unwrap(),
        "--in",
        &in_str,
        "--out",
        &out_str,
    ]);
    assert_eq!(code(&out), 1);

    for args in [
        &["rollback"][..],
        &["rollback", "--log", &log_str][..],
        &["rollback", "--log", &log_str, "--in", &in_str][..],
        &[
            "rollback", "--log", &log_str, "--in", &in_str, "--out", &out_str, "--limit", "3",
        ][..],
        &[
            "rollback", "--log", &log_str, "--in", &in_str, "--out", &out_str, "--pos", "1,2",
        ][..],
        &[
            "rollback",
            "--log",
            &log_str,
            "--in",
            &in_str,
            "--out",
            &out_str,
            "--since-tick",
            "x",
        ][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}

// ----------------------------------------------------------- pregen-plan

fn chunk_json(cx: i32, cz: i32) -> DataValue {
    DataValue::List(vec![DataValue::Int(cx as i64), DataValue::Int(cz as i64)])
}

#[test]
fn pregen_plan_square_circle_ticks_and_bad_args() {
    // Square r=5: the human report carries the exact Chunky-style numbers.
    let out = run(&["pregen-plan", "--radius", "5"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("square radius 5"), "{text}");
    assert!(text.contains("121 chunk(s) over 6 ring(s)"), "{text}");
    assert!(text.contains("first chunk (0, 0)"), "{text}");
    assert!(text.contains("last chunk (-5, -4)"), "{text}");

    // Same plan as JSON — first/last cross-checked against the reference
    // spiral the library itself enumerates.
    let out = run(&["pregen-plan", "--radius", "5", "--json"]);
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(
        m["schema"],
        DataValue::Str("mc-spine-pregen-plan/1".to_string())
    );
    assert_eq!(m["shape"], DataValue::Str("square".to_string()));
    assert_eq!(m["total_chunks"], DataValue::Int(121));
    assert_eq!(m["ring_count"], DataValue::Int(6));
    assert_eq!(m["radius_chunks"], DataValue::Int(5));
    let spiral = spiral_order(0, 0, 5, PregenShape::Square);
    assert_eq!(m["first_chunk"], chunk_json(spiral[0].0, spiral[0].1));
    let last = spiral.last().unwrap();
    assert_eq!(m["last_chunk"], chunk_json(last.0, last.1));
    assert_eq!(m["batch_size"], DataValue::Int(64));
    assert_eq!(m["ticks"], DataValue::Int(2));

    // Circle r=5: dx^2+dz^2 <= 25 covers exactly 81 chunks.
    let out = run(&[
        "pregen-plan",
        "--radius",
        "5",
        "--shape",
        "circle",
        "--json",
    ]);
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["shape"], DataValue::Str("circle".to_string()));
    assert_eq!(m["total_chunks"], DataValue::Int(81));
    let spiral = spiral_order(0, 0, 5, PregenShape::Circle);
    assert_eq!(spiral.len(), 81);
    assert_eq!(m["first_chunk"], chunk_json(spiral[0].0, spiral[0].1));
    let last = spiral.last().unwrap();
    assert_eq!(m["last_chunk"], chunk_json(last.0, last.1));

    // --center shifts every reported coordinate; --batch-size drives the
    // tick estimate by ceiling division (121 / 7 -> 18).
    let out = run(&[
        "pregen-plan",
        "--radius",
        "5",
        "--center",
        "3,-2",
        "--batch-size",
        "7",
        "--json",
    ]);
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["center"], chunk_json(3, -2));
    let spiral = spiral_order(3, -2, 5, PregenShape::Square);
    assert_eq!(m["first_chunk"], chunk_json(spiral[0].0, spiral[0].1));
    let last = spiral.last().unwrap();
    assert_eq!(m["last_chunk"], chunk_json(last.0, last.1));
    assert_eq!(m["ticks"], DataValue::Int(18));

    // Radius 0 plans a single chunk — the center itself.
    let out = run(&["pregen-plan", "--radius", "0", "--json"]);
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["total_chunks"], DataValue::Int(1));
    assert_eq!(m["ring_count"], DataValue::Int(1));
    assert_eq!(m["first_chunk"], chunk_json(0, 0));
    assert_eq!(m["last_chunk"], chunk_json(0, 0));
    assert_eq!(m["ticks"], DataValue::Int(1));

    for args in [
        &["pregen-plan"][..],
        &["pregen-plan", "--shape", "square"][..],
        &["pregen-plan", "--radius", "x"][..],
        &["pregen-plan", "--radius", "-1"][..],
        &["pregen-plan", "--radius", "5", "--shape", "hex"][..],
        &["pregen-plan", "--radius", "5", "--center", "1,2,3"][..],
        &["pregen-plan", "--radius", "5", "--center", "a,b"][..],
        &["pregen-plan", "--radius", "5", "--batch-size", "0"][..],
        &["pregen-plan", "--radius", "2000001"][..],
        &["pregen-plan", "--radius", "5", "extra"][..],
        &["pregen-plan", "--radius", "5", "--log", "x"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
    }
}

// ------------------------------------------------------------ pregen-run

#[test]
fn pregen_run_square_and_circle_execute_the_spiral() {
    // Square r=5, budget 10, cap 8: the whole 121-chunk square is generated
    // over 13 ticks while the unloader holds residency at 8.
    let out = run(&[
        "pregen-run",
        "--radius",
        "5",
        "--budget-per-tick",
        "10",
        "--max-resident",
        "8",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("square radius 5"), "{text}");
    assert!(text.contains("121 chunk(s) generated"), "{text}");
    assert!(text.contains("13 tick(s)"), "{text}");
    assert!(text.contains("113 evicted"), "{text}");
    assert!(text.contains("8 still resident"), "{text}");

    // Same run as JSON — every counter deterministic and cross-checked
    // against the library's own spiral enumerator.
    let out = run(&[
        "pregen-run",
        "--radius",
        "5",
        "--budget-per-tick",
        "10",
        "--max-resident",
        "8",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(
        m["schema"],
        DataValue::Str("mc-spine-pregen-run/1".to_string())
    );
    assert_eq!(m["shape"], DataValue::Str("square".to_string()));
    assert_eq!(m["radius_chunks"], DataValue::Int(5));
    assert_eq!(m["budget_per_tick"], DataValue::Int(10));
    assert_eq!(m["max_resident"], DataValue::Int(8));
    assert_eq!(
        m["generated_chunks"],
        DataValue::Int(spiral_order(0, 0, 5, PregenShape::Square).len() as i64)
    );
    assert_eq!(m["generated_chunks"], DataValue::Int(121));
    assert_eq!(m["ticks_run"], DataValue::Int(13));
    assert_eq!(m["resident_chunks"], DataValue::Int(8));
    assert_eq!(m["evicted_chunks"], DataValue::Int(113));
    assert_eq!(m["state"], DataValue::Str("completed".to_string()));
    // Bounded RAM: the post-generation transient is at most cap + one
    // tick's budget before the unloader runs.
    let peak = match &m["peak_resident_chunks"] {
        DataValue::Int(n) => *n,
        other => panic!("peak_resident_chunks is not an int: {other:?}"),
    };
    assert!(peak <= 8 + 10, "peak {peak} exceeds cap + budget");

    // Circle r=5 covers exactly 81 chunks; the default 1024 cap evicts
    // nothing, so generated == resident.
    let out = run(&[
        "pregen-run",
        "--radius",
        "5",
        "--shape",
        "circle",
        "--budget-per-tick",
        "7",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["shape"], DataValue::Str("circle".to_string()));
    assert_eq!(m["generated_chunks"], DataValue::Int(81));
    assert_eq!(m["ticks_run"], DataValue::Int(12)); // ceil(81 / 7)
    assert_eq!(m["evicted_chunks"], DataValue::Int(0));
    assert_eq!(m["resident_chunks"], DataValue::Int(81));
    assert_eq!(m["peak_resident_chunks"], DataValue::Int(81));

    // --center only shifts coordinates; the counters are translation
    // invariant.
    let out = run(&[
        "pregen-run",
        "--radius",
        "2",
        "--center",
        "10,-4",
        "--budget-per-tick",
        "5",
        "--json",
    ]);
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["center"], chunk_json(10, -4));
    assert_eq!(m["generated_chunks"], DataValue::Int(25));
    assert_eq!(m["ticks_run"], DataValue::Int(5));
}

#[test]
fn pregen_run_eviction_cap_keeps_residency_bounded() {
    // A tiny cap over a 25-chunk run: evicted + resident must account for
    // every generated chunk, and the resident bound must never be exceeded
    // at a tick boundary.
    let out = run(&[
        "pregen-run",
        "--radius",
        "2",
        "--budget-per-tick",
        "4",
        "--max-resident",
        "3",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["generated_chunks"], DataValue::Int(25));
    assert_eq!(m["resident_chunks"], DataValue::Int(3));
    assert_eq!(m["evicted_chunks"], DataValue::Int(22));
    assert_eq!(m["peak_resident_chunks"], DataValue::Int(7)); // 3 + 4

    // --max-resident 0 keeps nothing resident: the run still generates all
    // 25 chunks but flushes every one.
    let out = run(&[
        "pregen-run",
        "--radius",
        "2",
        "--budget-per-tick",
        "4",
        "--max-resident",
        "0",
        "--json",
    ]);
    let doc = parse_data_json(&stdout(&out)).unwrap();
    let m = doc.as_map().unwrap();
    assert_eq!(m["generated_chunks"], DataValue::Int(25));
    assert_eq!(m["resident_chunks"], DataValue::Int(0));
    assert_eq!(m["evicted_chunks"], DataValue::Int(25));
    assert_eq!(m["peak_resident_chunks"], DataValue::Int(4));
}

#[test]
fn pregen_run_fails_closed_on_bad_args() {
    for args in [
        // Missing required flags.
        &["pregen-run"][..],
        &["pregen-run", "--radius", "5"][..],
        &["pregen-run", "--budget-per-tick", "4"][..],
        // Zero or unparseable budgets.
        &["pregen-run", "--radius", "5", "--budget-per-tick", "0"][..],
        &["pregen-run", "--radius", "5", "--budget-per-tick", "-1"][..],
        &["pregen-run", "--radius", "5", "--budget-per-tick", "x"][..],
        // Bad shape, bad radius, bad center.
        &["pregen-run", "--radius", "5", "--budget-per-tick", "4", "--shape", "hex"][..],
        &["pregen-run", "--radius", "-1", "--budget-per-tick", "4"][..],
        &["pregen-run", "--radius", "2000001", "--budget-per-tick", "4"][..],
        &["pregen-run", "--radius", "5", "--budget-per-tick", "4", "--center", "1,2,3"][..],
        // Foreign flags and stray arguments.
        &["pregen-run", "--radius", "5", "--budget-per-tick", "4", "--batch-size", "4"][..],
        &["pregen-run", "--radius", "5", "--budget-per-tick", "4", "--in", "x"][..],
        &["pregen-run", "--radius", "5", "--budget-per-tick", "4", "extra"][..],
        &["pregen-run", "--radius", "5", "--budget-per-tick", "4", "--max-resident"][..],
    ] {
        let out = run(args);
        assert_eq!(code(&out), 2, "{args:?} -> {}", stdout(&out));
        assert!(stderr(&out).starts_with("mc-spine-world-migrate:"), "{args:?}");
    }
}
