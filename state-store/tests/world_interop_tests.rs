// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mc_spine_state_store::anvil_inspect::{
    decode_nbt_to_data_value, encode_data_value_to_nbt, gzip_decompress, gzip_encode_stored,
    NBT_SIZE_CAP,
};
use mc_spine_state_store::datafix::{DataValue, DV_1_21_1, DV_26_3};
use mc_spine_state_store::namespace_guard::{
    NamespaceTombstoneSidecar, PreservedKind, UnknownEntry,
};
use mc_spine_state_store::world_interop::{
    extract_preserved_attachments, inspect_world_interop, merge_preserved_attachments,
    prepare_cross_loader_handoff, roundtrip_level_dat_preserving_attachments,
    roundtrip_playerdata_preserving_attachments, CrossLoaderHandoff, InteropError,
    WorldLoaderOrigin, PRESERVED_MOD_ATTACHMENT_KEYS,
};
use mc_spine_state_store::world_snapshot::{SnapshotReason, WorldSnapshotStore};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
const UUID: &str = "12345678-1234-1234-1234-123456789abc";

struct TestWorld(PathBuf);

impl TestWorld {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "interop-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn put(&self, name: &str, bytes: &[u8]) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn level(&self, dv: i32, brands: &[&str], extras: &[(&str, DataValue)]) {
        let mut data = BTreeMap::from([
            ("DataVersion".to_string(), DataValue::Int(i64::from(dv))),
            (
                "LevelName".to_string(),
                DataValue::Str("Interop Test".to_string()),
            ),
            ("DayTime".to_string(), DataValue::Int(400)),
            ("Time".to_string(), DataValue::Int(1200)),
            (
                "DataPacks".to_string(),
                map(&[("Enabled", strings(&["vanilla", "file/mod.zip"]))]),
            ),
            (
                "WorldGenSettings".to_string(),
                map(&[("seed", DataValue::Int(929292))]),
            ),
        ]);
        if !brands.is_empty() {
            data.insert("ServerBrands".to_string(), strings(brands));
        }
        for (key, val) in extras {
            data.insert((*key).to_string(), val.clone());
        }
        self.put("level.dat", &gzip(&map(&[("Data", DataValue::Map(data))])));
    }

    fn regions(&self) {
        for file in [
            "region/r.0.0.mca",
            "entities/r.-1.0.mca",
            "poi/r.0.1.mca",
            "DIM-1/region/r.0.0.mca",
            "DIM-1/entities/r.0.0.mca",
            "DIM-1/poi/r.0.0.mca",
            "DIM1/region/r.0.0.mca",
            "DIM1/entities/r.0.0.mca",
            "DIM1/poi/r.0.0.mca",
        ] {
            self.put(file, &[0; 8192]);
        }
    }

    fn companions(&self) {
        self.put(
            &format!("playerdata/{UUID}.dat"),
            &gzip(&map(&[("Health", DataValue::Float(20.0))])),
        );
        self.put(
            &format!("advancements/{UUID}.json"),
            br#"{"minecraft:story/root":{"done":true}}"#,
        );
        self.put(
            &format!("stats/{UUID}.json"),
            br#"{"stats":{"minecraft:custom":{}},"DataVersion":3955}"#,
        );
        self.put(
            "data/scoreboard.dat",
            &gzip(&map(&[(
                "data",
                map(&[("Objectives", DataValue::List(vec![]))]),
            )])),
        );
    }
}

impl Drop for TestWorld {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn map(pairs: &[(&str, DataValue)]) -> DataValue {
    DataValue::Map(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    )
}

fn strings(values: &[&str]) -> DataValue {
    DataValue::List(
        values
            .iter()
            .map(|s| DataValue::Str((*s).to_string()))
            .collect(),
    )
}

fn gzip(doc: &DataValue) -> Vec<u8> {
    gzip_encode_stored(&encode_data_value_to_nbt("", doc).unwrap())
}

fn decoded_gzip(gz: &[u8]) -> DataValue {
    let raw = gzip_decompress(gz, NBT_SIZE_CAP).unwrap();
    decode_nbt_to_data_value(&raw).unwrap().1
}

fn raw_tag(id: u8, name: &str, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![id];
    bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn has_raw_tag(document: &[u8], raw: &[u8]) -> bool {
    document.windows(raw.len()).any(|window| window == raw)
}

fn attachments() -> Vec<(&'static str, DataValue)> {
    vec![
        (
            "neoforge:attachments",
            map(&[(
                "create:rail",
                map(&[(
                    "trains",
                    DataValue::List(vec![DataValue::Int(4), DataValue::Int(5)]),
                )]),
            )]),
        ),
        ("ForgeData", map(&[("mod:power", DataValue::Int(47))])),
        ("ForgeCaps", map(&[("mod:energy", DataValue::Int(900))])),
        (
            "fml",
            map(&[("LoadingModList", strings(&["create", "twilightforest"]))]),
        ),
        (
            "fabric:attachments",
            map(&[("origins:power", DataValue::Str("wings".into()))]),
        ),
        (
            "cardinal_components",
            map(&[("trinkets:slots", strings(&["ring", "amulet"]))]),
        ),
    ]
}

#[test]
fn vanilla_1211_and_263_discover_standard_dimensions_and_all_companions() {
    for dv in [DV_1_21_1, DV_26_3] {
        let world = TestWorld::new();
        world.level(dv, &["vanilla"], &[]);
        world.regions();
        world.companions();
        let report = inspect_world_interop(&world.0).unwrap();
        assert_eq!(report.origin_loader, WorldLoaderOrigin::Vanilla);
        assert_eq!(report.data_version, dv);
        assert_eq!(report.level_name.as_deref(), Some("Interop Test"));
        assert_eq!(report.companion_files.playerdata, 1);
        assert_eq!(report.companion_files.advancements, 1);
        assert_eq!(report.companion_files.stats, 1);
        assert_eq!(report.companion_files.saved_data, 1);
        assert_eq!(report.dimensions.len(), 3);
        assert!(report
            .dimensions
            .iter()
            .all(|d| d.region_files == 1 && d.entity_files == 1 && d.poi_files == 1));
        assert_eq!(
            report.data_packs.unwrap().as_map().unwrap()["Enabled"],
            strings(&["vanilla", "file/mod.zip"])
        );
        assert_eq!(
            report.world_gen_settings.unwrap().as_map().unwrap()["seed"],
            DataValue::Int(929292)
        );
    }
}

#[test]
fn neoforge_and_modded_dimensions_are_discovered() {
    let world = TestWorld::new();
    let keys = attachments();
    world.level(DV_1_21_1, &["neoforge"], &keys[..4]);
    for kind in ["region", "entities", "poi"] {
        world.put(
            &format!("dimensions/twilightforest/twilight_forest/{kind}/r.0.0.mca"),
            &[0; 8192],
        );
    }
    world.put("dimensions/ad_astra/moon/region/r.-2.3.mca", &[0; 8192]);
    let report = inspect_world_interop(&world.0).unwrap();
    assert_eq!(report.origin_loader, WorldLoaderOrigin::NeoForge);
    assert_eq!(report.loader_tags["ForgeData"], keys[1].1);
    assert_eq!(report.loader_tags["neoforge:attachments"], keys[0].1);
    let twilight = report
        .dimensions
        .iter()
        .find(|d| d.id == "twilightforest:twilight_forest")
        .unwrap();
    assert_eq!(
        (
            twilight.region_files,
            twilight.entity_files,
            twilight.poi_files
        ),
        (1, 1, 1)
    );
    assert_eq!(
        twilight.relative_root,
        Path::new("dimensions/twilightforest/twilight_forest")
    );
    assert!(report
        .dimensions
        .iter()
        .any(|d| d.id == "ad_astra:moon" && d.region_files == 1));
}

#[test]
fn fabric_and_hybrid_brands_detect_loader_tags() {
    let world = TestWorld::new();
    let keys = attachments();
    world.level(DV_26_3, &["fabric"], &keys[4..]);
    let report = inspect_world_interop(&world.0).unwrap();
    assert_eq!(report.origin_loader, WorldLoaderOrigin::Fabric);
    assert_eq!(report.loader_tags["cardinal_components"], keys[5].1);
    assert_eq!(report.loader_tags["fabric:attachments"], keys[4].1);
    world.level(DV_26_3, &["fabric", "neoforge"], &keys);
    let report = inspect_world_interop(&world.0).unwrap();
    assert_eq!(
        report.origin_loader,
        WorldLoaderOrigin::HybridModded {
            detected_brands: vec!["fabric".into(), "neoforge".into()]
        }
    );
}

#[test]
fn unknown_compound_keys_and_loader_tags_are_kept() {
    let doc = map(&[
        ("Health", DataValue::Float(20.0)),
        ("mod:custom", map(&[("value", DataValue::Int(99))])),
        (
            "neoforge:attachments",
            map(&[("create:state", DataValue::Int(9))]),
        ),
    ]);
    let preserved = extract_preserved_attachments(&doc, &["Health", "neoforge:attachments"]);
    assert_eq!(preserved.len(), 2);
    assert_eq!(preserved["mod:custom"], doc.as_map().unwrap()["mod:custom"]);
    let mut edited = map(&[("Health", DataValue::Float(16.0))]);
    merge_preserved_attachments(&mut edited, &preserved);
    assert_eq!(edited.as_map().unwrap()["Health"], DataValue::Float(16.0));
    assert_eq!(
        edited.as_map().unwrap()["mod:custom"],
        preserved["mod:custom"]
    );
    assert!(PRESERVED_MOD_ATTACHMENT_KEYS.contains(&"cardinal_components"));
}

#[test]
fn playerdata_updates_core_and_restores_original_unknown_values() {
    let keys = attachments();
    let mut player = map(&[
        ("Health", DataValue::Float(20.0)),
        (
            "Pos",
            DataValue::List(vec![
                DataValue::Float(1.25),
                DataValue::Float(64.0),
                DataValue::Float(-3.5),
            ]),
        ),
        (
            "custom:odd",
            map(&[(
                "Nested",
                DataValue::List(vec![DataValue::Int(1), DataValue::Int(2)]),
            )]),
        ),
    ]);
    for (k, v) in &keys {
        player.as_map_mut().unwrap().insert((*k).into(), v.clone());
    }
    let original = gzip(&player);
    let output = roundtrip_playerdata_preserving_attachments(&original, |doc| {
        let fields = doc.as_map_mut().unwrap();
        fields.insert("Health".to_string(), DataValue::Float(13.5));
        fields.insert(
            "Pos".to_string(),
            DataValue::List(vec![
                DataValue::Float(6.0),
                DataValue::Float(80.0),
                DataValue::Float(9.0),
            ]),
        );
        fields.remove("neoforge:attachments");
        fields.insert("cardinal_components".into(), DataValue::Int(0));
    })
    .unwrap();
    let after = decoded_gzip(&output);
    assert_eq!(after.as_map().unwrap()["Health"], DataValue::Float(13.5));
    assert_eq!(after.as_map().unwrap()["Pos"].as_map(), None);
    assert_eq!(
        after.as_map().unwrap()["Pos"],
        DataValue::List(vec![
            DataValue::Float(6.0),
            DataValue::Float(80.0),
            DataValue::Float(9.0)
        ])
    );
    for (key, _) in keys {
        assert_eq!(after.as_map().unwrap()[key], player.as_map().unwrap()[key]);
    }
    assert_eq!(
        after.as_map().unwrap()["custom:odd"],
        player.as_map().unwrap()["custom:odd"]
    );
    assert_eq!(decoded_gzip(&output), decoded_gzip(&gzip(&after)));
}

#[test]
fn roundtrip_preserves_unknown_nbt_wire_types_and_payload_bytes() {
    let mut player = encode_data_value_to_nbt(
        "",
        &map(&[(
            "Pos",
            DataValue::List(vec![DataValue::Float(2.5), DataValue::Float(66.0)]),
        )]),
    )
    .unwrap();
    assert_eq!(player.pop(), Some(0));
    let original_health = raw_tag(5, "Health", &20.0_f32.to_be_bytes());
    player.extend_from_slice(&original_health);
    // Type 7 is ByteArray, type 12 is LongArray, and type 5 is Float32.
    // DataValue would canonicalize these as List<Int> and Float64.
    let unknown_byte_array = raw_tag(7, "fabric:attachments", &[0, 0, 0, 3, 0x80, 0xff, 0x00]);
    let unknown_long_array = raw_tag(
        12,
        "mod:longs",
        &[
            0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ],
    );
    let mut nested_float = raw_tag(5, "charge", &1.25_f32.to_be_bytes());
    nested_float.push(0);
    let unknown_compound = raw_tag(10, "ForgeData", &nested_float);
    for tag in [&unknown_byte_array, &unknown_long_array, &unknown_compound] {
        player.extend_from_slice(tag);
    }
    player.push(0);
    let after = roundtrip_playerdata_preserving_attachments(&gzip_encode_stored(&player), |doc| {
        let fields = doc.as_map_mut().unwrap();
        fields.insert("Health".into(), DataValue::Float(8.5));
        fields.remove("fabric:attachments");
        fields.remove("ForgeData");
    })
    .unwrap();
    let after_raw = gzip_decompress(&after, NBT_SIZE_CAP).unwrap();
    for tag in [&unknown_byte_array, &unknown_long_array, &unknown_compound] {
        assert!(
            has_raw_tag(&after_raw, tag),
            "mod tag changed NBT wire representation"
        );
    }
    assert!(has_raw_tag(
        &after_raw,
        &raw_tag(5, "Health", &8.5_f32.to_be_bytes())
    ));
    assert_eq!(
        decoded_gzip(&after).as_map().unwrap()["Health"],
        DataValue::Float(8.5)
    );

    let mut level = encode_data_value_to_nbt(
        "",
        &map(&[(
            "Data",
            map(&[("DataVersion", DataValue::Int(DV_1_21_1.into()))]),
        )]),
    )
    .unwrap();
    assert_eq!(level.pop(), Some(0));
    assert_eq!(level.pop(), Some(0));
    level.extend_from_slice(&raw_tag(4, "DayTime", &100_i64.to_be_bytes()));
    level.extend_from_slice(&unknown_byte_array);
    level.extend_from_slice(&unknown_compound);
    level.extend_from_slice(&[0, 0]);
    let after = roundtrip_level_dat_preserving_attachments(&gzip_encode_stored(&level), |doc| {
        doc.as_map_mut()
            .unwrap()
            .get_mut("Data")
            .unwrap()
            .as_map_mut()
            .unwrap()
            .insert("DayTime".into(), DataValue::Int(101));
    })
    .unwrap();
    let after_raw = gzip_decompress(&after, NBT_SIZE_CAP).unwrap();
    assert!(has_raw_tag(&after_raw, &unknown_byte_array));
    assert!(has_raw_tag(&after_raw, &unknown_compound));
    assert!(has_raw_tag(
        &after_raw,
        &raw_tag(4, "DayTime", &101_i64.to_be_bytes())
    ));
}

#[test]
fn level_dat_daytime_update_preserves_modded_and_worldgen_tags() {
    let keys = attachments();
    let world = TestWorld::new();
    world.level(DV_26_3, &["neoforge"], &keys);
    let before = decoded_gzip(&fs::read(world.path("level.dat")).unwrap());
    let output = roundtrip_level_dat_preserving_attachments(
        &fs::read(world.path("level.dat")).unwrap(),
        |doc| {
            let data = doc.as_map_mut().unwrap().get_mut("Data").unwrap();
            let fields = data.as_map_mut().unwrap();
            fields.insert("DayTime".into(), DataValue::Int(9999));
            fields.remove("fml");
            fields.remove("WorldGenSettings");
            fields.insert("DataPacks".into(), DataValue::Int(1));
            fields.insert("fabric:attachments".into(), DataValue::Int(0));
        },
    )
    .unwrap();
    let after = decoded_gzip(&output);
    let old = before.as_map().unwrap()["Data"].as_map().unwrap();
    let new = after.as_map().unwrap()["Data"].as_map().unwrap();
    assert_eq!(new["DayTime"], DataValue::Int(9999));
    for name in [
        "fml",
        "WorldGenSettings",
        "DataPacks",
        "fabric:attachments",
        "cardinal_components",
        "neoforge:attachments",
    ] {
        assert_eq!(new[name], old[name], "{name} changed");
    }
}

#[test]
fn verified_snapshot_and_namespace_tombstones_before_cross_loader_handoff() {
    let world = TestWorld::new();
    world.level(DV_1_21_1, &["neoforge"], &attachments()[..4]);
    world.regions();
    world.companions();
    let original_level = fs::read(world.path("level.dat")).unwrap();
    // Keep the snapshot outside its input tree.
    let backup = world.0.with_file_name(format!(
        "{}-backup",
        world.0.file_name().unwrap().to_string_lossy()
    ));
    let sidecar = world.path("data/interop-tombstones.json");
    let unknown = vec![UnknownEntry {
        kind: PreservedKind::Block,
        original_id: "create:brass_funnel".into(),
        location_key: "minecraft:overworld@12,64,-8".into(),
        raw_payload: map(&[
            ("id", DataValue::Str("create:brass_funnel".into())),
            ("custom", DataValue::Int(89)),
        ]),
    }];
    let active = BTreeSet::from(["minecraft".to_string()]);
    let report = prepare_cross_loader_handoff(CrossLoaderHandoff {
        world_root: &world.0,
        backup_root: &backup,
        sidecar_path: &sidecar,
        target_loader: WorldLoaderOrigin::Vanilla,
        active_namespaces: &active,
        discovered_unknown: &unknown,
        mc_version: "1.21.1",
        timestamp_secs: 1_800_000_000,
    })
    .unwrap();
    assert_eq!(report.origin_loader, WorldLoaderOrigin::NeoForge);
    assert_eq!(report.target_loader, Some(WorldLoaderOrigin::Vanilla));
    assert_eq!(report.parked_unknown_entries, 1);
    let id = report.verified_snapshot_id.unwrap();
    let store = WorldSnapshotStore::new(&world.0, &backup).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    assert!(matches!(
        manifest.reason,
        SnapshotReason::PreModChange { .. }
    ));
    assert_eq!(
        store.verify_snapshot(&id).unwrap().files_verified,
        report.snapshot_files_verified.unwrap()
    );
    assert_eq!(fs::read(world.path("level.dat")).unwrap(), original_level);
    let sidecar_data = NamespaceTombstoneSidecar::load_verified(&sidecar).unwrap();
    assert_eq!(
        sidecar_data
            .get("minecraft:overworld@12,64,-8")
            .unwrap()
            .raw_payload,
        unknown[0].raw_payload
    );
    fs::remove_dir_all(&backup).unwrap();
}

#[test]
fn corrupt_region_and_companion_files_refuse_handoff_before_snapshot() {
    for invalid in [
        "region/r.0.0.mca",
        "data/broken.dat",
        "stats/bad.json",
        "playerdata/missing.dat",
    ] {
        let world = TestWorld::new();
        world.level(DV_1_21_1, &["vanilla"], &[]);
        world.put(invalid, b"invalid");
        let backup = world.path("backup");
        assert!(inspect_world_interop(&world.0).is_err(), "{invalid}");
        let active = BTreeSet::new();
        assert!(prepare_cross_loader_handoff(CrossLoaderHandoff {
            world_root: &world.0,
            backup_root: &backup,
            sidecar_path: &world.path("data/preserved.json"),
            target_loader: WorldLoaderOrigin::Fabric,
            active_namespaces: &active,
            discovered_unknown: &[],
            mc_version: "1.21.1",
            timestamp_secs: 10,
        })
        .is_err());
        assert!(!backup.exists());
    }
}

#[test]
fn rejects_invalid_nbt_and_size_bombs() {
    let world = TestWorld::new();
    world.level(DV_1_21_1, &["vanilla"], &[]);
    world.put("level.dat", &[0x1f, 0x8b, 8, 0]);
    assert!(inspect_world_interop(&world.0).is_err());
    assert!(roundtrip_playerdata_preserving_attachments(b"corrupt", |_| {}).is_err());
    world.level(DV_1_21_1, &["vanilla"], &[]);
    let huge = world.path("region/r.0.0.mca");
    fs::create_dir_all(huge.parent().unwrap()).unwrap();
    fs::File::create(&huge)
        .unwrap()
        .set_len(129 * 1_048_576)
        .unwrap();
    assert!(matches!(
        inspect_world_interop(&world.0),
        Err(InteropError::InvalidFile { .. })
    ));
}

#[test]
fn unexpected_mca_layout_is_rejected() {
    let world = TestWorld::new();
    world.level(DV_1_21_1, &["neoforge"], &[]);
    world.put("dimensions/twilightforest/region/r.0.0.mca", &[0; 8192]);
    assert!(matches!(
        inspect_world_interop(&world.0),
        Err(InteropError::InvalidFile { .. })
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_dimensions_and_traversal_fail_closed() {
    use std::os::unix::fs::symlink;
    let world = TestWorld::new();
    world.level(DV_1_21_1, &["neoforge"], &[]);
    symlink(&world.0, world.path("dimensions")).unwrap();
    assert!(matches!(
        inspect_world_interop(&world.0),
        Err(InteropError::UnsafePath { .. })
    ));
    fs::remove_file(world.path("dimensions")).unwrap();
    assert!(matches!(
        inspect_world_interop(&world.path("../outside")),
        Err(InteropError::UnsafePath { .. })
    ));
    let active = BTreeSet::new();
    let backup = world.path("backup");
    symlink(world.path("level.dat"), world.path("data-link")).unwrap();
    assert!(prepare_cross_loader_handoff(CrossLoaderHandoff {
        world_root: &world.0,
        backup_root: &backup,
        sidecar_path: &world.path("data/preserved.json"),
        target_loader: WorldLoaderOrigin::Vanilla,
        active_namespaces: &active,
        discovered_unknown: &[],
        mc_version: "1.21.1",
        timestamp_secs: 11,
    })
    .is_err());
    assert!(!backup.exists());
}

#[test]
fn overlapping_mca_sectors_are_rejected() {
    let world = TestWorld::new();
    world.level(DV_1_21_1, &["vanilla"], &[]);
    let mut region = vec![0; 12288];
    region[0..4].copy_from_slice(&[0, 0, 2, 1]);
    region[4..8].copy_from_slice(&[0, 0, 2, 1]);
    region[8192..8196].copy_from_slice(&2u32.to_be_bytes());
    region[8196] = 3;
    world.put("region/r.0.0.mca", &region);
    assert!(matches!(
        inspect_world_interop(&world.0),
        Err(InteropError::InvalidFile { .. })
    ));
}
