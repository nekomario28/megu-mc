// SPDX-License-Identifier: LGPL-3.0-or-later

//! Generic mod-to-core spine bridge tests: every mod lane (Fabric bridge,
//! NeoForge bridge, datapack, mod-offload factory network, Rust/WASM
//! guest) shares ONE `ModSpineHost` — one `ModDimRegistry`, one canonical
//! `SharedSectionRevTable`, one subscription/wake surface — with zero
//! hardcoded Overworld assumptions and zero duplicated coordinate or
//! codec primitives.

use std::cell::RefCell;
use std::rc::Rc;

use megu_core::coords::{BlockPos, ChunkKey, DimIndex, SectionKey};
use megu_core::section_rev::SharedSectionRevTable;

use mod_adapter_core::spine_bridge::{
    decode_mod_state_lz4, encode_mod_state_lz4, ModDimRegistry, ModEventSink,
    ModSpineHost, ModStateError, ModWatch, FIRST_MODDED_DIM,
};

const OW: DimIndex = DimIndex::OVERWORLD;

fn sec(dim: DimIndex, cx: i32, sy: i32, cz: i32) -> SectionKey {
    SectionKey { dim, cx, sy, cz }
}

fn chk(dim: DimIndex, cx: i32, cz: i32) -> ChunkKey {
    ChunkKey { dim, cx, cz }
}

// ---------------------------------------------------------------------------
// ModDimRegistry
// ---------------------------------------------------------------------------

#[test]
fn registry_pins_vanilla_and_assigns_dense_modded_indices() {
    let mut r = ModDimRegistry::new();
    assert_eq!(r.index_of("minecraft:overworld"), Some(OW));
    assert_eq!(r.index_of("minecraft:the_nether"), Some(DimIndex::NETHER));
    assert_eq!(r.index_of("minecraft:the_end"), Some(DimIndex::END));
    // Namespace-less spellings normalise into `minecraft:` like vanilla.
    assert_eq!(r.index_of("overworld"), Some(OW));
    assert_eq!(r.name_of(OW), Some("minecraft:overworld"));

    // Modded dimensions take dense indices from FIRST_MODDED_DIM upward
    // in registration order.
    let twilight = r.resolve_or_register("twilightforest:twilight_forest").unwrap();
    let aether = r.resolve_or_register("aether:the_aether").unwrap();
    assert_eq!(twilight, DimIndex(FIRST_MODDED_DIM));
    assert_eq!(aether, DimIndex(FIRST_MODDED_DIM + 1));
    // Re-resolution is stable, not re-assigned.
    assert_eq!(r.resolve_or_register("twilightforest:twilight_forest"), Ok(twilight));
    assert_eq!(r.name_of(aether), Some("aether:the_aether"));
    assert_eq!(r.registered_count(), 5);
}

#[test]
fn registry_rejects_malformed_names_without_registering() {
    let mut r = ModDimRegistry::new();
    assert!(r.resolve_or_register("a:b:c").is_err());
    assert!(r.resolve_or_register("bad ns:dim").is_err());
    assert!(r.resolve_or_register("mod:../escape").is_err());
    assert_eq!(r.index_of("a:b:c"), None);
    assert_eq!(r.registered_count(), 3);
    assert!(!r.is_registered("mod:../escape"));
}

// ---------------------------------------------------------------------------
// One shared spine across every mod lane
// ---------------------------------------------------------------------------

/// All four mod lanes — a Fabric-bridged mod, a NeoForge-bridged mod, a
/// mod-offload factory network and a WASM lane (sharing the table via the
/// `Rc<RefCell>` handle the WASM `SpineRevHost` consumes) — read and bump
/// ONE canonical revision table.
#[test]
fn all_lanes_share_one_spine_host() {
    let table = Rc::new(RefCell::new(SharedSectionRevTable::new()));
    let mut host = ModSpineHost::with_rev_table(Rc::clone(&table));

    let fabric_key = sec(OW, 0, 4, 0);
    let neoforge_key = sec(OW, 0, 4, 0);
    let offload_key = chk(OW, 0, 0);
    host.subscribe_section("fabric:create", fabric_key);
    host.subscribe_section("neoforge:mekanism", neoforge_key);
    host.subscribe_chunk("offload:factory-net-3", offload_key);
    // The "WASM lane" is any holder of the shared handle — identical to
    // what `SpineRevHost::shared_table` hands a guest's host.
    let wasm_lane_table = host.shared_rev_table();

    // A vanilla mutation through the generic sink trait wakes all three.
    let sink: &mut dyn ModEventSink = &mut host;
    let (srev, crev) = sink.notify_block_changed(OW, BlockPos::new(8, 70, 8));
    assert_eq!((srev, crev), (1, 1));

    let wakes = host.drain_wakes();
    let mods: Vec<&str> = wakes.iter().map(|w| w.mod_id.as_str()).collect();
    assert_eq!(mods.len(), 3);
    assert!(mods.contains(&"fabric:create"));
    assert!(mods.contains(&"neoforge:mekanism"));
    assert!(mods.contains(&"offload:factory-net-3"));

    // The WASM lane's table IS the same table: identical revisions.
    assert_eq!(wasm_lane_table.borrow().section_rev(fabric_key), 1);
    assert_eq!(table.borrow().chunk_rev(offload_key), 1);
}

/// Registering a modded dimension and arbitrary world heights works
/// identically for every lane — no hardcoded Overworld/`sy` clamps.
#[test]
fn modded_dimension_and_custom_heights_key_generically() {
    let mut host = ModSpineHost::new();
    let twilight = host
        .dims_mut()
        .resolve_or_register("twilightforest:twilight_forest")
        .unwrap();
    assert_eq!(twilight, DimIndex(FIRST_MODDED_DIM));

    // Deep-cave and sky-tower heights a vanilla `i8` section index could
    // never hold: y = -256 -> sy -16, y = 1024 -> sy 64.
    host.subscribe_section("neoforge:deep_mob", sec(twilight, 0, -16, 0));
    host.subscribe_section("fabric:sky_factory", sec(twilight, 0, 64, 0));

    host.notify_block_changed_named(
        "twilightforest:twilight_forest",
        BlockPos::new(3, -255, 3),
    )
    .unwrap();
    host.notify_block_changed(twilight, BlockPos::new(3, 1024, 3));

    let deep = sec(twilight, 0, -16, 0);
    let sky = sec(twilight, 0, 64, 0);
    assert_eq!(host.section_rev(deep), 1);
    assert_eq!(host.section_rev(sky), 1);
    // Vanilla dims were not touched.
    assert_eq!(host.section_rev(sec(OW, 0, -16, 0)), 0);

    let wakes = host.drain_wakes();
    assert_eq!(wakes.len(), 2);
    assert!(wakes.iter().any(|w| w.mod_id == "neoforge:deep_mob" && w.watch == ModWatch::Section(deep)));
    assert!(wakes.iter().any(|w| w.mod_id == "fabric:sky_factory" && w.watch == ModWatch::Section(sky)));
}

/// A border edit from one lane invalidates the adjacent section watched
/// by another lane — the canonical border-propagation rule, zero polling.
#[test]
fn border_edit_wakes_neighbour_section_watcher() {
    let mut host = ModSpineHost::new();
    // A NeoForge cable mod renders/caches the neighbour section (1,4,0).
    host.subscribe_section("neoforge:cable_net", sec(OW, 1, 4, 0));
    // A WASM-style watcher on the edited section itself.
    host.subscribe_section("wasm:counter", sec(OW, 0, 4, 0));
    // An uninterested mod on an untouched section must NOT wake.
    host.subscribe_section("fabric:far_away", sec(OW, 40, 4, 40));

    // Vanilla (or any lane) edits block (15, 64, 0): x=15 sits on the
    // high-X boundary, so the edit leaks into section (1,4,0).
    host.notify_block_changed(OW, BlockPos::new(15, 64, 0));

    assert_eq!(host.section_rev(sec(OW, 0, 4, 0)), 1);
    assert_eq!(host.section_rev(sec(OW, 1, 4, 0)), 1);

    let wakes = host.drain_wakes();
    assert_eq!(wakes.len(), 2, "unrelated mods must stay asleep: {wakes:?}");
    assert!(wakes.iter().any(|w| w.mod_id == "neoforge:cable_net"
        && w.watch == ModWatch::Section(sec(OW, 1, 4, 0))
        && w.revision == 1));
    assert!(wakes.iter().any(|w| w.mod_id == "wasm:counter"
        && w.watch == ModWatch::Section(sec(OW, 0, 4, 0))));
}

/// Corner edits propagate to every touched section *and* their parent
/// chunks — chunk watchers see the aggregate invalidation too.
#[test]
fn corner_edit_wakes_section_and_chunk_subscribers() {
    let mut host = ModSpineHost::new();
    // Block (15,64,15) is on the +X and +Z boundary: bumps sections
    // (0,4,0),(1,4,0),(0,4,1),(1,4,1) and chunks (0,0),(1,0),(0,1),(1,1).
    host.subscribe_section("offload:quad_mesh", sec(OW, 1, 4, 1));
    host.subscribe_chunk("fabric:column_cache", chk(OW, 0, 1));

    host.notify_block_changed(OW, BlockPos::new(15, 64, 15));

    let wakes = host.drain_wakes();
    assert!(wakes.iter().any(|w| w.mod_id == "offload:quad_mesh"
        && w.watch == ModWatch::Section(sec(OW, 1, 4, 1))));
    assert!(wakes.iter().any(|w| w.mod_id == "fabric:column_cache"
        && w.watch == ModWatch::Chunk(chk(OW, 0, 1))));
}

/// Interior edits stay local: no border neighbours are bumped or woken.
#[test]
fn interior_edit_does_not_wake_neighbours() {
    let mut host = ModSpineHost::new();
    host.subscribe_section("neoforge:west_wall", sec(OW, 1, 4, 0));
    // (8,72,8) is interior on all three axes: no border propagation.
    host.notify_block_changed(OW, BlockPos::new(8, 72, 8));
    assert_eq!(host.section_rev(sec(OW, 1, 4, 0)), 0);
    assert!(host.drain_wakes().is_empty());
}

// ---------------------------------------------------------------------------
// Subscriptions lifecycle
// ---------------------------------------------------------------------------

#[test]
fn unsubscribe_and_per_mod_drain_behave() {
    let mut host = ModSpineHost::new();
    let a = host.subscribe_section("fabric:a", sec(OW, 0, 0, 0));
    host.subscribe_section("neoforge:b", sec(OW, 0, 0, 0));
    assert_eq!(host.subscription_count(), 2);
    assert!(host.unsubscribe(a));
    assert!(!host.unsubscribe(a));

    host.notify_block_changed(OW, BlockPos::new(1, 1, 1));
    // Only neoforge:b woke; drain_wakes_for leaves other mods' events.
    host.subscribe_section("fabric:a", sec(OW, 0, 0, 0));
    host.notify_block_changed(OW, BlockPos::new(2, 2, 2));
    let mine = host.drain_wakes_for("fabric:a");
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].mod_id, "fabric:a");
    let rest = host.drain_wakes();
    assert!(rest.iter().all(|w| w.mod_id == "neoforge:b"));

    assert_eq!(host.unsubscribe_mod("neoforge:b"), 1);
    assert_eq!(host.subscriptions_of("neoforge:b"), vec![]);
}

#[test]
fn named_notify_registers_dimension_then_bumps() {
    let mut host = ModSpineHost::new();
    let (srev, _crev) = host
        .notify_block_changed_named("aether:the_aether", BlockPos::new(0, 320, 0))
        .unwrap();
    assert_eq!(srev, 1);
    let dim = host.dims().index_of("aether:the_aether").unwrap();
    assert_eq!(dim, DimIndex(FIRST_MODDED_DIM));
    // Invalid names fail closed: nothing bumped, nothing registered.
    assert!(host
        .notify_block_changed_named("bad:name:here", BlockPos::new(0, 0, 0))
        .is_err());
    assert_eq!(host.dims().registered_count(), 4);
}

// ---------------------------------------------------------------------------
// Generic mod-state LZ4+CRC32 codec
// ---------------------------------------------------------------------------

#[test]
fn mod_state_roundtrip_preserves_payload() {
    for state in [
        b"".as_slice(),
        b"a single byte of mod config".as_slice(),
        &vec![0xABu8; 100_000],
        &(0..=255u8).cycle().take(70_001).collect::<Vec<u8>>(),
    ] {
        let blob = encode_mod_state_lz4(state);
        assert_eq!(&blob[..4], b"MST1");
        assert_eq!(decode_mod_state_lz4(&blob, state.len().max(1)).unwrap(), state);
    }
}

#[test]
fn mod_state_rejects_tampered_and_oversized_blobs() {
    let state = b"persistent create-network state v3".as_slice();
    let blob = encode_mod_state_lz4(state);

    // Truncated header / payload.
    assert_eq!(decode_mod_state_lz4(&blob[..8], 1024), Err(ModStateError::Truncated));
    assert!(decode_mod_state_lz4(&blob[..blob.len() - 2], 1024).is_err());
    // Bad magic.
    let mut bad = blob.clone();
    bad[0] = b'X';
    assert_eq!(decode_mod_state_lz4(&bad, 1024), Err(ModStateError::BadMagic));
    // Tampered stored CRC.
    let mut bad = blob.clone();
    bad[4] ^= 0xFF;
    assert!(matches!(
        decode_mod_state_lz4(&bad, 1024),
        Err(ModStateError::CrcMismatch { .. })
    ));
    // Tampered compressed payload — rejected by LZ4 or CRC, never a
    // silent wrong decode.
    let mut bad = blob.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert!(decode_mod_state_lz4(&bad, 1024).is_err());
    // Declared-length amplification beyond the caller bound.
    let mut bad = blob.clone();
    bad[8..12].copy_from_slice(&(u32::MAX).to_le_bytes());
    assert_eq!(
        decode_mod_state_lz4(&bad, 1024),
        Err(ModStateError::DeclaredTooLarge {
            declared: u64::from(u32::MAX),
            max: 1024,
        })
    );
    // Legit blob past the caller's own bound also fails closed.
    assert!(matches!(
        decode_mod_state_lz4(&blob, 4),
        Err(ModStateError::DeclaredTooLarge { .. })
    ));
}
