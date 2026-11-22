// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane ROUTER-NAMESPACE-GUARD-1 integration tests: the unknown-namespace
//! preservation router — lossless quarantine of `modid:*` block/item/entity
//! payloads behind safe vanilla placeholders, schema-verified sidecar
//! persistence, restore-on-mod-return, and the verified pre-change
//! `WorldSnapshotStore` rollback — exercised against real files on disk.

use mc_spine_state_store::datafix::DataValue;
use mc_spine_state_store::namespace_guard::{
    guard_world_load_with_snapshot, namespace_of, NamespaceGuardError, NamespaceTombstoneSidecar,
    PreservedKind, UnknownEntry, PLACEHOLDER_BLOCK, PLACEHOLDER_ENTITY, PLACEHOLDER_ITEM,
};
use mc_spine_state_store::world_snapshot::{SnapshotReason, WorldSnapshotStore};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-nsguard-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn active_set(namespaces: &[&str]) -> BTreeSet<String> {
    namespaces.iter().map(|s| s.to_string()).collect()
}

fn map(pairs: &[(&str, DataValue)]) -> DataValue {
    DataValue::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

/// A deliberately deep, mixed-type block-entity payload: nested compounds,
/// lists, floats, bools, strings — exactly what `create`'s funnel stores.
fn brass_funnel_payload() -> DataValue {
    map(&[
        ("id", DataValue::Str("create:brass_funnel".to_string())),
        ("x", DataValue::Int(12)),
        ("y", DataValue::Int(64)),
        ("z", DataValue::Int(-8)),
        ("Extended", DataValue::Bool(true)),
        (
            "Filter",
            map(&[
                ("id", DataValue::Str("create:brass_casing".to_string())),
                ("Count", DataValue::Int(1)),
            ]),
        ),
        (
            "Items",
            DataValue::List(vec![
                map(&[
                    ("id", DataValue::Str("create:brass_sheet".to_string())),
                    ("Count", DataValue::Int(16)),
                    (
                        "tag",
                        map(&[(
                            "custom_name",
                            DataValue::Str("brass \"press\" output".to_string()),
                        )]),
                    ),
                ]),
                map(&[
                    ("id", DataValue::Str("minecraft:iron_ingot".to_string())),
                    ("Count", DataValue::Int(4)),
                ]),
            ]),
        ),
        (
            "BeltMotion",
            DataValue::List(vec![
                DataValue::Float(0.5),
                DataValue::Float(-1.25e-3),
                DataValue::Int(7),
            ]),
        ),
        ("Facing", DataValue::Str("north".to_string())),
    ])
}

fn item_payload() -> DataValue {
    map(&[
        ("id", DataValue::Str("create:wrench".to_string())),
        ("Count", DataValue::Int(1)),
        (
            "components",
            map(&[
                (
                    "minecraft:custom_name",
                    DataValue::Str("{\"text\":\"Engineer's Wrench\"}".to_string()),
                ),
                ("create:used", DataValue::Int(31)),
            ]),
        ),
    ])
}

fn entity_payload() -> DataValue {
    map(&[
        ("id", DataValue::Str("create:contraption".to_string())),
        ("Pos", DataValue::List(vec![
            DataValue::Float(12.5),
            DataValue::Float(64.0),
            DataValue::Float(-8.5),
        ])),
        ("Motionless", DataValue::Bool(false)),
        (
            "Blocks",
            map(&[
                ("create:mechanical_press", DataValue::Int(1)),
                ("create:brass_casing", DataValue::Int(3)),
            ]),
        ),
    ])
}

// ---------------------------------------------------- pass-through routing

#[test]
fn known_namespaces_pass_through_untouched() {
    let mut sidecar = NamespaceTombstoneSidecar::new();
    let active = active_set(&["minecraft", "create", "farmersdelight"]);

    for (kind, id) in [
        (PreservedKind::Block, "minecraft:stone"),
        (PreservedKind::Block, "create:mechanical_press"),
        (PreservedKind::Item, "farmersdelight:skillet"),
        // Bare path defaults to the vanilla namespace.
        (PreservedKind::Entity, "cow"),
    ] {
        let decision = sidecar
            .preserve_unknown(
                UnknownEntry {
                    kind,
                    original_id: id.to_string(),
                    location_key: format!("minecraft:overworld@0,64,0/{id}"),
                    raw_payload: map(&[("kept", DataValue::Bool(true))]),
                },
                100,
                &active,
            )
            .unwrap();
        assert_eq!(decision, None, "{id} must pass through with no substitution");
    }
    assert!(sidecar.is_empty());
    assert!(sidecar.summary_by_namespace().is_empty());
}

#[test]
fn malformed_ids_and_keys_fail_closed() {
    let mut sidecar = NamespaceTombstoneSidecar::new();
    let active = active_set(&["minecraft"]);
    let entry = |id: &str, key: &str| UnknownEntry {
        kind: PreservedKind::Block,
        original_id: id.to_string(),
        location_key: key.to_string(),
        raw_payload: DataValue::Null,
    };

    for bad_id in ["", "create:", ":brass_funnel", "a:b:c", ":"] {
        let err = sidecar
            .preserve_unknown(entry(bad_id, "minecraft:overworld@0,64,0"), 1, &active)
            .unwrap_err();
        assert!(
            matches!(err, NamespaceGuardError::InvalidEntry { .. }),
            "{bad_id:?}: expected InvalidEntry, got {err}"
        );
    }
    // Empty location key is refused the same way.
    let err = sidecar
        .preserve_unknown(entry("create:brass_funnel", ""), 1, &active)
        .unwrap_err();
    assert!(matches!(err, NamespaceGuardError::InvalidEntry { .. }));
    assert!(sidecar.is_empty());
    assert_eq!(namespace_of("stone"), Some("minecraft"));
    assert_eq!(namespace_of("create:brass_funnel"), Some("create"));
    assert_eq!(namespace_of(""), None);
}

// --------------------------------------------------- quarantine + payloads

#[test]
fn unknown_block_item_and_entity_are_preserved_losslessly() {
    let mut sidecar = NamespaceTombstoneSidecar::new();
    // `create` is absent; only vanilla and farmersdelight remain active.
    let active = active_set(&["minecraft", "farmersdelight"]);

    let block = UnknownEntry {
        kind: PreservedKind::Block,
        original_id: "create:brass_funnel".to_string(),
        location_key: "minecraft:overworld@12,64,-8".to_string(),
        raw_payload: brass_funnel_payload(),
    };
    let item = UnknownEntry {
        kind: PreservedKind::Item,
        original_id: "create:wrench".to_string(),
        location_key: "player:alex:slot:5".to_string(),
        raw_payload: item_payload(),
    };
    let entity = UnknownEntry {
        kind: PreservedKind::Entity,
        original_id: "create:contraption".to_string(),
        location_key: "minecraft:overworld@entity:7f3a".to_string(),
        raw_payload: entity_payload(),
    };

    // Every unknown kind routes to its safe vanilla placeholder.
    assert_eq!(
        sidecar.preserve_unknown(block.clone(), 900, &active).unwrap(),
        Some(PLACEHOLDER_BLOCK.to_string())
    );
    assert_eq!(
        sidecar.preserve_unknown(item.clone(), 900, &active).unwrap(),
        Some(PLACEHOLDER_ITEM.to_string())
    );
    assert_eq!(
        sidecar
            .preserve_unknown(entity.clone(), 900, &active)
            .unwrap(),
        Some(PLACEHOLDER_ENTITY.to_string())
    );

    assert_eq!(sidecar.len(), 3);
    let summary = sidecar.summary_by_namespace();
    assert_eq!(summary.get("create"), Some(&3));

    // The parked records carry the full original payload verbatim.
    let parked = sidecar.get("minecraft:overworld@12,64,-8").unwrap();
    assert_eq!(parked.kind, PreservedKind::Block);
    assert_eq!(parked.namespace, "create");
    assert_eq!(parked.placeholder_id, PLACEHOLDER_BLOCK);
    assert_eq!(parked.quarantined_at_tick, 900);
    assert_eq!(
        parked.raw_payload, block.raw_payload,
        "block-entity NBT must be preserved byte/value-identically"
    );
    assert_eq!(
        sidecar.get("player:alex:slot:5").unwrap().raw_payload,
        item.raw_payload
    );
    assert_eq!(
        sidecar
            .get("minecraft:overworld@entity:7f3a")
            .unwrap()
            .raw_payload,
        entity.raw_payload
    );
}

// ------------------------------------------- persist -> reload -> restore

#[test]
fn save_reload_and_restore_round_trip_losslessly() {
    let dir = TestDir::new("roundtrip");
    let sidecar_path = dir.path("namespace_tombstones.json");

    let mut sidecar = NamespaceTombstoneSidecar::new();
    let active = active_set(&["minecraft"]);
    let block = UnknownEntry {
        kind: PreservedKind::Block,
        original_id: "create:brass_funnel".to_string(),
        location_key: "minecraft:overworld@12,64,-8".to_string(),
        raw_payload: brass_funnel_payload(),
    };
    let item = UnknownEntry {
        kind: PreservedKind::Item,
        original_id: "create:wrench".to_string(),
        location_key: "player:alex:slot:5".to_string(),
        raw_payload: item_payload(),
    };
    let entity = UnknownEntry {
        kind: PreservedKind::Entity,
        original_id: "create:contraption".to_string(),
        location_key: "minecraft:overworld@entity:7f3a".to_string(),
        raw_payload: entity_payload(),
    };
    // A second missing mod, to prove partial restore keeps its entries parked.
    let other = UnknownEntry {
        kind: PreservedKind::Block,
        original_id: "farmersdelight:stove".to_string(),
        location_key: "minecraft:overworld@1,64,1".to_string(),
        raw_payload: map(&[("Cooking", DataValue::Bool(true))]),
    };
    for entry in [block.clone(), item.clone(), entity.clone(), other.clone()] {
        sidecar.preserve_unknown(entry, 42, &active).unwrap();
    }
    assert_eq!(sidecar.len(), 4);

    sidecar.save_atomic(&sidecar_path).unwrap();
    assert!(sidecar_path.is_file());

    // Reload verifies the schema and reconstructs the identical table.
    let mut loaded = NamespaceTombstoneSidecar::load_verified(&sidecar_path).unwrap();
    assert_eq!(loaded, sidecar);
    assert_eq!(
        loaded.get("minecraft:overworld@12,64,-8").unwrap().raw_payload,
        block.raw_payload
    );

    // `create` is re-installed. Everything from that namespace pops back out
    // with a value-identical payload; the still-missing mod stays parked.
    let active_again = active_set(&["minecraft", "create"]);
    let restored = loaded.restore_for_reloaded_namespaces(&active_again);
    assert_eq!(restored.len(), 3);
    // Deterministic order: sorted by location_key.
    let keys: Vec<&str> = restored.iter().map(|e| e.location_key.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
    for entry in &restored {
        assert_eq!(entry.namespace, "create");
        assert_eq!(entry.quarantined_at_tick, 42);
    }
    let by_loc: BTreeMap<&str, &mc_spine_state_store::namespace_guard::PreservedEntry> =
        restored.iter().map(|e| (e.location_key.as_str(), e)).collect();
    assert_eq!(
        by_loc["minecraft:overworld@12,64,-8"].raw_payload,
        block.raw_payload
    );
    assert_eq!(by_loc["player:alex:slot:5"].raw_payload, item.raw_payload);
    assert_eq!(
        by_loc["minecraft:overworld@entity:7f3a"].raw_payload,
        entity.raw_payload
    );
    // Restored entries are gone; the farmersdelight entry is still quarantined.
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.summary_by_namespace()["farmersdelight"], 1);
    assert!(loaded.get("minecraft:overworld@12,64,-8").is_none());
    assert!(loaded.get("minecraft:overworld@1,64,1").is_some());

    // A second restore with the same set is a clean no-op, and restoring the
    // last namespace empties the sidecar.
    assert!(loaded
        .restore_for_reloaded_namespaces(&active_again)
        .is_empty());
    let rest = loaded.restore_for_reloaded_namespaces(&active_set(&[
        "minecraft",
        "create",
        "farmersdelight",
    ]));
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].raw_payload, other.raw_payload);
    assert!(loaded.is_empty());
}

// --------------------------------------------- guarded load with snapshot

#[test]
fn guard_world_load_takes_verified_snapshot_before_mutating() {
    let dir = TestDir::new("guard");
    let world = dir.path("world");
    let backup = dir.path(".megu-snapshots");
    let sidecar_path = dir.path("namespace_tombstones.json");
    std::fs::create_dir_all(&world).unwrap();
    std::fs::write(world.join("level.dat"), b"level-dat: seed=99").unwrap();
    std::fs::create_dir_all(world.join("region")).unwrap();
    std::fs::create_dir_all(world.join("data")).unwrap();
    std::fs::write(world.join("region/r.0.0.mca"), [b'R'; 1024]).unwrap();
    std::fs::write(world.join("data/mods.dat"), b"create-6.0 data").unwrap();

    let active = active_set(&["minecraft"]);
    let discovered = vec![
        UnknownEntry {
            kind: PreservedKind::Block,
            original_id: "create:brass_funnel".to_string(),
            location_key: "minecraft:overworld@12,64,-8".to_string(),
            raw_payload: brass_funnel_payload(),
        },
        UnknownEntry {
            kind: PreservedKind::Item,
            original_id: "create:wrench".to_string(),
            location_key: "player:alex:slot:5".to_string(),
            raw_payload: item_payload(),
        },
        // A live namespace passes through: it is not counted, not parked.
        UnknownEntry {
            kind: PreservedKind::Block,
            original_id: "minecraft:stone".to_string(),
            location_key: "minecraft:overworld@0,64,0".to_string(),
            raw_payload: map(&[("Name", DataValue::Str("minecraft:stone".to_string()))]),
        },
    ];

    let report = guard_world_load_with_snapshot(
        &world,
        &backup,
        &sidecar_path,
        &active,
        &discovered,
        "26.3",
        1_700_000_000,
    )
    .unwrap();

    assert!(report.safe_to_proceed);
    assert_eq!(report.preserved_counts_by_namespace.get("create"), Some(&2));
    assert!(!report.snapshot_id.is_empty());

    // The rollback is real: a `pre_mod_change` manifest that verifies
    // blob-for-blob against the world as it was before any mutation.
    let store = WorldSnapshotStore::new(&world, &backup).unwrap();
    let manifest = store.load_manifest(&report.snapshot_id).unwrap();
    assert_eq!(manifest.timestamp_secs, 1_700_000_000);
    assert_eq!(manifest.mc_version, "26.3");
    assert_eq!(manifest.file_count, 3);
    assert_eq!(
        manifest.reason,
        SnapshotReason::PreModChange {
            summary: "pre-namespace-preserve".to_string()
        }
    );
    let verify = store.verify_snapshot(&report.snapshot_id).unwrap();
    assert_eq!(verify.files_verified, 3);

    // The sidecar was written atomically and round-trips the payloads.
    let loaded = NamespaceTombstoneSidecar::load_verified(&sidecar_path).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(
        loaded.get("minecraft:overworld@12,64,-8").unwrap().raw_payload,
        brass_funnel_payload()
    );
    assert_eq!(
        loaded.get("player:alex:slot:5").unwrap().raw_payload,
        item_payload()
    );

    // A second load with the mod restored must not park the still-missing
    // entries again — the merge path keeps them, and the live one passes.
    let active_again = active_set(&["minecraft", "create"]);
    let report2 = guard_world_load_with_snapshot(
        &world,
        &backup,
        &sidecar_path,
        &active_again,
        &[],
        "26.3",
        1_700_000_100,
    )
    .unwrap();
    assert!(report2.safe_to_proceed);
    assert!(report2.preserved_counts_by_namespace.is_empty());
    let merged = NamespaceTombstoneSidecar::load_verified(&sidecar_path).unwrap();
    assert_eq!(merged.len(), 2, "earlier parked entries must survive reloads");
    store.verify_snapshot(&report2.snapshot_id).unwrap();
}

#[test]
fn guard_fails_closed_without_a_world() {
    let dir = TestDir::new("guard-noworld");
    let world = dir.path("missing-world");
    let backup = dir.path("backup");
    let sidecar_path = dir.path("sidecar.json");
    let active = active_set(&["minecraft"]);
    let err = guard_world_load_with_snapshot(&world, &backup, &sidecar_path, &active, &[], "26.3", 1)
        .unwrap_err();
    assert!(
        matches!(err, NamespaceGuardError::Snapshot(_)),
        "expected Snapshot, got {err}"
    );
    // And nothing was written: no sidecar appeared on a failed guard.
    assert!(!sidecar_path.exists());
}

// ------------------------------------------------------- fail-closed load

#[test]
fn corrupt_or_foreign_sidecar_fails_closed() {
    let dir = TestDir::new("corrupt");
    let path = dir.path("sidecar.json");

    // Not JSON at all.
    std::fs::write(&path, b"\x00\x01not json{{{").unwrap();
    assert!(matches!(
        NamespaceTombstoneSidecar::load_verified(&path).unwrap_err(),
        NamespaceGuardError::Json(_) | NamespaceGuardError::Schema(_)
    ));

    // Valid JSON, wrong format tag.
    std::fs::write(
        &path,
        r#"{"format":"mcspine-world-snapshot-v1","entries":[]}"#,
    )
    .unwrap();
    assert!(matches!(
        NamespaceTombstoneSidecar::load_verified(&path).unwrap_err(),
        NamespaceGuardError::Schema(_)
    ));

    // Right tag, but entries is not an array.
    std::fs::write(&path, r#"{"format":"mcspine-namespace-tombstone-v1","entries":{}}"#).unwrap();
    assert!(matches!(
        NamespaceTombstoneSidecar::load_verified(&path).unwrap_err(),
        NamespaceGuardError::Schema(_)
    ));

    // A forged entry whose namespace lies about its original_id.
    std::fs::write(
        &path,
        r#"{"entries":[{"kind":"block","location_key":"minecraft:overworld@1,2,3","namespace":"forge","original_id":"create:press","placeholder_id":"minecraft:structure_void","quarantined_at_tick":1,"raw_payload":{}}],"entry_count":1,"format":"mcspine-namespace-tombstone-v1"}"#,
    )
    .unwrap();
    assert!(matches!(
        NamespaceTombstoneSidecar::load_verified(&path).unwrap_err(),
        NamespaceGuardError::Schema(_)
    ));

    // A modded placeholder id is refused: it would itself need quarantining.
    std::fs::write(
        &path,
        r#"{"entries":[{"kind":"item","location_key":"player:a:slot:0","namespace":"create","original_id":"create:wrench","placeholder_id":"create:wrench","quarantined_at_tick":1,"raw_payload":{}}],"entry_count":1,"format":"mcspine-namespace-tombstone-v1"}"#,
    )
    .unwrap();
    assert!(matches!(
        NamespaceTombstoneSidecar::load_verified(&path).unwrap_err(),
        NamespaceGuardError::Schema(_)
    ));

    // Declared entry_count disagreeing with the array.
    std::fs::write(
        &path,
        r#"{"entries":[],"entry_count":7,"format":"mcspine-namespace-tombstone-v1"}"#,
    )
    .unwrap();
    assert!(matches!(
        NamespaceTombstoneSidecar::load_verified(&path).unwrap_err(),
        NamespaceGuardError::Schema(_)
    ));

    // Missing file is an error too — never a silent empty sidecar.
    assert!(NamespaceTombstoneSidecar::load_verified(&dir.path("absent.json")).is_err());
}

#[test]
fn sidecar_is_deterministic_across_insertion_orders() {
    let dir = TestDir::new("determinism");
    let active = active_set(&["minecraft"]);
    let mk = |id: &str, key: &str| UnknownEntry {
        kind: PreservedKind::Block,
        original_id: id.to_string(),
        location_key: key.to_string(),
        raw_payload: map(&[("v", DataValue::Int(1))]),
    };
    let entries = [
        mk("create:b", "dim@2,0,0"),
        mk("create:a", "dim@1,0,0"),
        mk("modx:z", "dim@3,0,0"),
    ];

    let mut forward = NamespaceTombstoneSidecar::new();
    let mut reverse = NamespaceTombstoneSidecar::new();
    for e in entries.iter() {
        forward.preserve_unknown(e.clone(), 5, &active).unwrap();
    }
    for e in entries.iter().rev() {
        reverse.preserve_unknown(e.clone(), 5, &active).unwrap();
    }
    assert_eq!(forward, reverse);

    // Sorted-key serialization: identical documents byte for byte.
    let p1 = dir.path("s1.json");
    let p2 = dir.path("s2.json");
    forward.save_atomic(&p1).unwrap();
    reverse.save_atomic(&p2).unwrap();
    assert_eq!(std::fs::read(&p1).unwrap(), std::fs::read(&p2).unwrap());
    let text = std::fs::read_to_string(&p1).unwrap();
    assert!(text.contains("mcspine-namespace-tombstone-v1"));
}
