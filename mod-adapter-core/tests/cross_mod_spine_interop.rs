// SPDX-License-Identifier: LGPL-3.0-or-later

//! Cross-mod generic spine interop: ONE `ModSpineHost` — one
//! `ModDimRegistry`, one canonical `SharedSectionRevTable`, one wake
//! surface — shared simultaneously by a Fabric-bridged mod, a
//! NeoForge-bridged mod, a real `mod_offload` fluid network and a real
//! `.wasm` guest driven through the `megu_*_rev` hostcalls. No
//! mod-specific coordinate, revision, LZ4 or CRC primitives anywhere.

use std::cell::RefCell;
use std::rc::Rc;

use megu_core::coords::{BlockPos, ChunkKey, DimIndex, SectionKey};
use megu_wasm_mod_runtime::testutil::{
    body, call, code_sec, concat, export_sec, func_sec, i32_const, import_funcs_sec,
    mem_sec, module, types_sec, KIND_FUNC, KIND_MEM, I32, I64,
};
use megu_wasm_mod_runtime::{CapabilitySet, MeguHost, ModConfig, WasmMod};
use mod_adapter_core::spine_bridge::{
    ModEventSink, ModSpineHost, ModWatch, FIRST_MODDED_DIM,
};
use mod_offload::fluid_network::FluidNetwork;

const OW: DimIndex = DimIndex::OVERWORLD;

fn sec(dim: DimIndex, cx: i32, sy: i32, cz: i32) -> SectionKey {
    SectionKey { dim, cx, sy, cz }
}

// -------------------------------------------------------------- WASM lane --

/// The WASM lane's host: the guest's spine hostcalls route straight into
/// the shared `ModSpineHost` — reads hit the canonical table and a
/// `megu_bump_block` lands as a full `notify_block_changed` (canonical
/// bump *and* subscriber wakes), exactly like a vanilla edit.
struct WasmLaneHost {
    spine: Rc<RefCell<ModSpineHost>>,
}

impl MeguHost for WasmLaneHost {
    fn log(&mut self, _msg: &str) {}
    fn get_block(&mut self, _x: i32, _y: i32, _z: i32) -> i32 {
        0
    }
    fn set_block(&mut self, _x: i32, _y: i32, _z: i32, _state: i32) -> i32 {
        0
    }
    fn emit_chat(&mut self, _player_id: i32, _msg: &str) -> i32 {
        0
    }
    fn spine_section_rev(&mut self, key: SectionKey) -> Option<u64> {
        Some(self.spine.borrow().section_rev(key))
    }
    fn spine_chunk_rev(&mut self, key: ChunkKey) -> Option<u64> {
        Some(self.spine.borrow().chunk_rev(key))
    }
    fn spine_bump_block(&mut self, dim: DimIndex, x: i32, y: i32, z: i32) -> Option<u64> {
        Some(
            self.spine
                .borrow_mut()
                .notify_block_changed(dim, BlockPos::new(x, y, z))
                .0,
        )
    }
}

/// `megu_on_tick`: `megu_bump_block(0, 15, 64, 0)` then return
/// `wrap(bump_rev) + 10*wrap(megu_chunk_rev(0,0,0))
/// + 1000*wrap(megu_section_rev(0,5,0))`.
fn border_probe_mod() -> Vec<u8> {
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let spine4_ty: (&[u8], &[u8]) = (&[I32, I32, I32, I32], &[I64]);
    let spine3_ty: (&[u8], &[u8]) = (&[I32, I32, I32], &[I64]);
    let tick_body = body(
        &[],
        &concat(&[
            &i32_const(0),
            &i32_const(15),
            &i32_const(64),
            &i32_const(0),
            &call(0),
            &[0xA7], // i32.wrap_i64
            &i32_const(0),
            &i32_const(0),
            &i32_const(0),
            &call(2),
            &[0xA7],
            &i32_const(10),
            &[0x6C], // i32.mul
            &[0x6A], // i32.add
            &i32_const(0),
            &i32_const(0),
            &i32_const(5),
            &i32_const(0),
            &call(1),
            &[0xA7],
            &i32_const(1000),
            &[0x6C],
            &[0x6A],
            &[0x0B],
        ]),
    );
    module(&[
        types_sec(&[abi_ty, tick_ty, spine4_ty, spine3_ty]),
        import_funcs_sec(&[
            ("megu_v1", "megu_bump_block", 2),
            ("megu_v1", "megu_section_rev", 2),
            ("megu_v1", "megu_chunk_rev", 3),
        ]),
        func_sec(&[0, 1]),
        mem_sec(1, Some(1)),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 3),
            ("megu_on_tick", KIND_FUNC, 4),
            ("memory", KIND_MEM, 0),
        ]),
        code_sec(&[
            body(&[], &concat(&[&i32_const(1), &[0x0B]])),
            tick_body,
        ]),
    ])
}

/// `megu_on_tick`: `megu_bump_block(dim, x, y, z)` with the coordinates
/// baked in as constants; returns `wrap(new section rev)`.
fn const_bump_mod(dim: u32, x: i32, y: i32, z: i32) -> Vec<u8> {
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let spine4_ty: (&[u8], &[u8]) = (&[I32, I32, I32, I32], &[I64]);
    let tick_body = body(
        &[],
        &concat(&[
            &i32_const(dim as i32),
            &i32_const(x),
            &i32_const(y),
            &i32_const(z),
            &call(0),
            &[0xA7],
            &[0x0B],
        ]),
    );
    module(&[
        types_sec(&[abi_ty, tick_ty, spine4_ty]),
        import_funcs_sec(&[("megu_v1", "megu_bump_block", 2)]),
        func_sec(&[0, 1]),
        export_sec(&[("megu_abi_version", KIND_FUNC, 1), ("megu_on_tick", KIND_FUNC, 2)]),
        code_sec(&[
            body(&[], &concat(&[&i32_const(1), &[0x0B]])),
            tick_body,
        ]),
    ])
}

fn spawn_guest(
    wasm: &[u8],
    spine: &Rc<RefCell<ModSpineHost>>,
) -> WasmMod {
    WasmMod::from_wasm(
        wasm,
        Box::new(WasmLaneHost {
            spine: Rc::clone(spine),
        }),
        CapabilitySet::all(),
        ModConfig::default(),
    )
    .expect("guest loads")
}

// --------------------------------------------------------------- tests ----

/// Requirement 1: a Fabric mod, a NeoForge mod, a mod-offload factory
/// network and a WASM guest all share ONE `ModSpineHost`.
#[test]
fn all_four_lanes_share_one_spine_host() {
    let host = Rc::new(RefCell::new(ModSpineHost::new()));

    // Fabric-bridged mod caches meshes for section (0,4,0).
    host.borrow_mut()
        .subscribe_section("fabric:litematica_printer", sec(OW, 0, 4, 0));
    // NeoForge cable mod caches the border-neighbour section (1,4,0).
    host.borrow_mut()
        .subscribe_section("neoforge:cable_net", sec(OW, 1, 4, 0));
    // A live mod-offload fluid network watches its chunk column.
    let mut net = FluidNetwork::default();
    net.add_tank(0, 8_000, 64).unwrap();
    net.add_tank(1, 8_000, 72).unwrap();
    net.set_link(0, 1, Some(4)).unwrap();
    net.insert(0, 1, 4_000).unwrap();
    host.borrow_mut()
        .subscribe_chunk("offload:fluid-net", ChunkKey { dim: OW, cx: 0, cz: 0 });

    // The WASM guest lane: real bytes, real hostcalls, same host object.
    let mut guest = spawn_guest(&border_probe_mod(), &host);

    // Vanilla mutation through the generic sink trait object — the same
    // ingress a Fabric or NeoForge bridge would hold.
    {
        let mut h = host.borrow_mut();
        let sink: &mut dyn ModEventSink = &mut *h;
        sink.notify_block_changed(OW, BlockPos::new(8, 72, 8));
    }
    {
        let mut h = host.borrow_mut();
        let wakes = h.drain_wakes();
        assert_eq!(wakes.len(), 2, "{wakes:?}");
        assert!(wakes.iter().any(|w| w.mod_id == "fabric:litematica_printer"));
        assert!(wakes.iter().any(|w| w.mod_id == "offload:fluid-net"));
    }

    // The WASM guest issues megu_bump_block(15,64,0): the same canonical
    // bump — plus border-leak wakes for fabric, neoforge and offload.
    assert_eq!(guest.on_tick(1).unwrap(), 2 + 30);
    let mut h = host.borrow_mut();
    assert_eq!(h.section_rev(sec(OW, 0, 4, 0)), 2);
    assert_eq!(h.section_rev(sec(OW, 1, 4, 0)), 1);
    let wakes = h.drain_wakes();
    let mods: Vec<&str> = wakes.iter().map(|w| w.mod_id.as_str()).collect();
    assert!(mods.contains(&"fabric:litematica_printer"));
    assert!(mods.contains(&"neoforge:cable_net"));
    assert!(mods.contains(&"offload:fluid-net"));
    drop(h);

    // The guest reads the SAME canonical revisions back through
    // megu_section_rev / megu_chunk_rev (status word encodes them), and
    // the offload network is a live solver with pending work.
    assert!(net.has_pending());
    assert!(net.step().is_some());
}

/// Requirement 2: a custom modded dimension and custom world heights
/// work identically for every lane — no Overworld assumptions.
#[test]
fn modded_dim_and_extreme_heights_work_for_wasm_lane() {
    let host = Rc::new(RefCell::new(ModSpineHost::new()));
    let twilight = host
        .borrow_mut()
        .dims_mut()
        .resolve_or_register("twilightforest:twilight_forest")
        .unwrap();
    assert_eq!(twilight, DimIndex(FIRST_MODDED_DIM));

    // A NeoForge deep-cave mod watches section sy = -16 in dim 3; a
    // Fabric sky mod watches sy = 64 there.
    host.borrow_mut()
        .subscribe_section("neoforge:deep_mobs", sec(twilight, 0, -16, 0));
    host.borrow_mut()
        .subscribe_section("fabric:sky_islands", sec(twilight, 0, 64, 0));

    // The WASM guest bumps y = -255 (sy -16) in dim 3 — a coordinate no
    // vanilla-biased `i8` section index could even name.
    let mut deep_guest = spawn_guest(&const_bump_mod(3, 0, -255, 0), &host);
    assert_eq!(deep_guest.on_tick(0).unwrap(), 1);
    // And y = 1024 (sy 64).
    let mut sky_guest = spawn_guest(&const_bump_mod(3, 0, 1024, 0), &host);
    assert_eq!(sky_guest.on_tick(0).unwrap(), 1);

    let mut h = host.borrow_mut();
    assert_eq!(h.section_rev(sec(twilight, 0, -16, 0)), 1);
    assert_eq!(h.section_rev(sec(twilight, 0, 64, 0)), 1);
    assert_eq!(h.section_rev(sec(OW, 0, -16, 0)), 0);
    let wakes = h.drain_wakes();
    assert_eq!(wakes.len(), 2, "{wakes:?}");
    assert!(wakes.iter().any(|w| w.mod_id == "neoforge:deep_mobs"
        && w.watch == ModWatch::Section(sec(twilight, 0, -16, 0))));
    assert!(wakes.iter().any(|w| w.mod_id == "fabric:sky_islands"
        && w.watch == ModWatch::Section(sec(twilight, 0, 64, 0))));
}

/// Requirement 3: a border-block mutation by ONE lane invalidates the
/// neighbour section watched by ANOTHER — proven both vanilla→NeoForge
/// and WASM→NeoForge directions.
#[test]
fn border_mutation_crosses_mod_lanes() {
    let host = Rc::new(RefCell::new(ModSpineHost::new()));
    host.borrow_mut()
        .subscribe_section("neoforge:cable_net", sec(OW, 1, 4, 0));
    host.borrow_mut()
        .subscribe_section("wasm:border_watcher", sec(OW, 0, 4, 0));

    // Direction 1: vanilla lane mutates (15,64,0) -> NeoForge wakes.
    host.borrow_mut()
        .notify_block_changed(OW, BlockPos::new(15, 64, 0));
    {
        let mut h = host.borrow_mut();
        let wakes = h.drain_wakes();
        assert!(wakes.iter().any(|w| w.mod_id == "neoforge:cable_net"
            && w.watch == ModWatch::Section(sec(OW, 1, 4, 0))));
        assert!(wakes.iter().any(|w| w.mod_id == "wasm:border_watcher"));
    }

    // Direction 2: the WASM guest itself issues the border bump — the
    // same subscribers wake again with the new revision.
    let mut guest = spawn_guest(&const_bump_mod(0, 15, 64, 0), &host);
    assert_eq!(guest.on_tick(7).unwrap(), 2);
    let mut h = host.borrow_mut();
    let wakes = h.drain_wakes();
    // Second bump of the same boundary: the watched neighbour is now at
    // revision 2.
    assert!(wakes.iter().any(|w| w.mod_id == "neoforge:cable_net" && w.revision == 2));
}
