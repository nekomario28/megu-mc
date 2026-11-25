// SPDX-License-Identifier: LGPL-3.0-or-later

//! Canonical-spine hostcall tests: real `.wasm` guests driving
//! `megu_section_rev` / `megu_chunk_rev` / `megu_bump_block` through a
//! [`SpineRevHost`] wired onto ONE shared
//! `megu_core::section_rev::SharedSectionRevTable` — proving a WASM mod
//! reads and bumps the exact same canonical revisions every other mod
//! lane observes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use megu_core::coords::{ChunkKey, DimIndex, SectionKey};
use megu_core::rev::UiLayer;
use megu_core::section_rev::SharedSectionRevTable;
use megu_wasm_mod_runtime::testutil::*;
use megu_wasm_mod_runtime::{
    Capability, CapabilitySet, LoadError, MeguHost, ModConfig, ModFault, SpineRevHost,
    Trap, WasmMod,
};

// ------------------------------------------------------------------ mock --

#[derive(Default)]
struct StubHost {
    logs: Vec<String>,
    blocks: BTreeMap<(i32, i32, i32), i32>,
}

impl MeguHost for StubHost {
    fn log(&mut self, msg: &str) {
        self.logs.push(msg.to_string());
    }
    fn get_block(&mut self, _x: i32, _y: i32, _z: i32) -> i32 {
        0
    }
    fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> i32 {
        self.blocks.insert((x, y, z), state);
        0
    }
    fn emit_chat(&mut self, _player_id: i32, _msg: &str) -> i32 {
        0
    }
}

// -------------------------------------------------------- module builder --

// Func indices after the three spine imports.
const F_ABI: u32 = 3;
const F_ON_TICK: u32 = 4;

/// A spine-probing mod: every `megu_on_tick` bumps the border block
/// `(15, 64, 0)` in the overworld and returns
/// `wrap(bump_rev) + 10*wrap(chunk_rev(0,0,0)) + 1000*wrap(section_rev(0,5,0))`
/// so the host can verify all three hostcalls from one status word.
fn spine_probe_mod() -> Vec<u8> {
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let spine4_ty: (&[u8], &[u8]) = (&[I32, I32, I32, I32], &[I64]);
    let spine3_ty: (&[u8], &[u8]) = (&[I32, I32, I32], &[I64]);

    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    let tick_body = body(
        &[],
        &concat(&[
            // megu_bump_block(dim=0, x=15, y=64, z=0) -> i64 -> wrap
            &i32_const(0),
            &i32_const(15),
            &i32_const(64),
            &i32_const(0),
            &call(0),
            &[0xA7], // i32.wrap_i64
            // + 10 * wrap(megu_chunk_rev(dim=0, cx=0, cz=0))
            &i32_const(0),
            &i32_const(0),
            &i32_const(0),
            &call(2),
            &[0xA7],
            &i32_const(10),
            &[0x6C], // i32.mul
            &[0x6A], // i32.add
            // + 1000 * wrap(megu_section_rev(dim=0, cx=0, sy=5, cz=0))
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
            ("megu_abi_version", KIND_FUNC, F_ABI),
            ("megu_on_tick", KIND_FUNC, F_ON_TICK),
            ("memory", KIND_MEM, 0),
        ]),
        code_sec(&[abi_body, tick_body]),
    ])
}

fn load_on_table(
    table: &Rc<RefCell<SharedSectionRevTable>>,
    caps: CapabilitySet,
) -> Result<WasmMod, ModFault> {
    WasmMod::from_wasm(
        &spine_probe_mod(),
        Box::new(SpineRevHost::new(StubHost::default(), Rc::clone(table))),
        caps,
        ModConfig::default(),
    )
}

// -------------------------------------------------------------- tests ----

#[test]
fn guest_and_host_share_one_revision_sequence() {
    let table = Rc::new(RefCell::new(SharedSectionRevTable::new()));
    let mut m = load_on_table(&table, CapabilitySet::all()).expect("loads");

    // Tick 1: guest bumps (15,64,0). Own section (0,4,0) -> rev 1.
    // y=64 and z=0 are boundary coordinates, so the canonical bump also
    // touches neighbour sections (0,3,0) and (0,4,-1); (0,3,0) shares
    // the guest's chunk column, so chunk_rev(0,0) lands at 2 — the
    // canonical table's strict upper-bound invalidator, not an edit
    // count. Status = 1 + 10*2 = 21.
    assert_eq!(m.on_tick(1).unwrap(), 1 + 20);
    {
        let t = table.borrow();
        let own = SectionKey {
            dim: DimIndex::OVERWORLD,
            cx: 0,
            sy: 4,
            cz: 0,
        };
        // The border edit leaks into (1,4,0) and chunk (1,0) — the host
        // sees exactly what the canonical bump records.
        assert_eq!(t.section_rev(own), 1);
        assert_eq!(t.section_rev(SectionKey { cx: 1, ..own }), 1);
        assert_eq!(
            t.chunk_rev(ChunkKey {
                dim: DimIndex::OVERWORLD,
                cx: 0,
                cz: 0
            }),
            2
        );
    }

    // A non-WASM lane bumps a fully interior block in section (0,5,0):
    // the SAME canonical table, straight from the host side.
    table
        .borrow_mut()
        .bump_block(DimIndex::OVERWORLD, 8, 88, 8);

    // Tick 2: bump -> own section rev 2; chunk (0,0) -> 2+1(host)+2 =
    // 5; the guest's megu_section_rev read sees the host-side edit
    // (section rev 1). Status = 2 + 50 + 1000 = 1052.
    assert_eq!(m.on_tick(2).unwrap(), 2 + 50 + 1000);
}

#[test]
fn guest_bump_in_modded_dimension_uses_open_dim_axis() {
    let table = Rc::new(RefCell::new(SharedSectionRevTable::new()));
    // Any u32 dim index is legal: `3` is what a ModDimRegistry assigns
    // the first modded dimension. The guest calls megu_bump_block(3, 0,
    // -255, 0) — a section below vanilla's bedrock floor.
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let spine4_ty: (&[u8], &[u8]) = (&[I32, I32, I32, I32], &[I64]);
    let deep_bump = body(
        &[],
        &concat(&[
            &i32_const(3),
            &i32_const(0),
            &i32_const(-255),
            &i32_const(0),
            &call(0),
            &[0xA7],
            &[0x0B],
        ]),
    );
    let bytes = module(&[
        types_sec(&[abi_ty, tick_ty, spine4_ty]),
        import_funcs_sec(&[("megu_v1", "megu_bump_block", 2)]),
        func_sec(&[0, 1]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 1),
            ("megu_on_tick", KIND_FUNC, 2),
        ]),
        code_sec(&[body(&[], &concat(&[&i32_const(1), &[0x0B]])), deep_bump]),
    ]);
    let mut m = WasmMod::from_wasm(
        &bytes,
        Box::new(SpineRevHost::new(StubHost::default(), Rc::clone(&table))),
        CapabilitySet::all(),
        ModConfig::default(),
    )
    .expect("loads");
    // y = -255 -> sy = -16 in modded dim 3.
    assert_eq!(m.on_tick(0).unwrap(), 1);
    let t = table.borrow();
    assert_eq!(
        t.section_rev(SectionKey {
            dim: DimIndex(3),
            cx: 0,
            sy: -16,
            cz: 0
        }),
        1
    );
    assert_eq!(t.section_rev(SectionKey {
        dim: DimIndex::OVERWORLD,
        cx: 0,
        sy: -16,
        cz: 0
    }), 0);
}

#[test]
fn spine_imports_gate_on_capabilities() {
    // write_blocks missing -> megu_bump_block traps and quarantines.
    let table = Rc::new(RefCell::new(SharedSectionRevTable::new()));
    let caps = CapabilitySet::NONE.granted(Capability::ReadBlocks);
    let mut m = load_on_table(&table, caps).unwrap();
    match m.on_tick(1) {
        Err(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::WriteBlocks,
            import: "megu_bump_block",
        })) => {}
        other => panic!("expected bump denial, got {other:?}"),
    }
    assert!(m.quarantined().is_some());
    // The denied bump never reached the canonical table.
    assert_eq!(table.borrow().tracked_sections(), 0);
    assert_eq!(table.borrow().tracked_chunks(), 0);
}

#[test]
fn read_gated_rev_imports_deny_without_read_blocks() {
    // write_blocks present, read_blocks missing: the bump itself lands,
    // then megu_chunk_rev traps.
    let table = Rc::new(RefCell::new(SharedSectionRevTable::new()));
    let caps = CapabilitySet::NONE.granted(Capability::WriteBlocks);
    let mut m = load_on_table(&table, caps).unwrap();
    match m.on_tick(1) {
        Err(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::ReadBlocks,
            import: "megu_chunk_rev",
        })) => {}
        other => panic!("expected chunk_rev denial, got {other:?}"),
    }
}

#[test]
fn missigned_spine_import_fails_at_load() {
    // megu_section_rev declared as () -> i64: rejected at instantiation.
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let bad_ty: (&[u8], &[u8]) = (&[], &[I64]);
    let bytes = module(&[
        types_sec(&[abi_ty, bad_ty]),
        import_funcs_sec(&[("env", "megu_section_rev", 1)]),
        func_sec(&[0]),
        export_sec(&[("megu_abi_version", KIND_FUNC, 1)]),
        code_sec(&[body(&[], &concat(&[&i32_const(1), &[0x0B]]))]),
    ]);
    match WasmMod::from_wasm(
        &bytes,
        Box::new(StubHost::default()),
        CapabilitySet::all(),
        ModConfig::default(),
    ) {
        Err(ModFault::Load(LoadError::SignatureMismatch(_))) => {}
        Err(e) => panic!("expected SignatureMismatch, got {e:?}"),
        Ok(_) => panic!("expected SignatureMismatch, module loaded"),
    }
}

#[test]
fn host_without_spine_reads_zero() {
    // A plain MeguHost (no SpineRevHost wiring) keeps the default `None`
    // spine hooks: rev reads see 0 and a bump is a no-op returning 0.
    let mut m = WasmMod::from_wasm(
        &spine_probe_mod(),
        Box::new(StubHost::default()),
        CapabilitySet::all(),
        ModConfig::default(),
    )
    .expect("loads");
    assert_eq!(m.on_tick(1).unwrap(), 0);
    assert_eq!(m.on_tick(2).unwrap(), 0);
}

#[test]
fn shared_table_handle_round_trips() {
    let table = Rc::new(RefCell::new(SharedSectionRevTable::new()));
    let host = SpineRevHost::new(StubHost::default(), Rc::clone(&table));
    let shared = host.shared_table();
    shared
        .borrow_mut()
        .bump_block(DimIndex::OVERWORLD, 0, 0, 0);
    assert_eq!(
        table.borrow().section_rev(SectionKey::from_block_pos(
            DimIndex::OVERWORLD,
            0,
            0,
            0
        )),
        1
    );
}


#[derive(Default)]
struct UiRecordingHost {
    inner: StubHost,
    calls: Vec<UiLayer>,
}

impl MeguHost for UiRecordingHost {
    fn log(&mut self, msg: &str) {
        self.inner.log(msg);
    }
    fn get_block(&mut self, x: i32, y: i32, z: i32) -> i32 {
        self.inner.get_block(x, y, z)
    }
    fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> i32 {
        self.inner.set_block(x, y, z, state)
    }
    fn emit_chat(&mut self, player_id: i32, msg: &str) -> i32 {
        self.inner.emit_chat(player_id, msg)
    }
    fn ui_mark_dirty(&mut self, layer: UiLayer) -> i32 {
        self.calls.push(layer);
        0
    }
}

fn ui_probe_mod() -> Vec<u8> {
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
            // valid ModOverlay status plus 10 * invalid vanilla-layer status;
            // expected 0 + 10 * -1 = -10.
            body(
                &[],
                &concat(&[
                    &i32_const(6),
                    &call(0),
                    &i32_const(3),
                    &call(0),
                    &i32_const(10),
                    &[0x6C],
                    &[0x6A],
                    &[0x0B],
                ]),
            ),
        ]),
    ])
}

#[test]
fn ui_mark_dirty_accepts_only_mod_owned_layers() {
    let host = UiRecordingHost::default();
    let mut m = WasmMod::from_wasm(
        &ui_probe_mod(),
        Box::new(host),
        CapabilitySet::all(),
        ModConfig::default(),
    )
    .expect("loads");
    assert_eq!(m.on_tick(1).unwrap(), -10);
}

#[test]
fn ui_mark_dirty_gates_on_ui_surface_capability() {
    let mut m = WasmMod::from_wasm(
        &ui_probe_mod(),
        Box::new(UiRecordingHost::default()),
        CapabilitySet::NONE,
        ModConfig::default(),
    )
    .expect("loads");
    match m.on_tick(1) {
        Err(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::UiSurface,
            import: "megu_ui_mark_dirty",
        })) => {}
        other => panic!("expected ui_surface denial, got {other:?}"),
    }
}
