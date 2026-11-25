// SPDX-License-Identifier: LGPL-3.0-or-later

//! End-to-end runtime tests driving real assembled `.wasm` binaries
//! through `WasmMod` — no external toolchain.

use std::collections::BTreeMap;

use megu_wasm_mod_runtime::testutil::*;
use megu_wasm_mod_runtime::{
    Capability, CapabilitySet, LoadError, MeguHost, ModConfig, ModFault, Trap, WasmMod,
};

// ---------------------------------------------------------------- mock --

#[derive(Default)]
struct MockHost {
    logs: Vec<String>,
    blocks: BTreeMap<(i32, i32, i32), i32>,
    chats: Vec<(i32, String)>,
    get_block_ret: i32,
}

impl MeguHost for MockHost {
    fn log(&mut self, msg: &str) {
        self.logs.push(msg.to_string());
    }
    fn get_block(&mut self, _x: i32, _y: i32, _z: i32) -> i32 {
        self.get_block_ret
    }
    fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> i32 {
        self.blocks.insert((x, y, z), state);
        0
    }
    fn emit_chat(&mut self, player_id: i32, msg: &str) -> i32 {
        self.chats.push((player_id, msg.to_string()));
        0
    }
}

fn all_caps() -> CapabilitySet {
    CapabilitySet::all()
}

// ------------------------------------------------------- module builder --

// Func indices after the four imports.
const F_ABI: u32 = 4;
const F_ON_LOAD: u32 = 5;
const F_ON_TICK: u32 = 6;
const F_ON_INTERACT: u32 = 7;

/// A realistic mod module: imports all four host functions, exports all
/// ABI entry points, keeps a mutable i32 counter global, and carries a
/// `"hello"` data segment at address 0.
fn counter_mod() -> Vec<u8> {
    let ty_void_i32: (&[u8], &[u8]) = (&[], &[I32]);
    let log_ty: (&[u8], &[u8]) = (&[I32, I32], &[]);
    let get_ty: (&[u8], &[u8]) = (&[I32, I32, I32], &[I32]);
    let set_ty: (&[u8], &[u8]) = (&[I32, I32, I32, I32], &[I32]);
    let chat_ty: (&[u8], &[u8]) = (&[I32, I32, I32], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let interact_ty: (&[u8], &[u8]) = (&[I32, I32, I32, I32], &[I32]);

    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    // on_load: host_log(0, 5) ("hello"), return 0.
    let on_load_body = body(
        &[],
        &concat(&[
            &i32_const(0),
            &i32_const(5),
            &call(0),
            &i32_const(0),
            &[0x0B],
        ]),
    );
    // on_tick(tick): counter += 1; set_block(0, tick as i32, 0, 7);
    // return counter.
    let on_tick_body = body(
        &[],
        &concat(&[
            &global_get(0),
            &i32_const(1),
            &[0x6A], // i32.add
            &global_set(0),
            &i32_const(0),
            &local_get(0),
            &[0xA7], // i32.wrap_i64
            &i32_const(0),
            &i32_const(7),
            &call(2), // host_set_block (import idx 2)
            &[0x1A],  // drop
            &global_get(0),
            &[0x0B],
        ]),
    );
    // on_block_interact(x,y,z,player): emit_chat(player, 0, 5);
    // return get_block(x, y, z).
    let interact_body = body(
        &[],
        &concat(&[
            &local_get(3),
            &i32_const(0),
            &i32_const(5),
            &call(3), // host_emit_chat (import idx 3)
            &[0x1A],
            &local_get(0),
            &local_get(1),
            &local_get(2),
            &call(1), // host_get_block
            &[0x0B],
        ]),
    );

    module(&[
        types_sec(&[
            ty_void_i32,
            log_ty,
            get_ty,
            set_ty,
            chat_ty,
            tick_ty,
            interact_ty,
        ]),
        import_funcs_sec(&[
            ("env", "host_log", 1),
            ("env", "host_get_block", 2),
            ("env", "host_set_block", 3),
            ("env", "host_emit_chat", 4),
        ]),
        func_sec(&[0, 0, 5, 6]),
        mem_sec(1, Some(4)),
        global_sec(&[(I32, true, i32_const(0))]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, F_ABI),
            ("megu_on_load", KIND_FUNC, F_ON_LOAD),
            ("megu_on_tick", KIND_FUNC, F_ON_TICK),
            ("megu_on_block_interact", KIND_FUNC, F_ON_INTERACT),
            ("memory", KIND_MEM, 0),
        ]),
        code_sec(&[abi_body, on_load_body, on_tick_body, interact_body]),
        data_sec(&[(0, 0, b"hello")]),
    ])
}

fn load(bytes: &[u8], caps: CapabilitySet) -> Result<WasmMod, ModFault> {
    WasmMod::from_wasm(
        bytes,
        Box::new(MockHost::default()),
        caps,
        ModConfig::default(),
    )
}

// -------------------------------------------------------------- tests --

#[test]
fn full_lifecycle_mutates_world() {
    let mut m = load(&counter_mod(), all_caps()).expect("loads");
    assert_eq!(m.abi_version(), 1);
    assert_eq!(m.on_load().unwrap(), 0);
    assert_eq!(m.on_tick(3).unwrap(), 1);
    assert_eq!(m.on_tick(5).unwrap(), 2);
    assert_eq!(m.on_block_interact(10, 64, -4, 42).unwrap(), 0);
    // Host state is captured inside the boxed host; re-drive a fresh
    // instance to assert on it below in `world_state_visible`.
}

/// Same lifecycle, but observable through a shared host — proves the
/// guest actually mutated world state.
#[test]
fn world_state_visible() {
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Shared(Rc<RefCell<MockHost>>);
    impl MeguHost for Shared {
        fn log(&mut self, msg: &str) {
            self.0.borrow_mut().logs.push(msg.to_string());
        }
        fn get_block(&mut self, x: i32, y: i32, z: i32) -> i32 {
            self.0.borrow_mut().get_block(x, y, z)
        }
        fn set_block(&mut self, x: i32, y: i32, z: i32, s: i32) -> i32 {
            self.0.borrow_mut().set_block(x, y, z, s)
        }
        fn emit_chat(&mut self, p: i32, m: &str) -> i32 {
            self.0.borrow_mut().emit_chat(p, m)
        }
    }

    let shared = Rc::new(RefCell::new(MockHost::default()));
    let mut m = WasmMod::from_wasm(
        &counter_mod(),
        Box::new(Shared(Rc::clone(&shared))),
        all_caps(),
        ModConfig::default(),
    )
    .unwrap();

    m.on_load().unwrap();
    m.on_tick(3).unwrap();
    m.on_tick(5).unwrap();
    shared.borrow_mut().get_block_ret = 99;
    let r = m.on_block_interact(10, 64, -4, 42).unwrap();

    let h = shared.borrow();
    assert_eq!(h.logs, vec!["hello".to_string()]);
    assert_eq!(h.blocks.get(&(0, 3, 0)), Some(&7));
    assert_eq!(h.blocks.get(&(0, 5, 0)), Some(&7));
    assert_eq!(h.chats, vec![(42, "hello".to_string())]);
    assert_eq!(r, 99);
}

#[test]
fn capability_denied_fails_closed() {
    // No write_blocks → host_set_block traps and quarantines.
    let caps = CapabilitySet::NONE.granted(Capability::ReadBlocks);
    let mut m = load(&counter_mod(), caps).unwrap();
    m.on_load().unwrap(); // log is ungated: fine
    match m.on_tick(1) {
        Err(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::WriteBlocks,
            import: "host_set_block",
        })) => {}
        other => panic!("expected CapabilityDenied, got {other:?}"),
    }
    assert!(m.quarantined().is_some());
    // Once quarantined, every entry point short-circuits.
    assert_eq!(m.on_tick(2), Err(ModFault::Quarantined));
    assert_eq!(m.on_load(), Err(ModFault::Quarantined));
    // And the block write never happened (guest trapped before effect).
}

#[test]
fn chat_capability_denied() {
    let caps = CapabilitySet::NONE
        .granted(Capability::ReadBlocks)
        .granted(Capability::WriteBlocks);
    let mut m = load(&counter_mod(), caps).unwrap();
    m.on_tick(0).unwrap(); // set_block fine
    match m.on_block_interact(0, 0, 0, 7) {
        Err(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::ChatBroadcast,
            ..
        })) => {}
        other => panic!("expected chat denial, got {other:?}"),
    }
}

#[test]
fn infinite_loop_traps_on_fuel() {
    // on_tick body: `loop br 0 end; unreachable; end` — the spec types
    // a loop that can't fall through as still-reachable after its `end`,
    // so the `unreachable` op keeps the (i64)->i32 signature valid.
    let ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let abi: (&[u8], &[u8]) = (&[], &[I32]);
    let spin = body(
        &[],
        &concat(&[
            &[0x03, 0x40], // loop (void)
            &[0x0C, 0x00], // br 0
            &[0x0B],       // end
            &[0x00],       // unreachable
            &[0x0B],       // func end
        ]),
    );
    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    let bytes = module(&[
        types_sec(&[abi, ty]),
        func_sec(&[0, 1]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 0),
            ("megu_on_tick", KIND_FUNC, 1),
        ]),
        code_sec(&[abi_body, spin]),
    ]);
    let mut m = load(&bytes, all_caps()).unwrap();
    let t0 = std::time::Instant::now();
    match m.on_tick(0) {
        Err(ModFault::Trap(Trap::FuelExhausted)) => {}
        other => panic!("expected FuelExhausted, got {other:?}"),
    }
    assert!(t0.elapsed().as_secs() < 5, "fuel trap took far too long");
    assert!(matches!(
        m.quarantined(),
        Some(ModFault::Trap(Trap::FuelExhausted))
    ));
}

#[test]
fn out_of_bounds_memory_traps() {
    // on_tick: i32.load at address 0x7FFF_FFFF → OOB.
    let ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let abi: (&[u8], &[u8]) = (&[], &[I32]);
    let body_bytes = body(&[], &concat(&[&i32_const(-1), &i32_load(), &[0x0B]]));
    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    let bytes = module(&[
        types_sec(&[abi, ty]),
        func_sec(&[0, 1]),
        mem_sec(1, None),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 0),
            ("megu_on_tick", KIND_FUNC, 1),
        ]),
        code_sec(&[abi_body, body_bytes]),
    ]);
    let mut m = load(&bytes, all_caps()).unwrap();
    match m.on_tick(0) {
        Err(ModFault::Trap(Trap::MemoryOutOfBounds { .. })) => {}
        other => panic!("expected MemoryOutOfBounds, got {other:?}"),
    }
}

#[test]
fn malformed_and_invalid_binaries_fail_closed() {
    // Not wasm at all.
    assert!(matches!(
        load(b"definitely not wasm", all_caps()),
        Err(ModFault::Load(LoadError::Malformed(_)))
    ));
    // Truncated section: declares 10 bytes, supplies 1.
    let mut bad = b"\0asm\x01\0\0\0".to_vec();
    bad.extend_from_slice(&[1, 10, 0x60]);
    assert!(matches!(load(&bad, all_caps()), Err(ModFault::Load(_))));
    // Valid module missing megu_abi_version.
    let no_abi = module(&[
        types_sec(&[(&[], &[I32])]),
        func_sec(&[0]),
        export_sec(&[("megu_on_load", KIND_FUNC, 0)]),
        code_sec(&[body(&[], &concat(&[&i32_const(0), &[0x0B]]))]),
    ]);
    assert!(matches!(
        load(&no_abi, all_caps()),
        Err(ModFault::Load(LoadError::MissingExport("megu_abi_version")))
    ));
    // abi_version with a wrong result type → signature mismatch.
    let bad_sig = module(&[
        types_sec(&[(&[], &[I64])]),
        func_sec(&[0]),
        export_sec(&[("megu_abi_version", KIND_FUNC, 0)]),
        code_sec(&[body(&[], &concat(&[&i64_const(1), &[0x0B]]))]),
    ]);
    assert!(matches!(
        load(&bad_sig, all_caps()),
        Err(ModFault::Load(LoadError::SignatureMismatch(_)))
    ));
    // Type error in a body: i32.add on an empty stack.
    let ty_bad = module(&[
        types_sec(&[(&[], &[I32])]),
        func_sec(&[0]),
        export_sec(&[("megu_abi_version", KIND_FUNC, 0)]),
        code_sec(&[body(&[], &[0x6A, 0x0B])]),
    ]);
    assert!(matches!(
        load(&ty_bad, all_caps()),
        Err(ModFault::Load(LoadError::Invalid(_)))
    ));
    // Wrong ABI version.
    let v2 = module(&[
        types_sec(&[(&[], &[I32])]),
        func_sec(&[0]),
        export_sec(&[("megu_abi_version", KIND_FUNC, 0)]),
        code_sec(&[body(&[], &concat(&[&i32_const(2), &[0x0B]]))]),
    ]);
    assert!(matches!(
        load(&v2, all_caps()),
        Err(ModFault::Load(LoadError::UnsupportedAbi(2)))
    ));
}

#[test]
fn jar_archive_loads_and_runs() {
    let wasm = counter_mod();
    let manifest = br#"schema = "megu-mod/1"

[mod]
id = "wasm_counter"
name = "Wasm Counter"
version = "0.1.0"
mc_version_range = "[1.21.1,)"
authors = ["Test"]
license = "MIT"
description = "test"

[bundle]
file = "megu-mod-bundle.json"

[capabilities]
read_blocks = true
write_blocks = true
chat_broadcast = true
"#;
    let jar = stored_zip(&[
        ("META-INF/megu/mod.wasm", &wasm),
        ("megu-mod.toml", &manifest[..]),
        ("megu-mod-bundle.json", b"{}"),
    ]);
    let mut m = WasmMod::from_archive(&jar, Box::new(MockHost::default()), ModConfig::default())
        .expect("jar loads");
    assert_eq!(
        m.capabilities(),
        CapabilitySet::NONE
            .granted(Capability::ReadBlocks)
            .granted(Capability::WriteBlocks)
            .granted(Capability::ChatBroadcast)
    );
    m.on_load().unwrap();
    assert_eq!(m.on_tick(9).unwrap(), 1);
}

#[test]
fn jar_without_wasm_entry_fails() {
    let jar = stored_zip(&[("megu-mod.toml", b"schema = \"megu-mod/1\"")]);
    assert!(matches!(
        WasmMod::from_archive(&jar, Box::new(MockHost::default()), ModConfig::default()),
        Err(ModFault::Load(LoadError::Archive(_)))
    ));
}

#[test]
fn jar_with_unknown_capability_fails_closed() {
    let wasm = counter_mod();
    let manifest = b"[capabilities]\nwrite_blocks = true\nfly_to_moon = true\n";
    let jar = stored_zip(&[
        ("META-INF/megu/mod.wasm", &wasm),
        ("megu-mod.toml", &manifest[..]),
    ]);
    assert!(matches!(
        WasmMod::from_archive(&jar, Box::new(MockHost::default()), ModConfig::default()),
        Err(ModFault::Load(LoadError::Archive(_)))
    ));
}

#[test]
fn bounded_loops_get_fresh_fuel_per_tick() {
    // on_tick: 5000-iteration loop, then return — well under 200k fuel.
    // Two consecutive ticks both succeed → fuel is per-call.
    let ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let abi: (&[u8], &[u8]) = (&[], &[I32]);
    // local 1 = i (i32); loop: i++; br_if loop while i < 5000.
    let spin = body(
        &[(1, I32)],
        &concat(&[
            &[0x02, 0x40], // block
            &[0x03, 0x40], // loop
            &local_get(1),
            &i32_const(1),
            &[0x6A], // i32.add
            &local_tee(1),
            &i32_const(5000),
            &[0x48],       // i32.lt_s
            &[0x0D, 0x00], // br_if 0 (loop)
            &[0x0B],       // end loop
            &[0x0B],       // end block
            &local_get(1),
            &[0x0B], // func end
        ]),
    );
    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    let bytes = module(&[
        types_sec(&[abi, ty]),
        func_sec(&[0, 1]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 0),
            ("megu_on_tick", KIND_FUNC, 1),
        ]),
        code_sec(&[abi_body, spin]),
    ]);
    let mut m = load(&bytes, all_caps()).unwrap();
    assert_eq!(m.on_tick(0).unwrap(), 5000);
    assert_eq!(m.on_tick(1).unwrap(), 5000);
    assert!(m.quarantined().is_none());
}

#[test]
fn nested_control_and_globals_work() {
    // on_tick: if tick is even store 1 in global, else store 2;
    // then compute global * 10 + 5 via nested blocks → returns i32.
    let ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let abi: (&[u8], &[u8]) = (&[], &[I32]);
    // Body:
    //   local.get 0; i64.const 1; i64.and; i64.eqz; if (result i32)
    //     i32.const 1
    //   else
    //     i32.const 2
    //   end; global.set 0; global.get 0; i32.const 10; i32.mul;
    //   i32.const 5; i32.add; end
    let f = body(
        &[],
        &concat(&[
            &local_get(0),
            &i64_const(1),
            &[0x83],       // i64.and
            &[0x50],       // i64.eqz
            &[0x04, 0x7F], // if (result i32)
            &i32_const(1),
            &[0x05], // else
            &i32_const(2),
            &[0x0B], // end if
            &global_set(0),
            &global_get(0),
            &i32_const(10),
            &[0x6C], // i32.mul
            &i32_const(5),
            &[0x6A], // i32.add
            &[0x0B], // func end
        ]),
    );
    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    let bytes = module(&[
        types_sec(&[abi, ty]),
        func_sec(&[0, 1]),
        global_sec(&[(I32, true, i32_const(0))]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 0),
            ("megu_on_tick", KIND_FUNC, 1),
        ]),
        code_sec(&[abi_body, f]),
    ]);
    let mut m = load(&bytes, all_caps()).unwrap();
    assert_eq!(m.on_tick(4).unwrap(), 15); // even → 1*10+5
    assert_eq!(m.on_tick(7).unwrap(), 25); // odd  → 2*10+5
}

#[test]
fn start_function_runs_at_instantiate() {
    // start sets global0 = 42; abi_version returns global0... but
    // abi must be 1, so use a separate read: on_load returns global0.
    let abi: (&[u8], &[u8]) = (&[], &[I32]);
    let start_body = body(&[], &concat(&[&i32_const(42), &global_set(0), &[0x0B]]));
    let abi_body = body(&[], &concat(&[&i32_const(1), &[0x0B]]));
    let on_load_body = body(&[], &concat(&[&global_get(0), &[0x0B]]));
    // func order: 0 = start (()->()), 1 = abi, 2 = on_load
    let start_ty: (&[u8], &[u8]) = (&[], &[]);
    let bytes = module(&[
        types_sec(&[abi, start_ty]),
        func_sec(&[1, 0, 0]),
        global_sec(&[(I32, true, i32_const(0))]),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 1),
            ("megu_on_load", KIND_FUNC, 2),
        ]),
        start_sec(0),
        code_sec(&[start_body, abi_body, on_load_body]),
    ]);
    let mut m = load(&bytes, all_caps()).unwrap();
    assert_eq!(m.on_load().unwrap(), 42);
}

// ------------------------------------------------------ A15 hostile sandbox --

fn abi_and_tick_module(tick_body: Vec<u8>, memory: Option<(u32, Option<u32>)>) -> Vec<u8> {
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let tick_ty: (&[u8], &[u8]) = (&[I64], &[I32]);
    let mut sections = vec![types_sec(&[abi_ty, tick_ty]), func_sec(&[0, 1])];
    if let Some((min, max)) = memory {
        sections.push(mem_sec(min, max));
    }
    sections.extend([
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 0),
            ("megu_on_tick", KIND_FUNC, 1),
        ]),
        code_sec(&[body(&[], &concat(&[&i32_const(1), &[0x0B]])), tick_body]),
    ]);
    module(&sections)
}

#[test]
fn memory_growth_ceiling_fails_closed_without_trapping() {
    // status = grow(2) + 10*grow(1) + 100*memory.size.
    // With min=1 and cap=2: grow(2) fails as -1, grow(1) succeeds returning
    // the old size 1, and the final size is 2 pages -> -1 + 10 + 200 = 209.
    let tick = body(
        &[],
        &concat(&[
            &i32_const(2),
            &[0x40, 0x00], // memory.grow
            &i32_const(1),
            &[0x40, 0x00],
            &i32_const(10),
            &[0x6C],       // i32.mul
            &[0x6A],       // i32.add
            &[0x3F, 0x00], // memory.size
            &i32_const(100),
            &[0x6C],
            &[0x6A],
            &[0x0B],
        ]),
    );
    let bytes = abi_and_tick_module(tick, Some((1, None)));
    let config = ModConfig {
        max_mem_pages: 2,
        ..ModConfig::default()
    };
    let mut m = WasmMod::from_wasm(&bytes, Box::new(MockHost::default()), all_caps(), config)
        .expect("loads");
    assert_eq!(m.on_tick(0).unwrap(), 209);
    assert!(m.quarantined().is_none());
}

#[test]
fn declared_memory_minimum_above_config_or_hard_cap_never_instantiates() {
    let tick = body(&[], &concat(&[&i32_const(0), &[0x0B]]));
    let bytes = abi_and_tick_module(tick.clone(), Some((2, None)));
    let config = ModConfig {
        max_mem_pages: 1,
        ..ModConfig::default()
    };
    assert!(matches!(
        WasmMod::from_wasm(&bytes, Box::new(MockHost::default()), all_caps(), config),
        Err(ModFault::Load(LoadError::LimitExceeded(_)))
    ));

    // Even a misconfigured unlimited budget cannot lift the runtime's hard
    // 1024-page ceiling, so this module is rejected before a 64+ MiB allocation.
    let bytes = abi_and_tick_module(tick, Some((1025, None)));
    let config = ModConfig {
        max_mem_pages: u32::MAX,
        ..ModConfig::default()
    };
    assert!(matches!(
        WasmMod::from_wasm(&bytes, Box::new(MockHost::default()), all_caps(), config),
        Err(ModFault::Load(LoadError::LimitExceeded(_)))
    ));
}

fn host_log_probe(ptr: i32, len: i32) -> Vec<u8> {
    let abi_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let load_ty: (&[u8], &[u8]) = (&[], &[I32]);
    let log_ty: (&[u8], &[u8]) = (&[I32, I32], &[]);
    module(&[
        types_sec(&[abi_ty, load_ty, log_ty]),
        import_funcs_sec(&[("env", "host_log", 2)]),
        func_sec(&[0, 1]),
        mem_sec(1, Some(1)),
        export_sec(&[
            ("megu_abi_version", KIND_FUNC, 1),
            ("megu_on_load", KIND_FUNC, 2),
        ]),
        code_sec(&[
            body(&[], &concat(&[&i32_const(1), &[0x0B]])),
            body(
                &[],
                &concat(&[
                    &i32_const(ptr),
                    &i32_const(len),
                    &call(0),
                    &i32_const(0),
                    &[0x0B],
                ]),
            ),
        ]),
    ])
}

#[test]
fn hostile_hostcall_ptr_len_traps_without_host_memory_access() {
    // ptr=65535,len=2 crosses the single 64 KiB page boundary.
    let mut m = WasmMod::from_wasm(
        &host_log_probe(65_535, 2),
        Box::new(MockHost::default()),
        all_caps(),
        ModConfig::default(),
    )
    .expect("loads");
    assert!(matches!(
        m.on_load(),
        Err(ModFault::Trap(Trap::HostMemoryAccess {
            ptr: 65_535,
            len: 2
        }))
    ));
    assert!(m.quarantined().is_some());

    // len above the configured host-string ceiling fails before memory is read.
    let config = ModConfig {
        max_string: 4,
        ..ModConfig::default()
    };
    let mut m = WasmMod::from_wasm(
        &host_log_probe(0, 5),
        Box::new(MockHost::default()),
        all_caps(),
        config,
    )
    .expect("loads");
    assert!(matches!(
        m.on_load(),
        Err(ModFault::Trap(Trap::HostMemoryAccess { ptr: 0, len: 5 }))
    ));
    assert!(m.quarantined().is_some());
}
