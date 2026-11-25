// SPDX-License-Identifier: LGPL-3.0-or-later

//! Wave-4 integration proof: every mod lane talks to one `ModSpineHost`
//! through the megu-core CS primitives rather than lane-local side tables.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use megu_core::bvh::Aabb3;
use megu_core::coords::{BlockPos, DimIndex};
use megu_core::registry::RegistryId;
use megu_core::rev::{Revision, UiLayer};
use megu_core::ui_contract::{
    audit_manifest, ModUiManifest, UiExtensionSlot, UiRect, UiWidgetKind, UiWidgetSpec,
};
use megu_wasm_mod_runtime::testutil::{
    body, call, code_sec, concat, export_sec, func_sec, i32_const, import_funcs_sec, module,
    types_sec, I32, I64, KIND_FUNC,
};
use megu_wasm_mod_runtime::{CapabilitySet, MeguHost, ModConfig, WasmMod};
use mod_adapter_core::mod_isolation::{
    diff_snapshot_records, snapshot_merkle_tree, SnapshotCasDeduper, SnapshotFileEntry,
    SnapshotRecord, SnapshotTrigger, TimeMachineConfig, WorldTimeMachine,
};
use mod_adapter_core::spine_bridge::{
    ModAttachmentTarget, ModNetworkNodeKind, ModSpatialObjectKind, ModSpineHost, ModWatch,
};

fn temp_dir(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("megu-cs-mod-spine-{name}-{}-{unique}", std::process::id()))
}

fn labelled_widget(label: &str) -> UiWidgetSpec {
    UiWidgetSpec::new(UiWidgetKind::Label, UiRect::new(4, 4, 120, 12)).with_label(label)
}

#[test]
fn universal_registry_content_digest_is_stable_and_freezes() {
    let mut host = ModSpineHost::new();
    let copper = host
        .content_registries_mut()
        .register_block("create", "create:copper_pipe")
        .expect("register block");
    let wrench = host
        .content_registries_mut()
        .register_item("create", "create:wrench")
        .expect("register item");
    let channel = host
        .content_registries_mut()
        .register_channel("fabric", "create:contraption_sync")
        .expect("register channel");
    assert_eq!(copper, RegistryId(0));
    assert_eq!(wrench, RegistryId(0));
    assert_eq!(channel, RegistryId(0));

    let digest_a = host.content_registries().digest_crc32();
    assert_ne!(digest_a, 0);

    let mut same_order = ModSpineHost::new();
    same_order
        .content_registries_mut()
        .register_block("create", "create:copper_pipe")
        .unwrap();
    same_order
        .content_registries_mut()
        .register_item("create", "create:wrench")
        .unwrap();
    same_order
        .content_registries_mut()
        .register_channel("fabric", "create:contraption_sync")
        .unwrap();
    assert_eq!(digest_a, same_order.content_registries().digest_crc32());

    let mut different = ModSpineHost::new();
    different
        .content_registries_mut()
        .register_block("create", "create:zinc_pipe")
        .unwrap();
    assert_ne!(digest_a, different.content_registries().digest_crc32());

    host.content_registries_mut().freeze_all();
    assert!(host
        .content_registries_mut()
        .register_block("late", "late:block")
        .is_err());
}

#[test]
fn mod_ui_manifest_validation_registration_and_layer_revisions() {
    let mut host = ModSpineHost::new();
    let mut hud = ModUiManifest::new("wasm_hud", UiExtensionSlot::HudTopRight);
    hud.push_widget(labelled_widget("power"));
    assert!(audit_manifest(&hud, 854, 480, 0).is_within_budget());
    assert!(UiExtensionSlot::ALL
        .iter()
        .all(|slot| !slot.overlays_title_screen()));

    let initial = host.ui_layer_bus().rev(UiLayer::ModHud.index());
    let registry_id = host.register_mod_ui(hud).expect("valid mod UI registers");
    assert_eq!(registry_id, RegistryId(0));
    let first = host.ui_layer_bus().rev(UiLayer::ModHud.index());
    assert_ne!(initial, first);

    let dirty = host
        .mark_mod_ui_dirty("wasm_hud", UiLayer::ModHud)
        .expect("registered HUD can invalidate");
    assert!(dirty > first);
    assert!(host.ui_layer_bus().is_flagged_dirty(UiLayer::ModHud.index()));
    assert_eq!(host.mark_mod_ui_dirty("wasm_hud", UiLayer::StaticHud), None);
    assert_eq!(host.mark_mod_ui_dirty("missing_mod", UiLayer::ModHud), None);

    let mut invalid = ModUiManifest::new("BadCase", UiExtensionSlot::HudTopLeft);
    invalid.push_widget(labelled_widget("bad"));
    assert!(host.register_mod_ui(invalid).is_err());
}

#[test]
fn timing_wheel_delayed_mod_callbacks_are_ordered_and_cancellable() {
    let mut host = ModSpineHost::new();
    let fired = Rc::new(Cell::new(0u32));
    let cancel_fired = Rc::new(Cell::new(false));

    let capture = Rc::clone(&fired);
    host.schedule_mod_callback(2, move |host| {
        capture.set(capture.get() + host.pending_wakes() as u32 + 1);
    });
    let cancelled = host.schedule_mod_callback(1, {
        let capture = Rc::clone(&cancel_fired);
        move |_| capture.set(true)
    });
    assert_eq!(host.pending_mod_timers(), 2);
    assert!(host.cancel_mod_callback(cancelled));

    assert_eq!(host.advance_mod_timers(), 0);
    assert_eq!(fired.get(), 0);
    assert_eq!(host.advance_mod_timers(), 1);
    assert_eq!(fired.get(), 1);
    assert!(!cancel_fired.get());
    assert_eq!(host.pending_mod_timers(), 0);
}

#[test]
fn union_find_component_store_and_bvh_cover_generic_mod_data() {
    let mut host = ModSpineHost::new();
    let pipe_a = host
        .network_components_mut()
        .add_node("neoforge:mekanism", ModNetworkNodeKind::Pipe);
    let pipe_b = host
        .network_components_mut()
        .add_node("neoforge:mekanism", ModNetworkNodeKind::Pipe);
    let belt = host
        .network_components_mut()
        .add_node("fabric:create", ModNetworkNodeKind::Belt);
    assert!(!host.network_components_mut().connected(pipe_a, pipe_b));
    assert!(host.network_components_mut().connect_nodes(pipe_a, pipe_b));
    assert!(host.network_components_mut().connected(pipe_a, pipe_b));
    assert!(!host.network_components_mut().connected(pipe_a, belt));
    assert_eq!(host.network_components_mut().component_size(pipe_a), 2);
    assert_eq!(host.network_components().component_count(), 2);

    let target = host
        .component_store_mut()
        .track_target(ModAttachmentTarget::Entity { network_id: 77 });
    assert!(host.component_store_mut().attach_bytes(target, "energy", &[1, 2, 3]));
    assert_eq!(host.component_store().component(target, "energy"), Some(&[1, 2, 3][..]));
    assert!(host.component_store().interned_key("energy").is_some());
    assert_eq!(host.component_store_mut().remove_component(target, "energy"), Some(vec![1, 2, 3]));
    assert!(host.component_store_mut().untrack_target(target));
    assert!(!host.component_store_mut().attach_bytes(target, "energy", &[9]));

    let machine = host.spatial_index_mut().insert(
        Aabb3::new([0.0, 0.0, 0.0], [2.0, 2.0, 2.0]),
        "fabric:create",
        ModSpatialObjectKind::Contraption,
    );
    let far = host.spatial_index_mut().insert(
        Aabb3::new([20.0, 0.0, 0.0], [21.0, 1.0, 1.0]),
        "offload:zone",
        ModSpatialObjectKind::Zone,
    );
    let mut hits = Vec::new();
    host.spatial_index().query_aabb(
        Aabb3::new([1.0, 1.0, 1.0], [3.0, 3.0, 3.0]),
        &mut hits,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, machine);
    assert_eq!(hits[0].1.kind, ModSpatialObjectKind::Contraption);
    hits.clear();
    host.spatial_index().query_aabb(
        Aabb3::new([20.5, 0.5, 0.5], [20.75, 0.75, 0.75]),
        &mut hits,
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, far);
}

#[test]
fn bloom_bitset_bounded_ring_and_spsc_wakes_cover_all_lane_ids() {
    let mut host = ModSpineHost::new();
    let watched_block = host
        .content_registries_mut()
        .register_block("create", "create:shaft")
        .unwrap();
    for lane in ["fabric:create", "neoforge:mekanism", "offload:factory", "wasm:hud"] {
        host.subscribe_block(lane, watched_block);
    }
    assert!(host.block_watch_maybe(watched_block));
    assert_eq!(host.block_watcher_count(watched_block), 4);

    let pos = BlockPos::new(8, 70, 8);
    let (section_rev, chunk_rev) = host.notify_block_state_changed(DimIndex::OVERWORLD, pos, watched_block);
    assert!(section_rev > 0);
    assert!(chunk_rev > 0);
    let section = pos.section_key(DimIndex::OVERWORLD);
    let mask = host.dirty_voxel_mask(section).expect("dirty voxel mask");
    assert!(mask.get(pos.local_index() as u16));
    assert_eq!(host.pending_wakes(), 4);
    let wakes = host.drain_wakes();
    assert_eq!(wakes.len(), 4);
    for lane in ["fabric:create", "neoforge:mekanism", "offload:factory", "wasm:hud"] {
        assert!(wakes.iter().any(|wake| wake.mod_id == lane
            && wake.watch == ModWatch::Block(watched_block)
            && wake.revision == section_rev));
    }
    assert!(!host.drain_wake_signals().is_empty());
    assert!(host.clear_dirty_voxels(section));
}

#[test]
fn merkle_snapshot_delta_and_lz4_cas_dedup_are_exact() {
    let before = SnapshotRecord {
        id: "snap-before".into(),
        seq: 1,
        trigger: SnapshotTrigger::Manual,
        created_unix: 10,
        files: vec![
            SnapshotFileEntry { path: "level.dat".into(), sha256: "00".repeat(32), len: 4 },
            SnapshotFileEntry { path: "region/r.0.0.mca".into(), sha256: "11".repeat(32), len: 8 },
        ],
        skipped: Vec::new(),
        total_bytes: 12,
    };
    let after = SnapshotRecord {
        id: "snap-after".into(),
        seq: 2,
        trigger: SnapshotTrigger::ModUpdated,
        created_unix: 20,
        files: vec![
            SnapshotFileEntry { path: "level.dat".into(), sha256: "22".repeat(32), len: 4 },
            SnapshotFileEntry { path: "mods/create.nbt".into(), sha256: "33".repeat(32), len: 3 },
        ],
        skipped: Vec::new(),
        total_bytes: 7,
    };
    let before_tree = snapshot_merkle_tree(&before).unwrap();
    let after_tree = snapshot_merkle_tree(&after).unwrap();
    assert_ne!(before_tree.root(), after_tree.root());
    let delta = diff_snapshot_records(&before, &after).unwrap();
    assert_eq!(delta.added, vec!["mods/create.nbt"]);
    assert_eq!(delta.removed, vec!["region/r.0.0.mca"]);
    assert_eq!(delta.changed, vec!["level.dat"]);

    let mut cas = SnapshotCasDeduper::new(4096);
    let raw = b"create create create deduplicated mod snapshot";
    let a = cas.put_blob(raw).unwrap();
    let b = cas.put_blob(raw).unwrap();
    assert_eq!(a, b);
    assert_eq!(cas.unique_blobs(), 1);
    assert_eq!(cas.get_blob(&a).unwrap().as_deref(), Some(&raw[..]));
    assert!(cas.put_blob(&vec![0u8; 4097]).is_err());
}

#[test]
fn world_time_machine_reports_merkle_shaped_snapshot_deltas() {
    let root = temp_dir("time-machine");
    let world = root.join("world");
    fs::create_dir_all(&world).unwrap();
    fs::write(world.join("level.dat"), b"before").unwrap();
    fs::create_dir_all(world.join("region")).unwrap();
    fs::write(world.join("region/r.0.0.mca"), b"region").unwrap();

    let mut machine = WorldTimeMachine::new(root.join("store"), TimeMachineConfig::default());
    let before = machine
        .create_snapshot(&world, SnapshotTrigger::Manual, 1)
        .unwrap();
    fs::write(world.join("level.dat"), b"after").unwrap();
    fs::remove_file(world.join("region/r.0.0.mca")).unwrap();
    fs::write(world.join("modded.nbt"), b"new").unwrap();
    let after = machine
        .create_snapshot(&world, SnapshotTrigger::ModUpdated, 2)
        .unwrap();
    let delta = machine.diff_snapshots(&before.id, &after.id).unwrap();
    assert_eq!(delta.added, vec!["modded.nbt"]);
    assert_eq!(delta.removed, vec!["region/r.0.0.mca"]);
    assert_eq!(delta.changed, vec!["level.dat"]);
    let _ = fs::remove_dir_all(root);
}

struct WasmUiHost {
    spine: Rc<RefCell<ModSpineHost>>,
    mod_id: String,
}

impl MeguHost for WasmUiHost {
    fn log(&mut self, _msg: &str) {}
    fn get_block(&mut self, _x: i32, _y: i32, _z: i32) -> i32 { 0 }
    fn set_block(&mut self, _x: i32, _y: i32, _z: i32, _state: i32) -> i32 { -1 }
    fn emit_chat(&mut self, _player_id: i32, _msg: &str) -> i32 { -1 }

    fn ui_mark_dirty(&mut self, layer: UiLayer) -> i32 {
        if self
            .spine
            .borrow_mut()
            .mark_mod_ui_dirty(&self.mod_id, layer)
            .is_some()
        {
            0
        } else {
            -1
        }
    }
}

fn wasm_ui_probe() -> Vec<u8> {
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let ui_ty: (&[u8], &[u8]) = (&[I32], &[I32]);
    module(&[
        types_sec(&[abi_ty, tick_ty, ui_ty]),
        import_funcs_sec(&[("megu_v1", "megu_ui_mark_dirty", 2)]),
        func_sec(&[0, 1]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 1),
            ("megu_on_tick", KIND_FUNC, 2),
        ]),
        code_sec(&[
            body(&[], &concat(&[&i32_const(1), &[0x0B]])),
            body(&[], &concat(&[&i32_const(UiLayer::ModHud.index() as i32), &call(0), &[0x0B]])),
        ]),
    ])
}

#[test]
fn wasm_ui_hostcall_marks_registered_mod_hud_dirty_on_shared_spine() {
    let spine = Rc::new(RefCell::new(ModSpineHost::new()));
    let mut hud = ModUiManifest::new("wasm_hud", UiExtensionSlot::HudTopRight);
    hud.push_widget(labelled_widget("wasm"));
    spine.borrow_mut().register_mod_ui(hud).unwrap();
    let before = spine.borrow().ui_layer_bus().rev(UiLayer::ModHud.index());

    let host = WasmUiHost {
        spine: Rc::clone(&spine),
        mod_id: "wasm_hud".to_string(),
    };
    let mut guest = WasmMod::from_wasm(
        &wasm_ui_probe(),
        Box::new(host),
        CapabilitySet::all(),
        ModConfig::default(),
    )
    .expect("UI probe guest loads");
    assert_eq!(guest.on_tick(1).unwrap(), 0);
    let after = spine.borrow().ui_layer_bus().rev(UiLayer::ModHud.index());
    assert!(after > before);
    assert!(after > Revision::NONE);
}
