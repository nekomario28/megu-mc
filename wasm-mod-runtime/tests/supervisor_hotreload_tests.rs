// SPDX-License-Identifier: LGPL-3.0-or-later

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use megu_wasm_mod_runtime::testutil::*;
use megu_wasm_mod_runtime::{
    decode_kv_snapshot, encode_kv_snapshot, inspect_archive, Capability, CapabilitySet, KvError,
    LoadError, MeguHost, ModConfig, ModFault, PluginConfig, PluginKvLimits, PluginKvStore,
    PluginSupervisor, ReloadError, Trap, EXPORT_ABI_VERSION, EXPORT_ON_LOAD, EXPORT_ON_TICK,
    IMPORT_GET_BLOCK, IMPORT_KV_DELETE, IMPORT_KV_GET, IMPORT_KV_SET, IMPORT_SET_BLOCK,
    MANIFEST_ENTRY, MOD_WASM_ENTRY,
};

const KV_IMPORTS: &[(&str, &str, u32)] = &[
    ("megu_v1", IMPORT_KV_GET, 2),
    ("env", IMPORT_KV_SET, 2),
    ("env", IMPORT_KV_DELETE, 3),
];
const GET: u32 = 0;
const SET: u32 = 1;
const DELETE: u32 = 2;
const STABLE_KEY: i32 = 0;
const NEW_KEY: i32 = 1;
const OLD_VALUE: i32 = 2;
const NEXT_VALUE: i32 = 3;
const EXTRA_VALUE: i32 = 4;
const OUTPUT_PTR: i32 = 16;
const DATA: &[u8] = b"knABZ";
const FOREVER: &[u8] = &[0x03, 0x40, 0x0C, 0x00, 0x0B];
const DROP: &[u8] = &[0x1A];
const UNREACHABLE: &[u8] = &[0x00];

#[derive(Default)]
struct Host {
    block_reads: usize,
    block_writes: usize,
    watchdog: Option<(SyncSender<()>, Receiver<()>)>,
}

impl MeguHost for Host {
    fn log(&mut self, _: &str) {}
    fn emit_chat(&mut self, _: i32, _: &str) -> i32 {
        0
    }
    fn get_block(&mut self, _: i32, _: i32, _: i32) -> i32 {
        self.block_reads += 1;
        if let Some((entered, interrupted)) = self.watchdog.take() {
            entered.send(()).unwrap();
            interrupted.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        0
    }
    fn set_block(&mut self, _: i32, _: i32, _: i32, _: i32) -> i32 {
        self.block_writes += 1;
        0
    }
}

fn wasm(imports: &[(&str, &str, u32)], load: &[u8], tick: &[u8], data: &[u8]) -> Vec<u8> {
    let first_local = imports.len() as u32;
    module(&[
        types_sec(&[
            (&[], &[I32]),
            (&[I64], &[I32]),
            (&[I32; 4], &[I32]),
            (&[I32; 2], &[I32]),
            (&[I32; 3], &[I32]),
        ]),
        import_funcs_sec(imports),
        func_sec(&[0, 0, 1]),
        mem_sec(1, Some(1)),
        global_sec(&[(I32, true, i32_const(0))]),
        export_sec(&[
            (EXPORT_ABI_VERSION, KIND_FUNC, first_local),
            (EXPORT_ON_LOAD, KIND_FUNC, first_local + 1),
            (EXPORT_ON_TICK, KIND_FUNC, first_local + 2),
        ]),
        code_sec(&[
            body(&[], &concat(&[&i32_const(1), &[0x0B]])),
            body(&[], &concat(&[load, &[0x0B]])),
            body(&[], &concat(&[tick, &[0x0B]])),
        ]),
        data_sec(&[(0, 0, data)]),
    ])
}

fn call_i32(func: u32, args: &[i32]) -> Vec<u8> {
    let mut code = Vec::new();
    for &arg in args {
        code.extend(i32_const(arg));
    }
    code.extend(call(func));
    code
}

fn set(key: i32, value: i32) -> Vec<u8> {
    concat(&[&call_i32(SET, &[key, 1, value, 1]), DROP])
}

fn assert_byte(key: i32, expected: u8) -> Vec<u8> {
    // Check the returned length and the byte copied to guest memory.
    concat(&[
        &call_i32(GET, &[key, 1, OUTPUT_PTR, 1]),
        &i32_const(1),
        &[0x47, 0x04, 0x40, 0x00, 0x0B], // i32.ne; if { unreachable }
        &i32_const(OUTPUT_PTR),
        &[0x2D, 0x00, 0x00], // i32.load8_u
        &i32_const(i32::from(expected)),
        &[0x47, 0x04, 0x40, 0x00, 0x0B],
    ])
}

fn kv_config() -> PluginConfig {
    PluginConfig {
        capabilities: CapabilitySet::NONE.granted(Capability::KvStore),
        ..PluginConfig::default()
    }
}

#[test]
fn hot_reload_and_rollback_keep_committed_kv_and_restore_guest_state() {
    let mut supervisor = PluginSupervisor::default();
    let mut host = Host::default();
    let v1 = wasm(
        KV_IMPORTS,
        &i32_const(0),
        &concat(&[
            &set(STABLE_KEY, OLD_VALUE),
            &global_get(0),
            &i32_const(1),
            &[0x6A], // i32.add
            &global_set(0),
            &global_get(0),
        ]),
        DATA,
    );
    let first = supervisor
        .load_plugin("plugin", &v1, kv_config(), &mut host)
        .unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(first.previous_generation, None);
    assert_eq!(first.contract_rev, 2);
    assert_eq!(supervisor.tick_all(1, &mut host).outcomes[0].result, Ok(1));

    let v2 = wasm(
        KV_IMPORTS,
        &concat(&[
            &assert_byte(STABLE_KEY, b'A'),
            &set(NEW_KEY, NEXT_VALUE),
            &i32_const(0),
        ]),
        &concat(&[
            &assert_byte(NEW_KEY, b'B'),
            &set(NEW_KEY, EXTRA_VALUE),
            &i32_const(20),
        ]),
        DATA,
    );
    let next = supervisor
        .hot_reload_plugin("plugin", &v2, kv_config(), &mut host)
        .unwrap();
    assert_eq!(next.generation, 2);
    assert_eq!(next.previous_generation, Some(1));
    let store = supervisor.kv_store("plugin").unwrap();
    assert_eq!(store.get(b"k"), Some(b"A".as_slice()));
    assert_eq!(store.get(b"n"), Some(b"B".as_slice()));
    assert_eq!(supervisor.tick_all(2, &mut host).outcomes[0].result, Ok(20));

    let restored = supervisor.rollback_plugin("plugin").unwrap();
    assert_eq!(restored.generation, 1);
    assert_eq!(restored.previous_generation, Some(2));
    assert_eq!(supervisor.tick_all(3, &mut host).outcomes[0].result, Ok(2));
    assert_eq!(
        supervisor.kv_store("plugin").unwrap().get(b"n"),
        Some(b"Z".as_slice())
    );
    assert_eq!(
        supervisor.rollback_plugin("plugin"),
        Err(ReloadError::NoPriorHealthyGeneration)
    );
    assert_eq!(
        supervisor
            .hot_reload_plugin("plugin", &v2, kv_config(), &mut host)
            .unwrap()
            .generation,
        3
    );
}

#[test]
fn candidate_failures_leave_the_active_generation_and_kv_intact() {
    let mut supervisor = PluginSupervisor::default();
    let mut host = Host::default();
    let good = wasm(
        KV_IMPORTS,
        &concat(&[&set(STABLE_KEY, OLD_VALUE), &i32_const(0)]),
        &concat(&[&assert_byte(STABLE_KEY, b'A'), &i32_const(42)]),
        DATA,
    );
    supervisor
        .load_plugin("plugin", &good, kv_config(), &mut host)
        .unwrap();
    supervisor
        .hot_reload_plugin("plugin", &good, kv_config(), &mut host)
        .unwrap();

    let denied = supervisor.hot_reload_plugin("plugin", &good, PluginConfig::default(), &mut host);
    assert_eq!(
        denied,
        Err(ReloadError::Fault(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::KvStore,
            import: IMPORT_KV_GET,
        })))
    );
    assert!(matches!(
        supervisor.hot_reload_plugin("plugin", b"broken", kv_config(), &mut host),
        Err(ReloadError::Fault(ModFault::Load(LoadError::Malformed(_))))
    ));

    let revision_one = PluginConfig {
        max_contract_rev: 1,
        ..kv_config()
    };
    assert_eq!(
        supervisor.hot_reload_plugin("plugin", &good, revision_one, &mut host),
        Err(ReloadError::Fault(ModFault::Load(
            LoadError::UnsupportedContract {
                required: 2,
                supported: 1,
            }
        )))
    );

    for (tail, error) in [
        (
            UNREACHABLE.to_vec(),
            ReloadError::Fault(ModFault::Trap(Trap::Unreachable)),
        ),
        (
            concat(&[FOREVER, &i32_const(0)]),
            ReloadError::Fault(ModFault::Trap(Trap::FuelExhausted)),
        ),
        (i32_const(-1), ReloadError::StartupStatus(-1)),
    ] {
        let bad = wasm(
            KV_IMPORTS,
            &concat(&[
                &set(STABLE_KEY, NEXT_VALUE),
                &set(NEW_KEY, EXTRA_VALUE),
                &tail,
            ]),
            &i32_const(0),
            DATA,
        );
        let mut config = kv_config();
        config.runtime.fuel_on_load = 100;
        assert_eq!(
            supervisor.hot_reload_plugin("plugin", &bad, config, &mut host),
            Err(error)
        );
        assert_eq!(supervisor.generation("plugin"), Some(2));
        assert!(supervisor.quarantined("plugin").is_none());
        assert_eq!(supervisor.kv_store("plugin").unwrap().get(b"n"), None);
        assert_eq!(supervisor.tick_all(7, &mut host).outcomes[0].result, Ok(42));
    }
}

#[test]
fn isolated_startup_rejects_mutating_the_live_host() {
    let mut supervisor = PluginSupervisor::default();
    let mut host = Host::default();
    let imports = [("env", IMPORT_SET_BLOCK, 2)];
    let mutate = call_i32(0, &[1, 2, 3, 4]);
    let config = PluginConfig {
        capabilities: CapabilitySet::NONE.granted(Capability::BlockRegistry),
        ..PluginConfig::default()
    };
    let good = wasm(&imports, &i32_const(0), &mutate, b"");
    supervisor
        .load_plugin("plugin", &good, config.clone(), &mut host)
        .unwrap();
    let bad = wasm(&imports, &mutate, &i32_const(0), b"");
    assert_eq!(
        supervisor.hot_reload_plugin("plugin", &bad, config, &mut host),
        Err(ReloadError::Fault(ModFault::Trap(
            Trap::StagedHostMutation {
                import: IMPORT_SET_BLOCK,
            }
        )))
    );
    assert_eq!(host.block_writes, 0);
    assert_eq!(supervisor.tick_all(1, &mut host).outcomes[0].result, Ok(0));
    assert_eq!(host.block_writes, 1);
}

#[test]
fn tick_trap_discards_updates_deletions_and_new_keys_for_only_one_plugin() {
    let mut supervisor = PluginSupervisor::default();
    let mut host = Host::default();
    let fault = wasm(
        KV_IMPORTS,
        &concat(&[&set(STABLE_KEY, OLD_VALUE), &i32_const(0)]),
        &concat(&[
            &set(STABLE_KEY, NEXT_VALUE),
            &assert_byte(STABLE_KEY, b'B'),
            &call_i32(DELETE, &[STABLE_KEY, 1]),
            DROP,
            &set(NEW_KEY, EXTRA_VALUE),
            UNREACHABLE,
        ]),
        DATA,
    );
    let sibling = wasm(
        KV_IMPORTS,
        &concat(&[
            &call_i32(GET, &[STABLE_KEY, 1, OUTPUT_PTR, 1]),
            &i32_const(-1),
            &[0x47, 0x04, 0x40, 0x00, 0x0B],
            &set(STABLE_KEY, NEXT_VALUE),
            &i32_const(0),
        ]),
        &concat(&[&assert_byte(STABLE_KEY, b'B'), &i32_const(25)]),
        DATA,
    );
    supervisor
        .load_plugin("b-sibling", &sibling, kv_config(), &mut host)
        .unwrap();
    supervisor
        .load_plugin("a-fault", &fault, kv_config(), &mut host)
        .unwrap();
    let report = supervisor.tick_all(1, &mut host);
    assert_eq!(report.outcomes.len(), 2);
    assert_eq!(report.outcomes[0].mod_id, "a-fault");
    assert_eq!(
        report.outcomes[0].result,
        Err(ModFault::Trap(Trap::Unreachable))
    );
    assert_eq!(report.outcomes[1].mod_id, "b-sibling");
    assert_eq!(report.outcomes[1].result, Ok(25));
    assert_eq!(
        supervisor.kv_store("a-fault").unwrap().get(b"k"),
        Some(b"A".as_slice())
    );
    assert_eq!(supervisor.kv_store("a-fault").unwrap().get(b"n"), None);
    assert_eq!(
        supervisor.kv_store("b-sibling").unwrap().get(b"k"),
        Some(b"B".as_slice())
    );
    assert_eq!(supervisor.tick_all(2, &mut host).outcomes.len(), 1);
}

#[test]
fn fuel_and_kv_limits_discard_pending_tick_writes() {
    let mut supervisor = PluginSupervisor::new(PluginKvLimits {
        max_keys: 1,
        ..PluginKvLimits::default()
    });
    let mut host = Host::default();
    let load = concat(&[&set(STABLE_KEY, OLD_VALUE), &i32_const(0)]);
    let change = set(STABLE_KEY, NEXT_VALUE);
    let mut config = kv_config();
    config.runtime.fuel_per_tick = 100;

    for (id, action) in [
        ("a-fuel", FOREVER.to_vec()),
        ("b-limit", set(NEW_KEY, EXTRA_VALUE)),
    ] {
        let tick = concat(&[&change, &action, &i32_const(0)]);
        let guest = wasm(KV_IMPORTS, &load, &tick, DATA);
        supervisor
            .load_plugin(id, &guest, config.clone(), &mut host)
            .unwrap();
    }
    let report = supervisor.tick_all(1, &mut host);
    assert_eq!(
        report.outcomes[0].result,
        Err(ModFault::Trap(Trap::FuelExhausted))
    );
    assert_eq!(
        report.outcomes[1].result,
        Err(ModFault::Trap(Trap::KvLimitExceeded))
    );
    for id in ["a-fuel", "b-limit"] {
        let store = supervisor.kv_store(id).unwrap();
        assert_eq!(store.get(b"k"), Some(b"A".as_slice()));
        assert_eq!(store.get(b"n"), None);
    }
}

#[test]
fn watchdog_interrupts_an_infinite_loop_without_quarantining_a_sibling() {
    let mut supervisor = PluginSupervisor::default();
    let mut host = Host::default();
    let mut imports = KV_IMPORTS.to_vec();
    imports.push(("env", IMPORT_GET_BLOCK, 4));
    let forever = concat(&[
        &set(STABLE_KEY, NEXT_VALUE),
        &[0x03, 0x40], // loop
        &call_i32(3, &[0, 0, 0]),
        DROP,
        &[0x0C, 0x00, 0x0B], // br 0; end
        &i32_const(0),
    ]);
    let runaway = wasm(
        &imports,
        &concat(&[&set(STABLE_KEY, OLD_VALUE), &i32_const(0)]),
        &forever,
        DATA,
    );
    let healthy = wasm(&[], &i32_const(0), &i32_const(99), b"");
    let mut config = kv_config();
    config.capabilities = config.capabilities.granted(Capability::ReadBlocks);
    config.runtime.fuel_per_tick = 100_000;
    config.runtime.fuel_on_load = 100_000;
    supervisor
        .load_plugin("a-runaway", &runaway, config.clone(), &mut host)
        .unwrap();
    supervisor
        .load_plugin("b-healthy", &healthy, PluginConfig::default(), &mut host)
        .unwrap();

    // First interrupt a candidate during on_load. The active version
    // survives even though both generations use the same watchdog.
    let hot_candidate = wasm(&imports, &forever, &i32_const(0), DATA);
    let watchdog = start_watchdog(&supervisor, &mut host);
    assert_eq!(
        supervisor.hot_reload_plugin("a-runaway", &hot_candidate, config, &mut host),
        Err(ReloadError::Fault(ModFault::Trap(Trap::EpochExceeded)))
    );
    watchdog.join().unwrap();
    assert!(supervisor.quarantined("a-runaway").is_none());
    assert_eq!(supervisor.generation("a-runaway"), Some(1));
    assert_eq!(
        supervisor.kv_store("a-runaway").unwrap().get(b"k"),
        Some(b"A".as_slice())
    );

    let watchdog = start_watchdog(&supervisor, &mut host);
    let report = supervisor.tick_all(1, &mut host);
    watchdog.join().unwrap();
    assert_eq!(
        report.outcomes[0].result,
        Err(ModFault::Trap(Trap::EpochExceeded))
    );
    assert_eq!(report.outcomes[1].result, Ok(99));
    assert_eq!(
        supervisor.quarantined("a-runaway"),
        Some(&ModFault::Trap(Trap::EpochExceeded))
    );
    assert_eq!(
        supervisor.kv_store("a-runaway").unwrap().get(b"k"),
        Some(b"A".as_slice())
    );
    assert_eq!(
        supervisor.tick_all(2, &mut host).outcomes[0].mod_id,
        "b-healthy"
    );
}

fn start_watchdog(supervisor: &PluginSupervisor, host: &mut Host) -> thread::JoinHandle<()> {
    let epoch = supervisor.epoch_guard("a-runaway").unwrap();
    let (entered, wait_for_guest) = sync_channel(0);
    let (interrupted, wait_for_watchdog) = sync_channel(0);
    host.watchdog = Some((entered, wait_for_watchdog));
    thread::spawn(move || {
        wait_for_guest.recv_timeout(Duration::from_secs(5)).unwrap();
        epoch.bump();
        interrupted.send(()).unwrap();
    })
}

#[test]
fn archive_negotiates_clean_capabilities_and_checks_unused_imports() {
    let guest = wasm(KV_IMPORTS, &i32_const(0), &i32_const(0), DATA);
    let manifest = b"[capabilities]\nblock_registry = true\n\"kv_store\" = true # binary state\n\
        commands = true\nevents = true\nchat = true\nui_overlay = true\n";
    let archive = stored_zip(&[(MOD_WASM_ENTRY, &guest), (MANIFEST_ENTRY, manifest)]);
    let payload = inspect_archive(&archive, &ModConfig::default()).unwrap();
    for capability in [
        Capability::ReadBlocks,
        Capability::WriteBlocks,
        Capability::KvStore,
        Capability::Commands,
        Capability::EntityEvents,
        Capability::ChatBroadcast,
        Capability::UiSurface,
    ] {
        assert!(payload.capabilities.has(capability));
    }
    let mut supervisor = PluginSupervisor::default();
    let mut host = Host::default();
    assert_eq!(
        supervisor.load_plugin_archive("plugin", &archive, PluginConfig::default(), &mut host),
        Err(ReloadError::Fault(ModFault::Trap(Trap::CapabilityDenied {
            capability: Capability::KvStore,
            import: IMPORT_KV_GET,
        })))
    );
    assert_eq!(
        supervisor
            .load_plugin_archive("plugin", &archive, kv_config(), &mut host)
            .unwrap()
            .contract_rev,
        2
    );

    for bad_manifest in [
        b"[capabilities]\nkv_store_v1 = true".as_slice(),
        b"[capabilities]\nkv_store = true\nkv_store = false",
        b"[capabilities]\nkv_store = true\n[capabilities]\nkv_store = false",
    ] {
        let bad = stored_zip(&[(MOD_WASM_ENTRY, &guest), (MANIFEST_ENTRY, bad_manifest)]);
        assert!(matches!(
            supervisor.hot_reload_plugin_archive("plugin", &bad, kv_config(), &mut host),
            Err(ReloadError::Fault(ModFault::Load(LoadError::Archive(_))))
        ));
    }
    let fake_table = b"description = '''\n[capabilities]\nkv_store = true\n'''";
    let bad = stored_zip(&[(MOD_WASM_ENTRY, &guest), (MANIFEST_ENTRY, fake_table)]);
    assert!(matches!(
        supervisor.hot_reload_plugin_archive("plugin", &bad, kv_config(), &mut host),
        Err(ReloadError::Fault(ModFault::Trap(
            Trap::CapabilityDenied { .. }
        )))
    ));
    assert_eq!(supervisor.generation("plugin"), Some(1));

    let legacy = wasm(&[], &i32_const(0), &i32_const(0), b"");
    let legacy_archive = stored_zip(&[(MOD_WASM_ENTRY, &legacy)]);
    assert_eq!(
        supervisor
            .hot_reload_plugin_archive(
                "plugin",
                &legacy_archive,
                PluginConfig::default(),
                &mut host
            )
            .unwrap()
            .contract_rev,
        1
    );
}

#[test]
fn kv_limits_and_snapshot_validation_are_atomic() {
    let limits = PluginKvLimits {
        max_keys: 2,
        max_key_bytes: 2,
        max_value_bytes: 3,
        max_total_bytes: 6,
    };
    let mut store = PluginKvStore::new(limits);
    let mut txn = store.transaction();
    txn.set(b"z", b"\0\xff").unwrap();
    txn.set(b"a", b"ab").unwrap();
    assert_eq!(txn.set(b"too", b""), Err(KvError::LimitExceeded));
    assert_eq!(txn.set(b"z", b"1234"), Err(KvError::LimitExceeded));
    assert_eq!(txn.set(b"z", b"123"), Err(KvError::LimitExceeded));
    assert_eq!(txn.set(b"b", b""), Err(KvError::LimitExceeded));
    txn.commit();

    let bytes = encode_kv_snapshot(&store);
    assert_eq!(&bytes[..10], b"WKV1\x01\0\x02\0\0\0");
    assert_eq!(bytes[18], b'a');
    let restored = decode_kv_snapshot(&bytes, limits).unwrap();
    assert_eq!(encode_kv_snapshot(&restored), bytes);
    assert_eq!(restored.get(b"z"), Some(b"\0\xff".as_slice()));

    for end in 0..bytes.len() {
        assert!(decode_kv_snapshot(&bytes[..end], limits).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    let mut wrong_version = bytes.clone();
    wrong_version[4] = 2;
    let mut duplicate = bytes.clone();
    duplicate[29] = b'a';
    let mut unsorted = bytes.clone();
    unsorted[18] = b'z';
    unsorted[29] = b'a';
    let mut enormous_length = bytes.clone();
    enormous_length[10..14].copy_from_slice(&u32::MAX.to_le_bytes());
    for malformed in [trailing, duplicate, unsorted, enormous_length] {
        assert!(matches!(
            decode_kv_snapshot(&malformed, limits),
            Err(KvError::InvalidSnapshot)
        ));
    }
    assert!(matches!(
        decode_kv_snapshot(&wrong_version, limits),
        Err(KvError::UnsupportedSnapshotVersion(2))
    ));
    assert!(matches!(
        decode_kv_snapshot(
            &bytes,
            PluginKvLimits {
                max_total_bytes: 5,
                ..limits
            }
        ),
        Err(KvError::LimitExceeded)
    ));

    let mut txn = store.transaction();
    assert!(txn.delete(b"a").unwrap());
    txn.set(b"x", b"y").unwrap();
    txn.rollback();
    assert_eq!(encode_kv_snapshot(&store), bytes);

    let mut supervisor = PluginSupervisor::new(limits);
    let mut host = Host::default();
    let guest = wasm(&[], &i32_const(0), &i32_const(0), b"");
    supervisor
        .load_plugin("restore", &guest, PluginConfig::default(), &mut host)
        .unwrap();
    supervisor.restore_kv_snapshot("restore", &bytes).unwrap();
    assert!(supervisor
        .restore_kv_snapshot("restore", &wrong_version)
        .is_err());
    assert_eq!(
        encode_kv_snapshot(supervisor.kv_store("restore").unwrap()),
        bytes
    );
}
