// SPDX-License-Identifier: LGPL-3.0-or-later

//! Lane WIRE-MOD-GENERALIZE-41 tests: real generic `data/` JAR extraction
//! for ANY namespace, real NeoForge/Fabric 1.21.1 `.class` event
//! descriptors through the bounded subset VM, and safe read-only
//! `@Accessor` lifting — all fail-closed, all clean-room fixtures.

use mod_adapter_core::auto_adapter::{
    auto_adapt_jar_bytes, extract_mod_data, AdaptationTier, AutoAdaptOptions,
};
use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
use mod_adapter_core::bytecode_subset_vm::{
    execute_verified_method, parse_class_methods, verify_subset_method, HostCallKind, SubsetVmHost,
    VmError, VmValue, DEFAULT_FUEL,
};
use mod_adapter_core::class_mixin_scanner::parse_mixin_class;
use mod_adapter_core::fallback::ExecutionTier;
use mod_adapter_core::loader_extensions::BiomeModifierKind;
use mod_adapter_core::mixin_lifter::{
    event_descriptor_hook, lift_accessor_sites, lift_mixin_sites, AccessorRejection,
    AccessorValueKind, MixinAnnotationKind, MixinLiftVerdict, StandardEventHook, UnliftableReason,
};

// ---------------------------------------------------------------------------
// ZIP/JAR builder (stored members, real central directory + EOCD)
// ---------------------------------------------------------------------------

use megu_core::hash::crc32_ieee as crc32;

fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, bytes) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32(bytes);
        let size = bytes.len() as u32;
        let offset = out.len() as u32;

        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes()); // version needed
        out.extend(0u16.to_le_bytes()); // flags
        out.extend(0u16.to_le_bytes()); // method: stored
        out.extend(0u16.to_le_bytes()); // mod time
        out.extend(0u16.to_le_bytes()); // mod date
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name_bytes.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra len
        out.extend(name_bytes);
        out.extend(bytes);

        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes()); // version made by
        central.extend(10u16.to_le_bytes()); // version needed
        central.extend(0u16.to_le_bytes()); // flags
        central.extend(0u16.to_le_bytes()); // method
        central.extend(0u16.to_le_bytes()); // time
        central.extend(0u16.to_le_bytes()); // date
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name_bytes.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes()); // extra len
        central.extend(0u16.to_le_bytes()); // comment len
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_be_bytes()); // internal attrs
        central.extend(0u32.to_le_bytes()); // external attrs
        central.extend(offset.to_le_bytes());
        central.extend(name_bytes);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // disk
    out.extend(0u16.to_le_bytes()); // cd disk
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // comment len
    out
}

/// The six 1.21.1 data families under an arbitrary unknown namespace —
/// nothing here is keyed on a known mod id.
const NS: &str = "my_custom_mod";

fn six_family_jar() -> Vec<u8> {
    let manifest = br#"{"schemaVersion":1,"id":"my_custom_mod","version":"1.0.0","environment":"*"}"#;
    let recipe = br#"{"type":"my_custom_mod:infusing",
        "ingredient":"my_custom_mod:raw_gem",
        "result":{"id":"my_custom_mod:gem","count":1},
        "processing_time":80}"#;
    let tag = br#"{"replace":false,"values":["my_custom_mod:gem","my_custom_mod:raw_gem"]}"#;
    let loot = br#"{"type":"minecraft:block",
        "pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"my_custom_mod:gem"}]}]}"#;
    let dimension = br#"{"type":"my_custom_mod:gem_realm",
        "generator":{"type":"minecraft:noise","settings":"minecraft:overworld",
            "biome_source":{"type":"minecraft:fixed","biome":"my_custom_mod:gem_fields"}}}"#;
    let dimension_type =
        br#"{"min_y":-64,"height":384,"ambient_light":0.1,"has_sky":true}"#;
    let data_map = br#"{"values":{"my_custom_mod:gem_block":{"burn_time":2000}}}"#;
    let biome_modifier = br#"{"type":"neoforge:add_features",
        "biomes":"my_custom_mod:gem_fields",
        "features":"my_custom_mod:ore_gem",
        "step":"underground_ores"}"#;
    jar(&[
        ("fabric.mod.json", manifest),
        (
            "data/my_custom_mod/recipes/infuse_gem.json",
            recipe,
        ),
        ("data/my_custom_mod/tags/items/gems.json", tag),
        ("data/my_custom_mod/loot_tables/blocks/gem_ore.json", loot),
        ("data/my_custom_mod/dimension/gem_realm.json", dimension),
        ("data/my_custom_mod/dimension_type/gem_realm.json", dimension_type),
        (
            "data/my_custom_mod/data_maps/item/furnace_fuels.json",
            data_map,
        ),
        (
            "data/my_custom_mod/neoforge/biome_modifier/add_gem_ore.json",
            biome_modifier,
        ),
        // A recognized family member that fails its parser — recorded in
        // `rejected_members`, never silently dropped.
        (
            "data/my_custom_mod/loot_tables/blocks/corrupted.json",
            b"{not json",
        ),
    ])
}

// ---------------------------------------------------------------------------
// `.class` mixin fixture builder — per-method descriptors + annotations
// ---------------------------------------------------------------------------

enum EV {
    S(String),
    Arr(Vec<EV>),
}

struct Ann {
    desc: String,
    elems: Vec<(String, EV)>,
}

fn ann(desc: &str, elems: Vec<(&str, EV)>) -> Ann {
    Ann {
        desc: desc.to_string(),
        elems: elems
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }
}

fn s(v: &str) -> EV {
    EV::S(v.to_string())
}

enum CpItem {
    Utf8(String),
    Class(u16),
}

#[derive(Default)]
struct Cp(Vec<CpItem>);

impl Cp {
    fn utf8(&mut self, v: &str) -> u16 {
        for (i, item) in self.0.iter().enumerate() {
            if let CpItem::Utf8(s) = item {
                if s == v {
                    return (i + 1) as u16;
                }
            }
        }
        self.0.push(CpItem::Utf8(v.to_string()));
        self.0.len() as u16
    }

    fn class(&mut self, internal: &str) -> u16 {
        let name_index = self.utf8(internal);
        self.0.push(CpItem::Class(name_index));
        self.0.len() as u16
    }

    fn utf8_index(&self, v: &str) -> u16 {
        for (i, item) in self.0.iter().enumerate() {
            if let CpItem::Utf8(s) = item {
                if s == v {
                    return (i + 1) as u16;
                }
            }
        }
        panic!("fixture: utf8 {v:?} not interned");
    }

    fn class_index(&self, internal: &str) -> u16 {
        let name_index = self.utf8_index(internal);
        for (i, item) in self.0.iter().enumerate() {
            if let CpItem::Class(n) = item {
                if *n == name_index {
                    return (i + 1) as u16;
                }
            }
        }
        panic!("fixture: class {internal:?} not interned");
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for item in &self.0 {
            match item {
                CpItem::Utf8(v) => {
                    out.push(1);
                    out.extend((v.len() as u16).to_be_bytes());
                    out.extend(v.as_bytes());
                }
                CpItem::Class(name_index) => {
                    out.push(7);
                    out.extend(name_index.to_be_bytes());
                }
            }
        }
    }
}

fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &EV) {
    match ev {
        EV::S(v) => {
            out.push(b's');
            out.extend(cp.utf8(v).to_be_bytes());
        }
        EV::Arr(items) => {
            out.push(b'[');
            out.extend((items.len() as u16).to_be_bytes());
            for item in items {
                write_ev(out, cp, item);
            }
        }
    }
}

fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, a: &Ann) {
    out.extend(cp.utf8(&a.desc).to_be_bytes());
    out.extend((a.elems.len() as u16).to_be_bytes());
    for (name, ev) in &a.elems {
        out.extend(cp.utf8(name).to_be_bytes());
        write_ev(out, cp, ev);
    }
}

struct Meth {
    name: String,
    descriptor: String,
    anns: Vec<Ann>,
}

fn meth(name: &str, descriptor: &str, anns: Vec<Ann>) -> Meth {
    Meth {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        anns,
    }
}

/// Assemble a `.class`: `public class internal extends java/lang/Object`,
/// each method a `return`-only `Code` attribute with real descriptors
/// plus `RuntimeInvisibleAnnotations` (Mixin's CLASS retention) and the
/// class-level annotations.
fn build_mixin_class(internal: &str, class_anns: Vec<Ann>, methods: Vec<Meth>) -> Vec<u8> {
    let mut cp = Cp::default();
    cp.class(internal);
    cp.class("java/lang/Object");
    cp.utf8("Code");
    cp.utf8("RuntimeInvisibleAnnotations");
    for m in &methods {
        cp.utf8(&m.name);
        cp.utf8(&m.descriptor);
    }

    // Annotation attribute payloads first so the pool is complete.
    let mut method_attrs: Vec<Vec<(u16, Vec<u8>)>> = Vec::new();
    for m in &methods {
        let mut attrs = Vec::new();
        let mut code = Vec::new();
        code.extend(1u16.to_be_bytes()); // max_stack
        code.extend(2u16.to_be_bytes()); // max_locals
        code.extend(1u32.to_be_bytes()); // code_length
        code.push(0xB1); // return
        code.extend(0u16.to_be_bytes()); // exception_table_length
        code.extend(0u16.to_be_bytes()); // attributes_count
        attrs.push((cp.utf8("Code"), code));
        if !m.anns.is_empty() {
            let mut payload = (m.anns.len() as u16).to_be_bytes().to_vec();
            for a in &m.anns {
                write_ann(&mut payload, &mut cp, a);
            }
            attrs.push((cp.utf8("RuntimeInvisibleAnnotations"), payload));
        }
        method_attrs.push(attrs);
    }
    let mut class_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
    if !class_anns.is_empty() {
        let mut payload = (class_anns.len() as u16).to_be_bytes().to_vec();
        for a in &class_anns {
            write_ann(&mut payload, &mut cp, a);
        }
        class_attrs.push((cp.utf8("RuntimeInvisibleAnnotations"), payload));
    }

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // minor
    out.extend(61u16.to_be_bytes()); // major (Java 17)
    out.extend((cp.0.len() as u16 + 1).to_be_bytes()); // cp_count
    cp.encode(&mut out);
    out.extend(0x0021u16.to_be_bytes()); // public + super
    out.extend(cp.class_index(internal).to_be_bytes());
    out.extend(cp.class_index("java/lang/Object").to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces_count
    out.extend(0u16.to_be_bytes()); // fields_count
    out.extend((methods.len() as u16).to_be_bytes());
    for (m, attrs) in methods.iter().zip(&method_attrs) {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(cp.utf8_index(&m.name).to_be_bytes());
        out.extend(cp.utf8_index(&m.descriptor).to_be_bytes());
        out.extend((attrs.len() as u16).to_be_bytes());
        for (name_index, payload) in attrs {
            out.extend(name_index.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
    }
    out.extend((class_attrs.len() as u16).to_be_bytes());
    for (name_index, payload) in &class_attrs {
        out.extend(name_index.to_be_bytes());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    out
}

const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const ACCESSOR: &str = "Lorg/spongepowered/asm/mixin/gen/Accessor;";
const OVERWRITE: &str = "Lorg/spongepowered/asm/mixin/Overwrite;";

// ---------------------------------------------------------------------------
// Bounded-VM recording host
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EventHost {
    calls: Vec<HostCallKind>,
    cancelled: bool,
}

impl SubsetVmHost for EventHost {
    fn call(&mut self, kind: HostCallKind, args: &[VmValue]) -> Result<Option<VmValue>, VmError> {
        self.calls.push(kind);
        match kind {
            HostCallKind::EventGetPlayer
            | HostCallKind::EventGetEntity
            | HostCallKind::EventGetLevel
            | HostCallKind::EventGetPos
            | HostCallKind::EventGetBlockState
            | HostCallKind::EventGetServer
            | HostCallKind::EventGetDispatcher => Ok(Some(VmValue::Ref(7))),
            HostCallKind::EventIsCanceled => Ok(Some(VmValue::Int(i32::from(self.cancelled)))),
            HostCallKind::EventSetCanceled => {
                let VmValue::Int(v) = args[1] else {
                    panic!("setCanceled arg must be Int")
                };
                self.cancelled = v != 0;
                Ok(None)
            }
            HostCallKind::EventGetDamage => Ok(Some(VmValue::Float(4.0))),
            HostCallKind::EventSetDamage | HostCallKind::EventCancel => Ok(None),
            other => panic!("unexpected host call {other:?}"),
        }
    }
}

const ALOAD_0: &[u8] = &[0x2A];
const ICONST_1: &[u8] = &[0x04];
const POP: &[u8] = &[0x57];
const RETURN: &[u8] = &[0xB1];

fn invokevirtual(index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [0xB6, hi, lo]
}

fn code(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

// ---------------------------------------------------------------------------
// §4.1 — Real generic JAR data extraction
// ---------------------------------------------------------------------------

#[test]
fn generic_jar_extracts_all_six_1211_data_families_for_unknown_namespace() {
    let jar_bytes = six_family_jar();
    let data = extract_mod_data(&jar_bytes, "my_custom_mod.jar").expect("extract_mod_data");

    // Every family populated — zero empty fields.
    assert_eq!(data.namespaces, vec![NS.to_string()]);
    assert_eq!(data.recipes.len(), 1);
    assert_eq!(data.recipes[0].name, "my_custom_mod:infuse_gem");
    assert_eq!(data.recipes[0].recipe_type, "my_custom_mod:infusing");
    assert_eq!(
        data.tags.get("my_custom_mod:items/gems"),
        Some(&vec![
            "my_custom_mod:gem".to_string(),
            "my_custom_mod:raw_gem".to_string()
        ])
    );
    assert!(data
        .loot_tables
        .contains_key("my_custom_mod:blocks/gem_ore"));
    assert_eq!(data.dimensions.dimensions.len(), 1);
    assert_eq!(data.dimensions.dimension_types.len(), 1);
    assert_eq!(
        data.data_maps.keys(),
        vec![&("minecraft:item".to_string(), "furnace_fuels".to_string())]
    );
    let fuel_map = data
        .data_maps
        .get("minecraft:item", "furnace_fuels")
        .expect("furnace_fuels map");
    assert!(fuel_map.contains_key("my_custom_mod:gem_block"));
    assert_eq!(data.biome_modifiers.len(), 1);
    assert_eq!(data.biome_modifiers[0].id, "my_custom_mod:add_gem_ore");
    assert_eq!(data.biome_modifiers[0].kind, BiomeModifierKind::AddFeatures);
    // The malformed loot-table member is recorded, not silently dropped.
    assert_eq!(data.rejected_members.len(), 1);
    assert!(data.rejected_members[0].contains("corrupted.json"));

    // The full adaptation pipeline packages the same extraction.
    let result = auto_adapt_jar_bytes(&jar_bytes, "my_custom_mod.jar", &AutoAdaptOptions::default())
        .expect("auto_adapt_jar_bytes");
    assert_eq!(result.tier, AdaptationTier::NativeBundle);
    assert!(result.bundle.is_some());
    assert_eq!(result.data_bundle.namespaces, vec![NS.to_string()]);
    assert_eq!(result.data_bundle.recipes.len(), 1);
    assert_eq!(result.data_bundle.tags.len(), 1);
    assert_eq!(result.data_bundle.loot_tables.len(), 1);
    assert_eq!(result.data_bundle.dimensions.dimensions.len(), 1);
    assert_eq!(result.data_bundle.dimensions.dimension_types.len(), 1);
    assert_eq!(result.data_bundle.data_maps.keys().len(), 1);
    assert_eq!(result.data_bundle.biome_modifiers.len(), 1);
    // Deterministic: two runs of the same bytes produce identical output.
    let again = extract_mod_data(&jar_bytes, "my_custom_mod.jar").expect("extract_mod_data again");
    assert_eq!(format!("{:?}", data), format!("{:?}", again));

    // Fail-closed: a traversal member rejects the whole archive.
    let hostile = jar(&[
        ("fabric.mod.json", br#"{"schemaVersion":1,"id":"my_custom_mod"}"#),
        ("data/../escape.json", b"{}"),
    ]);
    assert!(extract_mod_data(&hostile, "hostile.jar").is_err());
    assert!(auto_adapt_jar_bytes(&hostile, "hostile.jar", &AutoAdaptOptions::default()).is_err());

    // Fail-closed: a truncated/corrupt archive is a hard error.
    let mut truncated = jar_bytes.clone();
    truncated.truncate(truncated.len() - 8);
    assert!(extract_mod_data(&truncated, "truncated.jar").is_err());
    assert!(extract_mod_data(b"not a zip", "garbage.jar").is_err());
}

// ---------------------------------------------------------------------------
// §4.2 — Real NeoForge/Fabric 1.21.1 `.class` VM event descriptors
// ---------------------------------------------------------------------------

#[test]
fn class_vm_recognizes_real_neoforge_and_fabric_1211_event_descriptors() {
    // The descriptor → hook classifier lowers real loader event names,
    // `L...;` descriptors, dotted names, and full method descriptors.
    let cases: &[(&str, StandardEventHook)] = &[
        (
            "Lnet/neoforged/neoforge/event/level/BlockEvent$BreakEvent;",
            StandardEventHook::BlockBreakPre,
        ),
        (
            "(Lnet/neoforged/neoforge/event/level/BlockEvent$BreakEvent;)V",
            StandardEventHook::BlockBreakPre,
        ),
        (
            "Lnet/neoforged/neoforge/event/tick/ServerTickEvent$Pre;",
            StandardEventHook::ServerTickPre,
        ),
        (
            "Lnet/neoforged/neoforge/event/tick/ServerTickEvent$Post;",
            StandardEventHook::ServerTickPost,
        ),
        (
            "Lnet/neoforged/neoforge/event/entity/player/PlayerInteractEvent$RightClickBlock;",
            StandardEventHook::BlockInteract,
        ),
        (
            "Lnet/neoforged/neoforge/event/entity/player/PlayerInteractEvent$RightClickItem;",
            StandardEventHook::ItemUse,
        ),
        (
            "net.neoforged.neoforge.event.entity.living.LivingDamageEvent$Pre",
            StandardEventHook::EntityDamagePre,
        ),
        (
            "Lnet/neoforged/neoforge/event/entity/living/LivingDeathEvent;",
            StandardEventHook::EntityDeath,
        ),
        (
            "Lnet/neoforged/neoforge/event/entity/EntityJoinLevelEvent;",
            StandardEventHook::EntityJoinLevel,
        ),
        (
            "Lnet/neoforged/neoforge/event/RegisterCommandsEvent;",
            StandardEventHook::CommandRegister,
        ),
        (
            "Lnet/fabricmc/fabric/api/event/player/UseBlockCallback;",
            StandardEventHook::BlockInteract,
        ),
        (
            "Lnet/fabricmc/fabric/api/event/lifecycle/ServerTickEvents$StartTick;",
            StandardEventHook::ServerTickPre,
        ),
        (
            "Lnet/fabricmc/fabric/api/event/lifecycle/ServerTickEvents$EndTick;",
            StandardEventHook::ServerTickPost,
        ),
        (
            "Lnet/fabricmc/fabric/api/event/player/AttackEntityCallback;",
            StandardEventHook::EntityAttack,
        ),
        (
            "Lnet/fabricmc/fabric/api/event/player/PlayerBlockBreakEvents$Before;",
            StandardEventHook::BlockBreakPre,
        ),
        (
            "Lnet/fabricmc/fabric/api/command/v2/CommandRegistrationCallback;",
            StandardEventHook::CommandRegister,
        ),
    ];
    for (descriptor, want) in cases {
        assert_eq!(
            event_descriptor_hook(descriptor),
            Some(*want),
            "descriptor {descriptor}"
        );
    }
    // Unknown descriptors fail closed — no guessed hook.
    assert_eq!(event_descriptor_hook("Lcom/example/Dream;"), None);
    assert_eq!(event_descriptor_hook("()V"), None);
    assert_eq!(event_descriptor_hook(""), None);

    // A real NeoForge handler: `onBreak(BlockEvent$BreakEvent)` calling
    // `getPos`, `isCanceled`, `setCanceled` — verify + bounded execute.
    let event_class = "net/neoforged/neoforge/event/level/BlockEvent$BreakEvent";
    let mut a = ClassAssembler::new("com/example/EventHooks");
    let get_pos = a.cp.method_ref(
        event_class,
        "getPos",
        "()Lnet/minecraft/core/BlockPos;",
    );
    let is_canceled = a.cp.method_ref(event_class, "isCanceled", "()Z");
    let set_canceled = a.cp.method_ref(event_class, "setCanceled", "(Z)V");
    let bytes = a.finish(&[MethodSpec::static_method(
        "onBreak",
        "(Lnet/neoforged/neoforge/event/level/BlockEvent$BreakEvent;)V",
        2,
        1,
        code(&[
            ALOAD_0,
            &invokevirtual(get_pos),
            POP,
            ALOAD_0,
            &invokevirtual(is_canceled),
            POP,
            ALOAD_0,
            ICONST_1,
            &invokevirtual(set_canceled),
            RETURN,
        ]),
    )]);
    let class = parse_class_methods(&bytes).expect("parse class");
    let method = verify_subset_method(
        &class,
        "onBreak",
        "(Lnet/neoforged/neoforge/event/level/BlockEvent$BreakEvent;)V",
    )
    .expect("verify handler");
    let mut host = EventHost::default();
    let result = execute_verified_method(
        &method,
        &[VmValue::Ref(1)],
        &mut host,
        DEFAULT_FUEL,
    )
    .expect("execute handler");
    assert!(result.return_value.is_none());
    assert_eq!(
        host.calls,
        vec![
            HostCallKind::EventGetPos,
            HostCallKind::EventIsCanceled,
            HostCallKind::EventSetCanceled,
        ]
    );
    assert!(host.cancelled);

    // Accessors stay scoped to the events that declare them: `getPos` on
    // a tick event is `UnknownHostCall`, not a guess.
    let tick_class = "net/neoforged/neoforge/event/tick/ServerTickEvent$Pre";
    let mut a = ClassAssembler::new("com/example/BadScope");
    let bad_pos = a.cp.method_ref(
        tick_class,
        "getPos",
        "()Lnet/minecraft/core/BlockPos;",
    );
    let bytes = a.finish(&[MethodSpec::static_method(
        "onTick",
        "(Lnet/neoforged/neoforge/event/tick/ServerTickEvent$Pre;)V",
        1,
        1,
        code(&[ALOAD_0, &invokevirtual(bad_pos), POP, RETURN]),
    )]);
    let class = parse_class_methods(&bytes).expect("parse bad scope");
    let err = verify_subset_method(
        &class,
        "onTick",
        "(Lnet/neoforged/neoforge/event/tick/ServerTickEvent$Pre;)V",
    )
    .expect_err("getPos on ServerTickEvent must not resolve");
    assert!(
        matches!(err, VmError::UnknownHostCall { .. }),
        "expected UnknownHostCall, got {err:?}"
    );

    // `getServer` on the tick event does resolve — the accessor table is
    // per-event.
    let mut a = ClassAssembler::new("com/example/TickHook");
    let get_server = a.cp.method_ref(
        tick_class,
        "getServer",
        "()Lnet/minecraft/server/MinecraftServer;",
    );
    let bytes = a.finish(&[MethodSpec::static_method(
        "onTick",
        "(Lnet/neoforged/neoforge/event/tick/ServerTickEvent$Pre;)V",
        1,
        1,
        code(&[ALOAD_0, &invokevirtual(get_server), POP, RETURN]),
    )]);
    let class = parse_class_methods(&bytes).expect("parse tick hook");
    let method = verify_subset_method(
        &class,
        "onTick",
        "(Lnet/neoforged/neoforge/event/tick/ServerTickEvent$Pre;)V",
    )
    .expect("verify tick handler");
    let mut host = EventHost::default();
    execute_verified_method(&method, &[VmValue::Ref(9)], &mut host, DEFAULT_FUEL)
        .expect("execute tick handler");
    assert_eq!(host.calls, vec![HostCallKind::EventGetServer]);

    // Fail-closed on unsupported opcodes: `invokedynamic` never verifies.
    let mut a = ClassAssembler::new("com/example/LambdaHook");
    a.cp.method_ref("java/lang/Object", "<init>", "()V");
    let bytes = a.finish(&[MethodSpec::static_method(
        "hook",
        "(Lnet/fabricmc/fabric/api/event/player/UseBlockCallback;)V",
        0,
        1,
        vec![0xBA, 0, 0, 0, 0, 0xB1], // invokedynamic + return
    )]);
    let class = parse_class_methods(&bytes).expect("parse invokedynamic class");
    let err = verify_subset_method(
        &class,
        "hook",
        "(Lnet/fabricmc/fabric/api/event/player/UseBlockCallback;)V",
    )
    .expect_err("invokedynamic must be rejected");
    assert!(
        matches!(err, VmError::DisallowedOpcode { opcode: 0xBA, .. }),
        "expected DisallowedOpcode(0xBA), got {err:?}"
    );

    // Fail-closed on forbidden classes: `Runtime.exec` never verifies.
    let mut a = ClassAssembler::new("com/example/EvilHook");
    let exec = a.cp.method_ref(
        "java/lang/Runtime",
        "exec",
        "(Ljava/lang/String;)Ljava/lang/Process;",
    );
    let bytes = a.finish(&[MethodSpec::static_method(
        "hook",
        "(Lnet/neoforged/neoforge/event/level/BlockEvent$BreakEvent;)V",
        2,
        2,
        code(&[ALOAD_0, ALOAD_0, &invokevirtual(exec), POP, RETURN]),
    )]);
    let class = parse_class_methods(&bytes).expect("parse evil class");
    let err = verify_subset_method(
        &class,
        "hook",
        "(Lnet/neoforged/neoforge/event/level/BlockEvent$BreakEvent;)V",
    )
    .expect_err("Runtime.exec must be rejected");
    assert!(
        matches!(err, VmError::ForbiddenHostClass { .. }),
        "expected ForbiddenHostClass, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// §4.4 — Safe read-only `@Accessor` lifting
// ---------------------------------------------------------------------------

#[test]
fn mixin_lifter_lifts_readonly_accessor_and_fails_closed_on_setter_or_overwrite() {
    // One accessor mixin class: `@Mixin(targets = "...Entity")` with a
    // real `getYRot()F` getter, a mutating `setYRot(F)V` setter, and an
    // unknown-field getter — plus an `@Overwrite` on a second mixin.
    let accessor_class = build_mixin_class(
        "com/example/mixin/EntityAccess",
        vec![ann(
            MIXIN,
            vec![(
                "targets",
                EV::Arr(vec![s("net.minecraft.world.entity.Entity")]),
            )],
        )],
        vec![
            meth(
                "getYRot",
                "()F",
                vec![ann(ACCESSOR, vec![("value", s("yRot"))])],
            ),
            meth(
                "setYRot",
                "(F)V",
                vec![ann(ACCESSOR, vec![("value", s("yRot"))])],
            ),
            meth(
                "getSecret",
                "()I",
                vec![ann(ACCESSOR, vec![("value", s("secretField"))])],
            ),
        ],
    );
    let sites = parse_mixin_class(&accessor_class).expect("parse accessor class");
    assert_eq!(sites.len(), 3);
    assert!(sites
        .iter()
        .all(|s| s.annotation == MixinAnnotationKind::Accessor));
    // The scanner records the handler name + real descriptor.
    let getter = sites.iter().find(|s| s.handler_name == "getYRot").unwrap();
    assert_eq!(getter.handler_descriptor, "()F");
    assert_eq!(getter.target_method, "yRot");

    let plan = lift_accessor_sites(&sites);
    assert_eq!(plan.bindings.len(), 1);
    assert_eq!(plan.bindings[0].field, "Entity#yRot");
    assert_eq!(plan.bindings[0].value_kind, AccessorValueKind::Float);
    assert_eq!(plan.bindings[0].tier, ExecutionTier::UniversalNative);
    assert_eq!(plan.rejected.len(), 2);
    let rejections: Vec<AccessorRejection> = plan
        .rejected
        .iter()
        .map(|(_, reason)| *reason)
        .collect();
    assert!(rejections.contains(&AccessorRejection::MutatingSetter));
    assert!(rejections.contains(&AccessorRejection::UnknownField));
    assert!(!plan.is_fully_lifted());
    assert_eq!(plan.tier(), ExecutionTier::RequiresFallback);

    // The injection lifter still reports the accessor sites honestly as
    // unsupported injector shapes — the accessor plan is a parallel
    // surface, not a silent swallow.
    let verdict = lift_mixin_sites(&sites);
    match &verdict {
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
            assert_eq!(unliftable.len(), 3);
            assert!(unliftable
                .iter()
                .all(|(_, r)| *r == UnliftableReason::UnsupportedAnnotation));
        }
        other => panic!("expected RequiresFallback, got {other:?}"),
    }

    // Every requested safe field lifts through the same path — build the
    // full table of read-only getters end-to-end.
    let all_safe = build_mixin_class(
        "com/example/mixin/SafeReads",
        vec![ann(
            MIXIN,
            vec![(
                "targets",
                EV::Arr(vec![
                    s("net.minecraft.world.entity.Entity"),
                    s("net.minecraft.world.entity.LivingEntity"),
                    s("net.minecraft.world.level.block.state.BlockState"),
                    s("net.minecraft.world.item.ItemStack"),
                    s("net.minecraft.server.level.ServerPlayer"),
                ]),
            )],
        )],
        vec![
            meth("getYRot", "()F", vec![ann(ACCESSOR, vec![("value", s("yRot"))])]),
            meth("getHealth", "()F", vec![ann(ACCESSOR, vec![("value", s("health"))])]),
            meth("getBlock", "()Lnet/minecraft/world/level/block/Block;", vec![ann(ACCESSOR, vec![("value", s("block"))])]),
            meth("getCount", "()I", vec![ann(ACCESSOR, vec![("value", s("count"))])]),
            meth("getGameMode", "()Lnet/minecraft/server/level/ServerPlayerGameMode;", vec![ann(ACCESSOR, vec![("value", s("gameMode"))])]),
        ],
    );
    let sites = parse_mixin_class(&all_safe).expect("parse safe class");
    let plan = lift_accessor_sites(&sites);
    // Five methods × five targets each emit a site per target; only the
    // (class, field) rows in the safe table lift — every other pairing
    // fails closed as UnknownField, never silently lifted.
    assert_eq!(sites.len(), 25);
    let lifted_fields: std::collections::BTreeSet<&str> = plan
        .bindings
        .iter()
        .map(|b| b.field.as_str())
        .collect();
    assert_eq!(
        lifted_fields,
        ["Entity#yRot", "LivingEntity#health", "BlockState#block", "ItemStack#count", "ServerPlayer#gameMode"]
            .into_iter()
            .collect()
    );
    assert!(!plan.is_fully_lifted());
    assert!(plan
        .rejected
        .iter()
        .all(|(_, r)| *r == AccessorRejection::UnknownField));

    // `@Overwrite` sites are not accessors at all — the lifter fails
    // closed with UnsupportedAnnotation and the accessor plan leaves
    // them to `lift_mixin_sites`.
    let overwrite_class = build_mixin_class(
        "com/example/mixin/OverwriteMixin",
        vec![ann(
            MIXIN,
            vec![(
                "targets",
                EV::Arr(vec![s("net.minecraft.world.entity.LivingEntity")]),
            )],
        )],
        vec![meth(
            "tick",
            "()V",
            vec![ann(OVERWRITE, vec![("method", s("tick"))])],
        )],
    );
    let sites = parse_mixin_class(&overwrite_class).expect("parse overwrite class");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].annotation, MixinAnnotationKind::Overwrite);
    let plan = lift_accessor_sites(&sites);
    assert!(plan.bindings.is_empty());
    assert!(plan.rejected.is_empty());
    match lift_mixin_sites(&sites) {
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
            assert_eq!(unliftable.len(), 1);
            assert_eq!(unliftable[0].1, UnliftableReason::UnsupportedAnnotation);
        }
        other => panic!("expected RequiresFallback, got {other:?}"),
    }
}
