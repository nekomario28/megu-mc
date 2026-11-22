// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Multi-mod compatibility archetype end-to-end suite (Issue #221).
//!
//! Historically only Seed Protect was verified as an end-to-end mod
//! archetype. This test loads **six** distinct archetypes together through
//! one `mods/` directory — a renamed performance mod, a NeoForge culinary
//! mod, a dimension+portal+progression mod, a liftable Mixin mod, a
//! standalone native `mc-spine-mod-bundle/1` Megu mod, and an invasive
//! coremod — through [`auto_adapt_mods_dir`] and then folds every emitted
//! bundle into [`UnifiedModRuntime`], the same composition the live
//! `mod-suite` boot path performs.
//!
//! Every fixture is built in-memory: real stored-ZIP JARs, a minimal TOML
//! `neoforge.mods.toml`, `fabric.mod.json` manifests, hand-assembled JVMS
//! §4 `.class` bytecode carrying `@Mixin`/`@Inject`/`@Overwrite`/`@Redirect`
//! annotations, datapack JSON, and declarative bundle documents.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use mod_adapter_core::auto_adapter::{
    auto_adapt_jar_bytes, auto_adapt_mods_dir, write_adaptation_outputs, AdaptationTier,
    AutoAdaptOptions, JarAdaptationResult, ModsDirAdaptationReport, ADAPTATION_REPORT_FILE,
};
use mod_adapter_core::data_loader::{parse_json, JsonValue};
use mod_adapter_core::dimension_mod::{
    detect_vertical_portal, DimensionModClassification, GeneratorKind,
};
use mod_adapter_core::ecosystem_compat::ModLoaderKind;
use mod_adapter_core::mixin_lifter::{MixinLiftVerdict, StandardEventHook};
use mod_adapter_core::unified_bundle::{DeclarativeModBundle, UnifiedModRuntime};
use mod_adapter_core::version_compat::McVersion;
use mod_adapter_core::world_extensions::ProgressionVerdict;

// ---------------------------------------------------------------------------
// Scratch directories (unique per test, removed on drop)
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mc-spine-multi-mod-{}-{}-{tag}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Stored-ZIP JAR builder (real central directory + EOCD)
// ---------------------------------------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Pack `(name, bytes)` members into a real stored-method ZIP archive.
fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;

        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name);
        out.extend(*data);

        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(10u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_be_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// Minimal .class builder (@Mixin class + annotated ()V methods)
// ---------------------------------------------------------------------------

enum CpItem {
    Utf8(String),
    Class(u16),
    Int(i32),
}

#[derive(Default)]
struct Cp(Vec<CpItem>);

impl Cp {
    fn push(&mut self, item: CpItem) -> u16 {
        self.0.push(item);
        self.0.len() as u16
    }
    fn utf8(&mut self, s: &str) -> u16 {
        self.push(CpItem::Utf8(s.to_string()))
    }
    fn class(&mut self, internal_name: &str) -> u16 {
        let name_index = self.utf8(internal_name);
        self.push(CpItem::Class(name_index))
    }
    fn int(&mut self, v: i32) -> u16 {
        self.push(CpItem::Int(v))
    }
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.0 {
            match item {
                CpItem::Utf8(s) => {
                    out.push(1);
                    out.extend((s.len() as u16).to_be_bytes());
                    out.extend(s.as_bytes());
                }
                CpItem::Class(name_index) => {
                    out.push(7);
                    out.extend(name_index.to_be_bytes());
                }
                CpItem::Int(v) => {
                    out.push(3);
                    out.extend(v.to_be_bytes());
                }
            }
        }
        out
    }
}

enum Ev {
    S(String),
    Z(bool),
    Nested(&'static str, Vec<(&'static str, Ev)>),
    Arr(Vec<Ev>),
}

fn ev_s(v: &str) -> Ev {
    Ev::S(v.to_string())
}

fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &Ev) {
    match ev {
        Ev::S(v) => {
            out.push(b's');
            out.extend(cp.utf8(v).to_be_bytes());
        }
        Ev::Z(b) => {
            out.push(b'Z');
            out.extend(cp.int(i32::from(*b)).to_be_bytes());
        }
        Ev::Nested(desc, elems) => {
            out.push(b'@');
            write_ann(out, cp, desc, elems);
        }
        Ev::Arr(items) => {
            out.push(b'[');
            out.extend((items.len() as u16).to_be_bytes());
            for item in items {
                write_ev(out, cp, item);
            }
        }
    }
}

fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, desc: &str, elems: &[(&str, Ev)]) {
    out.extend(cp.utf8(desc).to_be_bytes());
    out.extend((elems.len() as u16).to_be_bytes());
    for (name, ev) in elems {
        out.extend(cp.utf8(name).to_be_bytes());
        write_ev(out, cp, ev);
    }
}

const MIXIN_DESC: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const INJECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
const AT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/At;";
const OVERWRITE_DESC: &str = "Lorg/spongepowered/asm/mixin/Overwrite;";
const REDIRECT_DESC: &str = "Lorg/spongepowered/asm/mixin/injection/Redirect;";

fn at(value: &str) -> Ev {
    Ev::Nested(AT_DESC, vec![("value", ev_s(value))])
}

struct FixtureMethod {
    name: &'static str,
    ann_desc: &'static str,
    elems: Vec<(&'static str, Ev)>,
}

/// Minimal structurally-valid `.class`: public class `internal` extending
/// `java/lang/Object`, each method `()V` carrying a single
/// `RuntimeInvisibleAnnotations` annotation, plus a class-level
/// `@Mixin(targets=[...])`.
fn build_mixin_class(internal: &str, targets: &[&str], methods: &[FixtureMethod]) -> Vec<u8> {
    let mut cp = Cp::default();
    let this_idx = cp.class(internal);
    let super_idx = cp.class("java/lang/Object");
    let rva_idx = cp.utf8("RuntimeInvisibleAnnotations");
    let desc_idx = cp.utf8("()V");

    let mut mixin_payload = 1u16.to_be_bytes().to_vec();
    write_ann(
        &mut mixin_payload,
        &mut cp,
        MIXIN_DESC,
        &[(
            "targets",
            Ev::Arr(targets.iter().map(|t| ev_s(t)).collect()),
        )],
    );

    let mut method_parts = Vec::new();
    for m in methods {
        let name_idx = cp.utf8(m.name);
        let mut payload = 1u16.to_be_bytes().to_vec();
        write_ann(&mut payload, &mut cp, m.ann_desc, &m.elems);
        method_parts.push((name_idx, payload));
    }

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(((cp.0.len() + 1) as u16).to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(this_idx.to_be_bytes());
    out.extend(super_idx.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces
    out.extend(0u16.to_be_bytes()); // fields
    out.extend((methods.len() as u16).to_be_bytes());
    for (name_idx, payload) in &method_parts {
        out.extend(0x0001u16.to_be_bytes());
        out.extend(name_idx.to_be_bytes());
        out.extend(desc_idx.to_be_bytes());
        out.extend(1u16.to_be_bytes());
        out.extend(rva_idx.to_be_bytes());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    out.extend(1u16.to_be_bytes());
    out.extend(rva_idx.to_be_bytes());
    out.extend((mixin_payload.len() as u32).to_be_bytes());
    out.extend(mixin_payload);
    out
}

/// A plain `.class` with no annotations at all — real bytecode with an
/// empty mixin surface.
fn build_plain_class(internal: &str) -> Vec<u8> {
    let mut cp = Cp::default();
    let this_idx = cp.class(internal);
    let super_idx = cp.class("java/lang/Object");
    let desc_idx = cp.utf8("()V");
    let name_idx = cp.utf8("init");
    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(((cp.0.len() + 1) as u16).to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(this_idx.to_be_bytes());
    out.extend(super_idx.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces
    out.extend(0u16.to_be_bytes()); // fields
    out.extend(1u16.to_be_bytes()); // one method, no attributes
    out.extend(0x0001u16.to_be_bytes());
    out.extend(name_idx.to_be_bytes());
    out.extend(desc_idx.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // no class attributes
    out
}

// ---------------------------------------------------------------------------
// The six archetype fixtures
// ---------------------------------------------------------------------------

const PERF_JAR: &str = "my-fast-render.jar";
const CULINARY_JAR: &str = "hearthcraft-neoforge-1.4.2.jar";
const DIMENSION_JAR: &str = "aether-frontier-2.0.jar";
const MIXIN_JAR: &str = "guard-events-1.0.jar";
const COREMOD_JAR: &str = "invasive-coremod-1.0.jar";
const NATIVE_BUNDLE_FILE: &str = "custom_creatures.bundle.json";

fn fabric_manifest(id: &str, version: &str, extra: &str) -> String {
    format!(r#"{{"schemaVersion":1,"id":"{id}","version":"{version}","environment":"*"{extra}}}"#)
}

/// Archetype 1 — a performance mod renamed to hide its identity:
/// `my-fast-render.jar` declares `id: "sodium"`, so filename inference
/// (`my-fast-render`) finds nothing; only the manifest-declared id catches
/// it. Its `.class` member is valid bytecode (so the security-first static
/// constant-pool check passes), while `mixin_scan` stays `None` because the
/// superseded skip still bypasses Mixin lifting.
fn perf_mod_jar() -> Vec<u8> {
    let manifest = fabric_manifest("sodium", "0.6.13", "");
    let class = build_plain_class("com/fastsodium/RenderStub");
    jar(&[
        ("fabric.mod.json", manifest.as_bytes()),
        ("com/fastsodium/RenderStub.class", &class),
    ])
}

/// Archetype 2 — a NeoForge culinary/farming mod: `neoforge.mods.toml`
/// manifest plus `*:cooking` and `*:cutting` recipes, item and block tags,
/// and a `*:add_item` loot modifier under `data/hearthcraft/`.
fn culinary_mod_jar() -> Vec<u8> {
    let mods_toml = br#"modLoader = "javafml"
loaderVersion = "[1,)"
license = "MIT"

[[mods]]
modId = "hearthcraft"
version = "1.4.2"

[[dependencies.hearthcraft]]
modId = "minecraft"
type = "required"
versionRange = "[1.21,1.22)"
side = "BOTH"

[[dependencies.hearthcraft]]
modId = "neoforge"
type = "required"
versionRange = "[21.1,)"
side = "BOTH"
"#;
    jar(&[
        ("META-INF/neoforge.mods.toml", mods_toml.as_slice()),
        (
            "com/hearthcraft/HearthcraftMod.class",
            build_plain_class("com/hearthcraft/HearthcraftMod").as_slice(),
        ),
        (
            "data/hearthcraft/recipe/pot_roast.json",
            br#"{"type":"hearthcraft:cooking","ingredients":[{"item":"minecraft:beef"},{"item":"hearthcraft:onion"},{"tag":"hearthcraft:vegetables"}],"result":{"id":"hearthcraft:pot_roast","count":1},"container":{"item":"minecraft:bowl"},"cookingtime":240,"experience":0.6}"#,
        ),
        (
            "data/hearthcraft/recipe/roast_slices.json",
            br#"{"type":"hearthcraft:cutting","ingredients":[{"item":"hearthcraft:pot_roast"}],"tool":{"item":"hearthcraft:iron_knife"},"result":[{"item":"hearthcraft:roast_slice","count":3}]}"#,
        ),
        (
            "data/hearthcraft/tags/items/vegetables.json",
            br#"{"replace":false,"values":["hearthcraft:onion","minecraft:carrot","minecraft:potato"]}"#,
        ),
        (
            "data/hearthcraft/tags/blocks/heat_sources.json",
            br#"{"replace":false,"values":["minecraft:fire","minecraft:campfire","hearthcraft:stove"]}"#,
        ),
        (
            "data/hearthcraft/loot_modifiers/seeds_from_grass.json",
            br#"{"type":"hearthcraft:add_item","item":"hearthcraft:wild_seeds","chance":0.4,"conditions":[{"condition":"minecraft:loot_table_id","loot_table_id":"minecraft:blocks/short_grass"}]}"#,
        ),
    ])
}

/// The `mc-spine-mod-bundle/1` document embedded inside archetype 3's JAR:
/// one vertical-frame portal into `aether_demo:skyreach` plus a progression
/// barrier sealing `aether_demo:storm_peaks` until an advancement is held.
fn aether_bundle_json() -> &'static str {
    r#"{
  "schema": "mc-spine-mod-bundle/1",
  "mod_id": "aether_demo",
  "mc_version_range": "[1.21,)",
  "portals": [
    {
      "name": "aether_demo:skyreach_portal",
      "target_dim": "aether_demo:skyreach",
      "teleport_grace_ticks": 30,
      "trigger": {
        "kind": "vertical_frame",
        "frame_block": "aether_demo:sky_stone",
        "igniter_item": "aether_demo:sky_shard",
        "min_width": 2,
        "min_height": 3,
        "max_width": 9,
        "max_height": 9,
        "portal_block": "aether_demo:skyreach_portal"
      }
    }
  ],
  "progression_barriers": [
    {
      "id": "aether_demo:storm_lock",
      "dimension": "aether_demo:skyreach",
      "target_biomes": ["aether_demo:storm_peaks"],
      "required_advancements": ["aether_demo:enter_skyreach"],
      "block_break_protected": true,
      "block_place_protected": true,
      "enforce_effects": [
        {"effect": "minecraft:slowness", "duration_ticks": 100, "amplifier": 1}
      ],
      "periodic_damage": {"hearts": 0.5, "interval_ticks": 40}
    }
  ],
  "biome_regions": [],
  "creatures": [],
  "custom_channels": ["aether_demo:skyreach_sync"]
}"#
}

/// Archetype 3 — a dimension + portal + progression Fabric mod: a real
/// `dimension_type`/`dimension` pair discovered as pure data-driven, plus
/// an embedded [`auto_adapter::EMBEDDED_BUNDLE_MEMBER`] document installed
/// verbatim into the unified runtime.
fn dimension_mod_jar() -> Vec<u8> {
    let manifest = fabric_manifest("aether_demo", "2.0.0", "");
    jar(&[
        ("fabric.mod.json", manifest.as_bytes()),
        (
            "com/aetherdemo/AetherDemo.class",
            build_plain_class("com/aetherdemo/AetherDemo").as_slice(),
        ),
        (
            "data/aether_demo/dimension_type/skyreach.json",
            br#"{"min_y":0,"height":256,"has_skylight":true,"has_ceiling":false,"natural":true,"ambient_light":0.7,"effects":"minecraft:overworld","coordinate_scale":1.0}"#,
        ),
        (
            "data/aether_demo/dimension/skyreach.json",
            br#"{"type":"aether_demo:skyreach","generator":{"type":"minecraft:noise","settings":"aether_demo:skyreach_noise","biome_source":{"type":"minecraft:fixed","biome":"aether_demo:storm_peaks"}}}"#,
        ),
        (
            "data/aether_demo/tags/blocks/ores.json",
            br#"{"replace":false,"values":["aether_demo:zephyr_ore","aether_demo:cloudium_ore"]}"#,
        ),
        ("mc-spine-bundle.json", aether_bundle_json().as_bytes()),
    ])
}

/// Archetype 4 — a liftable Mixin Fabric mod: two registered mixin classes
/// whose `@Inject` sites bind `EntityDamagePre` (cancellable `HEAD` on
/// `LivingEntity.hurt`) and `PlayerJoin` (`TAIL` on
/// `PlayerList.placeNewPlayer`).
fn liftable_mixin_jar() -> Vec<u8> {
    let manifest = fabric_manifest("guard_events", "1.0.0", r#","mixins":["guard.mixins.json"]"#);
    let config = br#"{
        "required": true,
        "package": "com.guard.mixin",
        "compatibilityLevel": "JAVA_17",
        "mixins": ["LivingEntityGuardMixin", "PlayerListGuardMixin"]
    }"#;
    let damage = build_mixin_class(
        "com/guard/mixin/LivingEntityGuardMixin",
        &["net.minecraft.world.entity.LivingEntity"],
        &[FixtureMethod {
            name: "guard$onHurt",
            ann_desc: INJECT_DESC,
            elems: vec![
                ("method", ev_s("hurt")),
                ("at", at("HEAD")),
                ("cancellable", Ev::Z(true)),
            ],
        }],
    );
    let join = build_mixin_class(
        "com/guard/mixin/PlayerListGuardMixin",
        &["net.minecraft.server.players.PlayerList"],
        &[FixtureMethod {
            name: "guard$onJoin",
            ann_desc: INJECT_DESC,
            elems: vec![("method", ev_s("placeNewPlayer")), ("at", at("TAIL"))],
        }],
    );
    jar(&[
        ("fabric.mod.json", manifest.as_bytes()),
        ("guard.mixins.json", config.as_slice()),
        (
            "com/guard/mixin/LivingEntityGuardMixin.class",
            damage.as_slice(),
        ),
        ("com/guard/mixin/PlayerListGuardMixin.class", join.as_slice()),
        (
            "com/guard/GuardMod.class",
            build_plain_class("com/guard/GuardMod").as_slice(),
        ),
    ])
}

/// Archetype 6 — an invasive coremod: `@Overwrite` on `ServerLevel.tick`
/// plus a `@Redirect` on `LivingEntity.hurt`. Both annotations are
/// categorically unliftable, so the mod lands at Tier 3 with the exact
/// offending class/method named in the blockers.
fn coremod_jar() -> Vec<u8> {
    let manifest = fabric_manifest(
        "invasive_coremod",
        "0.9.9",
        r#","mixins":["coremod.mixins.json"]"#,
    );
    let config = br#"{
        "package": "com.coremod.mixin",
        "mixins": ["ServerLevelCoreMixin"]
    }"#;
    let class = build_mixin_class(
        "com/coremod/mixin/ServerLevelCoreMixin",
        &["net.minecraft.server.level.ServerLevel"],
        &[
            FixtureMethod {
                // @Overwrite targets by handler name: `tick` replaces the
                // vanilla ServerLevel#tick body outright.
                name: "tick",
                ann_desc: OVERWRITE_DESC,
                elems: vec![],
            },
            FixtureMethod {
                name: "coremod$redirectTick",
                ann_desc: REDIRECT_DESC,
                elems: vec![
                    ("method", ev_s("tick")),
                    (
                        "at",
                        Ev::Nested(
                            AT_DESC,
                            vec![
                                ("value", ev_s("INVOKE")),
                                (
                                    "target",
                                    ev_s("net/minecraft/server/level/ServerLevel.tickBlocks"),
                                ),
                            ],
                        ),
                    ),
                ],
            },
        ],
    );
    jar(&[
        ("fabric.mod.json", manifest.as_bytes()),
        ("coremod.mixins.json", config.as_slice()),
        ("com/coremod/mixin/ServerLevelCoreMixin.class", class.as_slice()),
    ])
}

/// Archetype 5 — a standalone native Megu mod: a plain
/// `mc-spine-mod-bundle/1` document dropped next to the JARs, declaring
/// custom content (tame/heal item ids inside a creature spec), a
/// progression barrier, a biome region, and custom payload channels.
fn native_bundle_json() -> &'static str {
    r##"{
  "schema": "mc-spine-mod-bundle/1",
  "mod_id": "custom_creatures",
  "mc_version_range": "[1.21,)",
  "portals": [],
  "progression_barriers": [
    {
      "id": "custom_creatures:wilds_lock",
      "dimension": "minecraft:overworld",
      "target_biomes": ["custom_creatures:moss_fields"],
      "required_advancements": ["custom_creatures:spot_mossback"],
      "block_break_protected": true,
      "block_place_protected": false,
      "enforce_effects": [
        {"effect": "minecraft:mining_fatigue", "duration_ticks": 80, "amplifier": 2}
      ],
      "periodic_damage": null
    }
  ],
  "biome_regions": [
    {
      "mod_id": "custom_creatures",
      "dimension": "minecraft:overworld",
      "weight": 900,
      "biome_overrides": {
        "minecraft:plains": "custom_creatures:moss_fields"
      }
    }
  ],
  "creatures": [
    {
      "entity_type": "custom_creatures:mossback",
      "max_health": 26.0,
      "tame_items": ["custom_creatures:moss_biscuit", "#custom_creatures:mossback_treats"],
      "tame_chance": 0.45,
      "breed_items": ["custom_creatures:moss_biscuit"],
      "heal_items": [{"item": "custom_creatures:moss_stew", "heal": 4.0}],
      "supports_wander_mode": true,
      "can_shoulder_perch": false,
      "rideable_when_tamed": false,
      "movement": {"kind": "amphibious", "land_speed": 0.12, "swim_speed": 0.5}
    }
  ],
  "custom_channels": ["custom_creatures:sync_v1"]
}"##
}

/// Write all six archetypes into one `mods/` directory and run the full
/// zero-JVM adaptation pass over it.
fn populate_mods_dir(mods: &Path) {
    fs::create_dir_all(mods).expect("create mods dir");
    fs::write(mods.join(PERF_JAR), perf_mod_jar()).expect("perf jar");
    fs::write(mods.join(CULINARY_JAR), culinary_mod_jar()).expect("culinary jar");
    fs::write(mods.join(DIMENSION_JAR), dimension_mod_jar()).expect("dimension jar");
    fs::write(mods.join(MIXIN_JAR), liftable_mixin_jar()).expect("mixin jar");
    fs::write(mods.join(COREMOD_JAR), coremod_jar()).expect("coremod jar");
    fs::write(mods.join(NATIVE_BUNDLE_FILE), native_bundle_json()).expect("bundle file");
}

fn adapt(mods: &Path) -> ModsDirAdaptationReport {
    auto_adapt_mods_dir(mods, &AutoAdaptOptions::default()).expect("mods dir adaptation must succeed")
}

fn jar_named<'a>(report: &'a ModsDirAdaptationReport, name: &str) -> &'a JarAdaptationResult {
    report
        .jars
        .iter()
        .find(|j| j.jar == name)
        .unwrap_or_else(|| panic!("{name} missing from adaptation report"))
}

fn lifted_hooks(result: &JarAdaptationResult) -> Vec<StandardEventHook> {
    let Some(scan) = &result.mixin_scan else {
        return Vec::new();
    };
    match &scan.verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => {
            bindings.iter().map(|b| b.hook).collect()
        }
        MixinLiftVerdict::RequiresFallback { lifted, .. } => {
            lifted.iter().map(|b| b.hook).collect()
        }
    }
}

fn json_field<'a>(value: &'a JsonValue, key: &str) -> &'a JsonValue {
    value
        .as_object()
        .expect("JSON object")
        .get(key)
        .unwrap_or_else(|| panic!("missing field {key}"))
}

fn json_text<'a>(value: &'a JsonValue, key: &str) -> &'a str {
    json_field(value, key).as_str().expect("JSON string")
}

fn json_count(value: &JsonValue, key: &str) -> u64 {
    json_field(value, key).as_u64().expect("JSON integer")
}

// ---------------------------------------------------------------------------
// The six-archetype mods dir adapts end to end
// ---------------------------------------------------------------------------

#[test]
fn six_archetypes_adapt_together_in_one_mods_dir() {
    let scratch = Scratch::new("six-mods");
    let mods = scratch.path("mods");
    populate_mods_dir(&mods);
    let report = adapt(&mods);

    // Five JARs classified; the standalone bundle is a counted non-JAR
    // entry, not a silent drop.
    assert_eq!(report.jars.len(), 5);
    assert_eq!(report.non_jar_entries, 1);
    assert_eq!(report.tier_count(AdaptationTier::AutoSkipped), 1);
    assert_eq!(report.tier_count(AdaptationTier::NativeBundle), 2);
    assert_eq!(report.tier_count(AdaptationTier::Tier2LiftedShim), 1);
    assert_eq!(report.tier_count(AdaptationTier::Tier3FallbackRequired), 1);
    assert!(report.requires_fallback());
    assert!(report.audit_notes.is_empty(), "duplicate ids: {:?}", report.audit_notes);

    // --- Archetype 1: renamed perf mod -> AutoSkipped before bytecode. ---
    let perf = jar_named(&report, PERF_JAR);
    assert_eq!(perf.tier, AdaptationTier::AutoSkipped);
    assert_eq!(perf.mod_id(), Some("sodium"));
    // Skip happened at manifest-id inspection: no mixin scan, no culinary,
    // and the deliberately corrupt `.class` member never aborted the pass.
    assert!(perf.mixin_scan.is_none());
    assert!(perf.culinary.is_none());
    assert!(
        perf.audit_notes
            .iter()
            .any(|n| n.contains("sodium") && n.contains("BUILTIN_SUPERSEDED_PERF_MODS")),
        "skip audit note: {:?}",
        perf.audit_notes
    );

    // --- Archetype 2: NeoForge culinary -> NativeBundle, data found. ---
    let culinary = jar_named(&report, CULINARY_JAR);
    assert_eq!(culinary.tier, AdaptationTier::NativeBundle);
    assert_eq!(culinary.mod_id(), Some("hearthcraft"));
    let desc = culinary.descriptor.as_ref().expect("neoforge descriptor");
    assert_eq!(desc.loader, ModLoaderKind::NeoForge);
    let stats = culinary.culinary.as_ref().expect("culinary stats");
    assert_eq!(stats.cooking_recipes, 1);
    assert_eq!(stats.cutting_recipes, 1);
    assert_eq!(stats.loot_modifiers, 1);
    assert_eq!(stats.tag_files, 2);
    assert_eq!(stats.tag_values, 6);
    // The synthesized bundle names the manifest id and stays section-empty.
    let bundle = culinary.bundle.as_ref().expect("synthesized bundle");
    assert_eq!(bundle.mod_id, "hearthcraft");
    assert!(bundle.portals.is_empty() && bundle.creatures.is_empty());
    assert!(culinary.blockers.is_empty());

    // --- Archetype 3: dimension+portal+progression -> NativeBundle. ---
    let dim = jar_named(&report, DIMENSION_JAR);
    assert_eq!(dim.tier, AdaptationTier::NativeBundle);
    assert_eq!(dim.mod_id(), Some("aether_demo"));
    let dim_data = dim.dimensions.as_ref().expect("dimension discovery");
    assert!(dim_data.is_dimension_mod());
    assert_eq!(
        dim_data.classification(),
        DimensionModClassification::PureDataDriven
    );
    assert_eq!(dim_data.dimension_types.len(), 1);
    assert_eq!(dim_data.dimension_types[0].id, "aether_demo:skyreach");
    assert_eq!(dim_data.dimensions.len(), 1);
    assert_eq!(dim_data.dimensions[0].id, "aether_demo:skyreach");
    assert_eq!(dim_data.dimensions[0].generator_kind, GeneratorKind::Noise);
    assert_eq!(
        dim_data.ore_blocks,
        vec![
            "aether_demo:cloudium_ore".to_string(),
            "aether_demo:zephyr_ore".to_string()
        ]
    );
    assert!(dim_data.skipped_malformed.is_empty());
    assert!(dim.data.embedded_bundle);
    // The embedded document won over synthesis verbatim.
    let bundle = dim.bundle.as_ref().expect("embedded bundle installed");
    assert_eq!(bundle.mod_id, "aether_demo");
    assert_eq!(bundle.portals.len(), 1);
    assert_eq!(bundle.portals[0].name, "aether_demo:skyreach_portal");
    assert_eq!(bundle.portals[0].target_dim, "aether_demo:skyreach");
    assert_eq!(bundle.progression_barriers.len(), 1);
    assert_eq!(
        bundle.progression_barriers[0].id,
        "aether_demo:storm_lock"
    );
    assert_eq!(bundle.custom_channels, ["aether_demo:skyreach_sync"]);

    // --- Archetype 4: liftable mixin -> Tier2LiftedShim, hooks bound. ---
    let mixin = jar_named(&report, MIXIN_JAR);
    assert_eq!(mixin.tier, AdaptationTier::Tier2LiftedShim);
    assert_eq!(mixin.mod_id(), Some("guard_events"));
    let scan = mixin.mixin_scan.as_ref().expect("mixin scan");
    assert_eq!(scan.mixin_configs_found, 1);
    assert_eq!(scan.classes_scanned, 2);
    assert_eq!(scan.sites.len(), 2);
    let MixinLiftVerdict::FullyLiftable(bindings) = &scan.verdict else {
        panic!("guard_events must fully lift: {:?}", scan.verdict);
    };
    assert_eq!(bindings.len(), 2);
    let mut hooks = lifted_hooks(mixin);
    hooks.sort();
    assert_eq!(
        hooks,
        vec![StandardEventHook::EntityDamagePre, StandardEventHook::PlayerJoin]
    );
    assert!(
        bindings
            .iter()
            .any(|b| b.hook == StandardEventHook::EntityDamagePre && b.cancellable),
        "EntityDamagePre must carry the cancellable flag"
    );
    assert!(
        scan.sites.iter().any(|s| {
            s.mixin_class == "com/guard/mixin/PlayerListGuardMixin"
                && s.target_method == "placeNewPlayer"
        }),
        "PlayerJoin site recorded: {:?}",
        scan.sites
    );
    assert!(mixin.bundle.is_none(), "Tier 2 emits no native bundle");

    // --- Archetype 6: invasive coremod -> Tier 3 with exact blockers. ---
    let core = jar_named(&report, COREMOD_JAR);
    assert_eq!(core.tier, AdaptationTier::Tier3FallbackRequired);
    assert_eq!(core.mod_id(), Some("invasive_coremod"));
    assert!(core.requires_fallback());
    assert_eq!(core.blockers.len(), 2, "blockers: {:?}", core.blockers);
    // Exact human-readable offending class/method — the operator sees the
    // mixin class, the vanilla target, the annotation, and the reason.
    assert!(
        core.blockers.iter().any(|b| {
            b.contains("com/coremod/mixin/ServerLevelCoreMixin")
                && b.contains("net.minecraft.server.level.ServerLevel#tick")
                && b.contains("@Overwrite")
                && b.contains("UnsupportedAnnotation")
        }),
        "@Overwrite blocker: {:?}",
        core.blockers
    );
    assert!(
        core.blockers.iter().any(|b| {
            b.contains("ServerLevelCoreMixin")
                && b.contains("@Redirect")
                && b.contains("INVOKE")
                && b.contains("UnsupportedAnnotation")
        }),
        "@Redirect blocker: {:?}",
        core.blockers
    );
    // The format_human summary survives a Tier 3 row too.
    assert!(core.format_human().contains("tier3_fallback_required"));

    // --- Archetype 5 + bundles: the unified runtime folds every emitted
    // bundle exactly the way live boot does: standalone *.bundle.json files
    // first, then per-JAR results in report order. ---
    let standalone = DeclarativeModBundle::from_json(native_bundle_json())
        .expect("standalone Megu bundle parses");
    assert_eq!(standalone.mod_id, "custom_creatures");
    assert_eq!(standalone.creatures.len(), 1);
    assert_eq!(standalone.custom_channels, ["custom_creatures:sync_v1"]);

    let mut runtime = UnifiedModRuntime::default();
    let install = runtime.register_bundle(&standalone, McVersion::V1_21_1);
    let summary = install.expect("standalone bundle installs on 1.21.1");
    assert_eq!(summary.mod_id, "custom_creatures");
    assert_eq!(summary.creatures_installed, 1);
    assert_eq!(summary.progression_barriers_installed, 1);
    assert_eq!(summary.biome_regions_installed, 1);
    assert_eq!(summary.custom_channels_newly_registered, 1);
    for jar in &report.jars {
        if let Some(bundle) = &jar.bundle {
            runtime
                .register_bundle(bundle, McVersion::V1_21_1)
                .unwrap_or_else(|e| panic!("{} bundle must install: {e}", jar.jar));
        }
    }
    assert_eq!(
        runtime.installed_mod_ids(),
        &[
            "custom_creatures".to_string(),
            "aether_demo".to_string(),
            "hearthcraft".to_string()
        ]
    );

    // Portal from the embedded bundle is live in the shared registry and
    // really detects a frame.
    let rule = runtime
        .portal_registry()
        .find("aether_demo:skyreach_portal")
        .expect("installed skyreach portal rule");
    let mut world: BTreeMap<(i32, i32, i32), String> = BTreeMap::new();
    for dy in -1..=3 {
        for da in -1..=2 {
            let on_frame = dy == -1 || dy == 3 || da == -1 || da == 2;
            world.insert(
                (3 + da, 64 + dy, 7),
                if on_frame {
                    "aether_demo:sky_stone".to_string()
                } else {
                    "minecraft:air".to_string()
                },
            );
        }
    }
    let block_at = |x: i32, y: i32, z: i32| {
        Some(
            world
                .get(&(x, y, z))
                .cloned()
                .unwrap_or_else(|| "minecraft:air".to_string()),
        )
    };
    let transition =
        detect_vertical_portal(rule, [4, 65, 7], &block_at).expect("frame must activate");
    assert_eq!(transition.target_dim, "aether_demo:skyreach");
    assert_eq!(transition.cell_count, 6);

    // Both progression barriers are live: a fresh player is locked, an
    // advancement holder walks through.
    let engine = runtime.progression_engine();
    let no_adv = BTreeSet::new();
    let locked = engine.check_progression(
        "aether_demo:skyreach",
        "aether_demo:storm_peaks",
        &no_adv,
        false,
    );
    let ProgressionVerdict::Locked {
        rule_id,
        missing_advancements,
        periodic_damage,
        ..
    } = locked
    else {
        panic!("storm_peaks must be locked for a fresh player");
    };
    assert_eq!(rule_id, "aether_demo:storm_lock");
    assert_eq!(missing_advancements, ["aether_demo:enter_skyreach"]);
    assert_eq!(periodic_damage, Some((0.5, 40)));
    assert_eq!(
        engine.check_progression(
            "aether_demo:skyreach",
            "aether_demo:storm_peaks",
            &BTreeSet::from(["aether_demo:enter_skyreach".to_string()]),
            false,
        ),
        ProgressionVerdict::Allowed
    );
    let locked = engine.check_progression(
        "minecraft:overworld",
        "custom_creatures:moss_fields",
        &no_adv,
        false,
    );
    assert!(
        matches!(
            locked,
            ProgressionVerdict::Locked {
                block_break_blocked: true,
                block_place_blocked: false,
                ..
            }
        ),
        "wilds_lock must gate moss_fields: {locked:?}"
    );

    // The Megu creature spec, biome region and channels registered too.
    let spec = runtime
        .creature_spec("custom_creatures:mossback")
        .expect("mossback spec installed");
    assert_eq!(spec.max_health, 26.0);
    assert!(spec.tame_items.contains(&"#custom_creatures:mossback_treats".to_string()));
    assert!(runtime.is_channel_registered("custom_creatures:sync_v1"));
    assert!(runtime.is_channel_registered("aether_demo:skyreach_sync"));
    let mut saw_region = false;
    for qx in 0..64 {
        if runtime
            .biome_blender()
            .winning_region_id(7, "minecraft:overworld", qx, 0)
            == "custom_creatures"
        {
            saw_region = true;
            assert_eq!(
                runtime.biome_blender().resolve_biome(
                    7,
                    "minecraft:overworld",
                    qx,
                    0,
                    "minecraft:plains"
                ),
                "custom_creatures:moss_fields"
            );
        }
    }
    assert!(saw_region, "weight-900 region must win some cell");
}

// ---------------------------------------------------------------------------
// Deterministic mc-spine-mods-dir-adaptation/1 report + bundle outputs
// ---------------------------------------------------------------------------

#[test]
fn mods_dir_report_is_deterministic_and_outputs_parse() {
    let scratch = Scratch::new("report");
    let mods = scratch.path("mods");
    populate_mods_dir(&mods);

    let first = adapt(&mods);
    let second = adapt(&mods);
    assert_eq!(
        first.to_json(),
        second.to_json(),
        "adaptation report must be byte-deterministic"
    );

    // The emitted JSON is a real mc-spine-mods-dir-adaptation/1 document.
    let report_json = first.to_json();
    let root = parse_json(report_json.as_bytes()).expect("report JSON parses");
    assert_eq!(json_text(&root, "schema"), "mc-spine-mods-dir-adaptation/1");
    assert_eq!(json_count(&root, "jars_found"), 5);
    assert_eq!(json_count(&root, "non_jar_entries"), 1);
    assert_eq!(json_text(&root, "verdict"), "requires_fallback");
    let totals = json_field(&root, "totals");
    assert_eq!(json_count(totals, "auto_skipped"), 1);
    assert_eq!(json_count(totals, "native_bundle"), 2);
    assert_eq!(json_count(totals, "tier2_lifted_shim"), 1);
    assert_eq!(json_count(totals, "tier3_fallback_required"), 1);
    let jars = json_field(&root, "jars").as_array().expect("jar rows");
    assert_eq!(jars.len(), 5);
    // Every row carries the per-jar schema and a tier string.
    for row in jars {
        assert_eq!(json_text(row, "schema"), "mc-spine-jar-adaptation/1");
        assert!(!json_text(row, "tier").is_empty());
    }

    // write_adaptation_outputs: two native bundles + the report file.
    let out_dir = scratch.path("adapted");
    let written = write_adaptation_outputs(&out_dir, &report_json, &first.jars)
        .expect("outputs must write");
    assert_eq!(written.len(), 3, "written: {written:?}");

    // Synthesized culinary bundle: minimal but schema-valid.
    let hearth_path = out_dir.join("hearthcraft.bundle.json");
    let hearth = DeclarativeModBundle::from_json(
        &fs::read_to_string(&hearth_path).expect("hearthcraft bundle written"),
    )
    .expect("written bundle re-parses");
    assert_eq!(hearth.mod_id, "hearthcraft");

    // Embedded dimension bundle: the verbatim document round-trips.
    let aether_path = out_dir.join("aether_demo.bundle.json");
    let aether_text = fs::read_to_string(&aether_path).expect("aether bundle written");
    let aether = DeclarativeModBundle::from_json(&aether_text).expect("aether bundle re-parses");
    assert_eq!(aether.mod_id, "aether_demo");
    assert_eq!(aether.portals.len(), 1);
    assert_eq!(aether.progression_barriers.len(), 1);
    assert_eq!(
        aether_text,
        aether.to_json(),
        "written bundle is the deterministic canonical document"
    );

    // Tier 2 / Tier 3 / auto-skipped jars emit no bundle file at all.
    for name in [
        "sodium.bundle.json",
        "guard_events.bundle.json",
        "invasive_coremod.bundle.json",
        "custom_creatures.bundle.json",
    ] {
        assert!(
            !out_dir.join(name).exists(),
            "{name} must not be written"
        );
    }

    // The report file carries exactly the deterministic document.
    let report_path = out_dir.join(ADAPTATION_REPORT_FILE);
    assert!(written.contains(&report_path));
    assert_eq!(
        fs::read_to_string(&report_path).expect("report written"),
        report_json
    );
}

// ---------------------------------------------------------------------------
// The Tier 3 coremod fails cleanly on its own — never a panic, never an Err
// ---------------------------------------------------------------------------

#[test]
fn coremod_jar_fails_cleanly_in_isolation() {
    // A jar that merely cannot adapt lands in Tier 3; it is never Err and
    // never panics — even alone in a one-jar mods dir.
    let result = auto_adapt_jar_bytes(&coremod_jar(), COREMOD_JAR, &AutoAdaptOptions::default())
        .expect("an unadaptable mod is a Tier 3 verdict, not an error");
    assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
    assert!(result.blockers.iter().all(|b| b.contains("mixin site")));
    let scan = result.mixin_scan.as_ref().expect("mixin scan ran");
    assert_eq!(scan.sites.len(), 2);
    assert!(matches!(
        scan.verdict,
        MixinLiftVerdict::RequiresFallback { .. }
    ));

    // A directory containing only the coremod still produces a coherent
    // report — the other archetypes are unaffected either way.
    let scratch = Scratch::new("coremod-only");
    let mods = scratch.path("mods");
    fs::create_dir_all(&mods).unwrap();
    fs::write(mods.join(COREMOD_JAR), coremod_jar()).unwrap();
    let report = adapt(&mods);
    assert_eq!(report.jars.len(), 1);
    assert_eq!(
        report.tier_count(AdaptationTier::Tier3FallbackRequired),
        1
    );
    assert_eq!(
        jar_named(&report, COREMOD_JAR).blockers.len(),
        2
    );
}
