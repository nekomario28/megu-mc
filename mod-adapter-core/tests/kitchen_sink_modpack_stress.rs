// SPDX-License-Identifier: LGPL-3.0-or-later

//! Kitchen-sink 35+ diverse-mod stress & integration suite.
//!
//! One synthetic modpack holding **38 valid JARs spanning every mod
//! category** — NeoForge and Fabric tech/cooking/magic/creature/worldgen/
//! QoL mods, recursive `META-INF/jarjar/` Jar-in-Jar containers, config
//! library shims, and renamed superseded performance mods — plus two
//! hostile siblings (a path-traversal probe JAR and a truncated JAR), two
//! `.disabled` entries, and two standalone `megu-mod-bundle/1` documents
//! (the native mod format, the same `*.bundle.json` the live
//! `scan_bundles_dir` picks up). Every archive is generated in-memory by
//! the stored-ZIP builder below: zero third-party files, zero copyrighted
//! content, zero network.
//!
//! What this proves, end to end:
//! 1. `auto_adapt_mods_dir` loads the whole valid pack in bounded time —
//!    every mod is discovered, classified, and adapted; all four
//!    superseded perf mods skip *before* a single `.class` byte is read
//!    (`mixin_scan` stays `None`); no mod id collides.
//! 2. The pipeline is fail-closed over the whole directory exactly as the
//!    live bridge documents (`src/live_mod_bridge.rs::scan_mods_dir`): a
//!    single traversal/corrupt sibling aborts the whole-pass `Err`, and
//!    the per-JAR recovery path then adapts every valid mod while both
//!    hostile JARs produce clean errors — never poisoning their 36
//!    siblings, never panicking.
//! 3. `UnifiedTagIndex` unifies `c:` / `forge:` / `neoforge:` tag aliases
//!    declared by *different* mods (nested Jar-in-Jar tag files folded
//!    in) into one index with zero dropped entries, while `minecraft:`
//!    and non-conventional ids stay untouched.
//! 4. `BlockCapabilityBus` carries items end-to-end across three
//!    different mods' machines — a gearforged supply chest through a
//!    conduitry conveyor buffer into forgecolossus's `3x3x3`
//!    `MultiBlockForgeRuntime` and out the output chest — with
//!    simulate-then-commit rollback on incompatible items, ticked under
//!    `ModTickRuntime`'s dirty-gate.
//! 5. A bundle-declared colossal creature runs the five-anchor
//!    `GrowthStageCurve`, resolves head/wing/tail ray hits through
//!    `MultiPartHitbox`, takes off via `MountableFlyer` once its growth
//!    stage unlocks flight, and sweeps a bounded `ElementalBreathSpec`
//!    cone.
//! 6. Config-library shims (`jupiter`/`uranus`/`architectury`/
//!    `cloth-config`) satisfy dependents' declared deps and classify
//!    hot-reloadable on the bounded subset VM; removing them produces
//!    EN/JA `DepRecoveryPlan` guidance for every dependent.
//! 7. The whole adaptation is deterministic: two runs over the same
//!    directory produce byte-identical reports.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mod_adapter_core::auto_adapter::{
    auto_adapt_jar_bytes, auto_adapt_mods_dir, extract_mod_data, AdaptationTier,
    AutoAdaptOptions,
};
use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
use mod_adapter_core::capability_bus::{
    BlockCapabilityBus, BlockCoord, CapabilityItemStack, CookingPotItemAdapter,
    FaceDirection, SimpleSlotInventory,
};
use mod_adapter_core::colossal_and_forge::{
    evaluate_breath_cone, forge_shell_3x3, step_mountable_flyer, AlloyRecipe,
    BlockProbe, BreathEntity, ElementKind, ElementalBreathSpec, ForgeCellExpect,
    GrowthStageCurve, GrowthStageSpec, MountableFlyerInput, MountableFlyerSpec,
    MountableFlyerState, MultiBlockForgeRuntime, MultiBlockForgeSpec,
    MultiPartHitbox, PartKind, PartOffsetSpec,
};
use mod_adapter_core::creature_primitives::MovementProfile;
use mod_adapter_core::culinary_adapter::{tag_contains, CulinaryRegistry};
use mod_adapter_core::dep_resolver::{
    assess_hot_reload_after_add, plan_dep_recovery, ReloadRequirement,
};
use mod_adapter_core::ecosystem_compat::{plan_dual_loader_mods_dir, ModLoaderKind};
use mod_adapter_core::loader_extensions::{
    inspect_jar_in_jar, parse_tag_file_json, TagFileEntry, TagRef,
};
use mod_adapter_core::mod_tick_runtime::ModTickRuntime;
use mod_adapter_core::unified_bundle::{DeclarativeModBundle, UnifiedModRuntime};
use mod_adapter_core::unified_tags::{
    canonical_common_tag_id, tag_id_for_member_path, TagDomain, UnifiedTagIndex,
};
use mod_adapter_core::version_compat::McVersion;

// ---------------------------------------------------------------------------
// Scratch directories (unique per test, removed on drop)
// ---------------------------------------------------------------------------

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "megu-kitchen-sink-{}-{}-{tag}",
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

/// A generated archive plus its member table — retained so later passes
/// can feed the exact same bytes through `ConventionalTagRegistry` and
/// `plan_dual_loader_mods_dir` without re-reading the ZIP.
struct ModArchive {
    file_name: &'static str,
    bytes: Vec<u8>,
    members: Vec<(String, Vec<u8>)>,
}

fn archive(file_name: &'static str, entries: &[(&str, &[u8])]) -> ModArchive {
    ModArchive {
        file_name,
        bytes: jar(entries),
        members: entries
            .iter()
            .map(|(n, b)| ((*n).to_string(), b.to_vec()))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Manifests / embedded documents
// ---------------------------------------------------------------------------

fn fabric_manifest(id: &str, version: &str, extra: &str) -> String {
    format!(r#"{{"schemaVersion":1,"id":"{id}","version":"{version}","environment":"*"{extra}}}"#)
}

fn neoforge_manifest(mod_id: &str, version: &str, extra_deps: &str) -> String {
    format!(
        r#"modLoader = "javafml"
loaderVersion = "[1,)"
license = "MIT"

[[mods]]
modId = "{mod_id}"
version = "{version}"

[[dependencies.{mod_id}]]
modId = "minecraft"
type = "required"
versionRange = "[1.21,1.22)"
side = "BOTH"

[[dependencies.{mod_id}]]
modId = "neoforge"
type = "required"
versionRange = "[21.1,)"
side = "BOTH"
{extra_deps}"#
    )
}

fn neoforge_required_dep(mod_id: &str, dep: &str, range: &str) -> String {
    format!(
        r#"
[[dependencies.{mod_id}]]
modId = "{dep}"
type = "required"
versionRange = "{range}"
side = "BOTH"
"#
    )
}

/// Real subset-verifiable `.class`: `public <internal>` with
/// `init() { return; }` — the same fixture shape
/// `dep_resolver_tests::subset_safe_mod_jar` proves hot-reloadable.
fn safe_class(internal: &str) -> Vec<u8> {
    ClassAssembler::new(internal).finish(&[MethodSpec::new("init", "()V", 1, 1, vec![0xB1])])
}

/// A structurally valid `.class` with zero verifiable-method surface —
/// used inside superseded perf JARs (which must never be scanned anyway).
fn plain_class(internal: &str) -> Vec<u8> {
    safe_class(internal)
}

/// Minimal 16x16 RGBA PNG: signature + IHDR(13) + empty IDAT + IEND —
/// enough for the bounded header inspector to read real dimensions.
fn png_16x16_rgba() -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    out.extend(13u32.to_be_bytes());
    out.extend(b"IHDR");
    out.extend(16u32.to_be_bytes());
    out.extend(16u32.to_be_bytes());
    out.extend([8u8, 6, 0, 0, 0]);
    out.extend(crc32(&[]).to_be_bytes());
    out.extend(0u32.to_be_bytes());
    out.extend(b"IDAT");
    out.extend(crc32(&[]).to_be_bytes());
    out.extend(0u32.to_be_bytes());
    out.extend(b"IEND");
    out.extend(crc32(&[]).to_be_bytes());
    out
}

fn tag(values: &[&str]) -> String {
    let inner: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
    format!(r#"{{"replace":false,"values":[{}]}}"#, inner.join(","))
}

// ---------------------------------------------------------------------------
// The 40-archive kitchen-sink fixture set
// ---------------------------------------------------------------------------

const SUPERSEDED_IDS: [&str; 4] = ["lithium", "sodium", "ferritecore", "modernfix"];
const SHIM_IDS: [&str; 4] = ["jupiter", "uranus", "architectury", "cloth_config"];
const VALID_JAR_COUNT: usize = 38;
const HOSTILE_JAR_COUNT: usize = 2;

/// Tech & Automation (6): NeoForge + Fabric, machines, crushers, smelters,
/// conduits, kinetic blocks, `c:`/`forge:`/`neoforge:` tags, and one
/// recursive Jar-in-Jar container.
fn tech_mods() -> Vec<ModArchive> {
    vec![
        archive(
            "gearforged-neoforge-3.1.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("gearforged", "3.1", "").as_bytes(),
                ),
                (
                    "com/gearforged/ForgeMod.class",
                    safe_class("com/gearforged/ForgeMod").as_slice(),
                ),
                (
                    "data/gearforged/recipes/steel_ingot.json",
                    br#"{"type":"minecraft:smelting","ingredient":{"item":"gearforged:iron_dust"},"result":{"id":"gearforged:steel_ingot","count":1},"cookingtime":200,"experience":0.7}"#,
                ),
                (
                    "data/gearforged/recipes/steel_block.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["SSS","SSS","SSS"],"key":{"S":{"item":"gearforged:steel_ingot"}},"result":{"id":"gearforged:steel_block","count":1}}"#,
                ),
                (
                    "data/gearforged/recipes/steel_dust.json",
                    br#"{"type":"gearforged:crushing","ingredients":[{"item":"gearforged:steel_ingot"}],"result":{"id":"gearforged:steel_dust","count":2}}"#,
                ),
                (
                    "data/forge/tags/items/ingots/steel.json",
                    tag(&["gearforged:steel_ingot", "gearforged:steel_block"]).as_bytes(),
                ),
                (
                    "data/gearforged/tags/blocks/machines.json",
                    tag(&["gearforged:crusher", "gearforged:smelter"]).as_bytes(),
                ),
                (
                    "assets/gearforged/blockstates/crusher.json",
                    br#"{"variants":{"":{"model":"gearforged:block/crusher"}}}"#,
                ),
                (
                    "assets/gearforged/models/block/crusher.json",
                    br#"{"parent":"minecraft:block/cube_all","textures":{"all":"gearforged:block/crusher"}}"#,
                ),
            ],
        ),
        archive(
            "kineticworks-fabric-2.4.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("kineticworks", "2.4.0", "").as_bytes(),
                ),
                (
                    "data/kineticworks/recipes/gear_press.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":[" G ","GIG"," G "],"key":{"G":{"tag":"c:ingots/brass"},"I":{"item":"minecraft:iron_ingot"}},"result":{"id":"kineticworks:gear","count":4}}"#,
                ),
                (
                    "data/kineticworks/recipes/iron_dust.json",
                    br#"{"type":"kineticworks:crushing","ingredients":[{"item":"minecraft:iron_ore"}],"result":{"id":"kineticworks:iron_dust","count":2}}"#,
                ),
                (
                    "data/c/tags/items/tools/wrench.json",
                    tag(&["kineticworks:wrench"]).as_bytes(),
                ),
                (
                    "data/kineticworks/tags/blocks/kinetic.json",
                    tag(&["kineticworks:cogwheel", "kineticworks:shaft"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "conduitry-neoforge-1.9.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("conduitry", "1.9", "").as_bytes(),
                ),
                (
                    "data/conduitry/recipes/brass_conduit.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["BBB","B B","BBB"],"key":{"B":{"tag":"c:ingots/brass"}},"result":{"id":"conduitry:item_conduit","count":8}}"#,
                ),
                (
                    "data/neoforge/tags/items/ingots/brass.json",
                    tag(&["conduitry:brass_ingot"]).as_bytes(),
                ),
                (
                    "data/conduitry/tags/blocks/conduits.json",
                    tag(&["conduitry:item_conduit", "conduitry:fluid_conduit"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "smeltmaster-fabric-0.8.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("smeltmaster", "0.8.1", "").as_bytes(),
                ),
                (
                    "data/smeltmaster/recipes/glass.json",
                    br#"{"type":"minecraft:smelting","ingredient":{"item":"minecraft:sand"},"result":{"id":"minecraft:glass","count":1},"cookingtime":160,"experience":0.1}"#,
                ),
                (
                    "data/c/tags/items/ingots/refined.json",
                    tag(&["#forge:ingots/steel", "smeltmaster:coke_dust"]).as_bytes(),
                ),
                (
                    "data/smeltmaster/tags/blocks/furnaces.json",
                    tag(&["smeltmaster:blast_smelter"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "jijforge-neoforge-4.0.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("jijforge", "4.0", "").as_bytes(),
                ),
                (
                    "data/jijforge/recipes/forge_hammer.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["II"," S"],"key":{"I":{"tag":"c:ingots/steel"},"S":{"item":"minecraft:stick"}},"result":{"id":"jijforge:forge_hammer","count":1}}"#,
                ),
                (
                    "META-INF/jarjar/metadata.json",
                    br#"{"jars":[{"identifier":{"group":"com.jij","artifact":"jijcore"},"version":{"artifactVersion":"1.0","range":"[1.0,)"},"path":"META-INF/jarjar/jijcore.jar"}]}"#,
                ),
                (
                    "META-INF/jarjar/jijcore.jar",
                    jar(&[
                        (
                            "fabric.mod.json",
                            fabric_manifest("jijcore", "1.0.0", "").as_bytes(),
                        ),
                        (
                            "META-INF/jarjar/metadata.json",
                            br#"{"jars":[{"identifier":{"group":"com.jij","artifact":"jijutil"},"version":{"artifactVersion":"2.1","range":"[2.0,)"},"path":"META-INF/jarjar/jijutil.jar"}]}"#,
                        ),
                        (
                            "META-INF/jarjar/jijutil.jar",
                            jar(&[(
                                "fabric.mod.json",
                                fabric_manifest("jijutil", "2.1.0", "").as_bytes(),
                            )])
                            .as_slice(),
                        ),
                    ])
                    .as_slice(),
                ),
            ],
        ),
        archive(
            "brassworks-neoforge-1.1.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("brassworks", "1.1", "").as_bytes(),
                ),
                (
                    "data/c/tags/items/ingots/brass.json",
                    tag(&["brassworks:brass_ingot", "brassworks:brass_sheet"]).as_bytes(),
                ),
                (
                    "data/brassworks/recipes/brass_plate.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["BB","BB"],"key":{"B":{"tag":"c:ingots/brass"}},"result":{"id":"brassworks:brass_plate","count":1}}"#,
                ),
                (
                    "data/brassworks/tags/items/plates.json",
                    tag(&["brassworks:brass_plate"]).as_bytes(),
                ),
            ],
        ),
    ]
}

/// Cooking, Farming & Food (6): cooking/cutting/fermenting recipes,
/// `*:add_item` + `*:add_table` loot modifiers, `c:crops/*` + `c:foods/*`
/// tags, and food specs.
fn cooking_mods() -> Vec<ModArchive> {
    vec![
        archive(
            "hearthdelight-neoforge-1.4.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("hearthdelight", "1.4", "").as_bytes(),
                ),
                (
                    "data/hearthdelight/recipes/pot_roast.json",
                    br#"{"type":"hearthdelight:cooking","ingredients":[{"item":"minecraft:beef"},{"item":"hearthdelight:onion"}],"result":{"id":"hearthdelight:pot_roast","count":1},"container":{"item":"minecraft:bowl"},"cookingtime":240,"experience":0.6}"#,
                ),
                (
                    "data/hearthdelight/recipes/roast_slices.json",
                    br#"{"type":"hearthdelight:cutting","ingredients":[{"item":"hearthdelight:pot_roast"}],"tool":{"item":"hearthdelight:iron_knife"},"result":[{"item":"hearthdelight:roast_slice","count":3}]}"#,
                ),
                (
                    "data/hearthdelight/recipes/kimchi.json",
                    br#"{"type":"hearthdelight:fermenting","ingredients":[{"tag":"c:crops/onion"}],"result":{"id":"hearthdelight:kimchi","count":1},"duration":9600}"#,
                ),
                (
                    "data/hearthdelight/loot_modifiers/seeds_from_grass.json",
                    br#"{"type":"hearthdelight:add_item","item":"hearthdelight:wild_seeds","chance":0.4,"conditions":[{"condition":"minecraft:loot_table_id","loot_table_id":"minecraft:blocks/short_grass"}]}"#,
                ),
                (
                    "data/hearthdelight/food/pot_roast.json",
                    br#"{"nutrition":10,"saturation":0.8,"eat_seconds":2.0}"#,
                ),
                (
                    "data/c/tags/items/crops/onion.json",
                    tag(&["hearthdelight:onion"]).as_bytes(),
                ),
                (
                    "data/c/tags/items/foods/meals.json",
                    tag(&["hearthdelight:pot_roast", "hearthdelight:kimchi"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "spiceharvest-fabric-2.0.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("spiceharvest", "2.0.0", "").as_bytes(),
                ),
                (
                    "data/spiceharvest/recipes/pickled_peppers.json",
                    br#"{"type":"spiceharvest:fermenting","ingredients":[{"tag":"c:crops/spices"}],"result":{"id":"spiceharvest:pickled_peppers","count":1},"duration":7200}"#,
                ),
                (
                    "data/spiceharvest/recipes/spice_rub.json",
                    br#"{"type":"spiceharvest:cooking","ingredients":[{"item":"minecraft:chicken"},{"tag":"c:crops/spices"}],"result":{"id":"spiceharvest:spiced_chicken","count":1},"cookingtime":200,"experience":0.4}"#,
                ),
                (
                    "data/spiceharvest/loot_modifiers/add_spice_table.json",
                    br#"{"type":"spiceharvest:add_table","table":"minecraft:chests/village/village_plains_house","entries":[{"item":"spiceharvest:pepper_seeds","weight":3}]}"#,
                ),
                (
                    "data/c/tags/items/crops/spices.json",
                    tag(&["spiceharvest:pepper", "spiceharvest:cumin"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "barrelworks-neoforge-1.2.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("barrelworks", "1.2", "").as_bytes(),
                ),
                (
                    "data/barrelworks/recipes/aged_cheese.json",
                    br#"{"type":"barrelworks:fermenting","ingredients":[{"item":"barrelworks:milk_curd"}],"result":{"id":"barrelworks:aged_cheese","count":1},"duration":12000}"#,
                ),
                (
                    "data/barrelworks/tags/blocks/barrels.json",
                    tag(&["barrelworks:oak_barrel", "barrelworks:spruce_barrel"]).as_bytes(),
                ),
                (
                    "data/c/tags/items/foods/fermented.json",
                    tag(&["barrelworks:aged_cheese"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "cropfields-fabric-3.3.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("cropfields", "3.3.0", "").as_bytes(),
                ),
                (
                    "data/c/tags/items/crops/wheat.json",
                    tag(&["cropfields:golden_wheat"]).as_bytes(),
                ),
                (
                    "data/c/tags/items/crops/corn.json",
                    tag(&["cropfields:sweetcorn"]).as_bytes(),
                ),
                (
                    "data/cropfields/loot_modifiers/corn_from_grass.json",
                    br#"{"type":"cropfields:add_item","item":"cropfields:corn_seeds","chance":0.25,"conditions":[{"condition":"minecraft:loot_table_id","loot_table_id":"minecraft:blocks/tall_grass"}]}"#,
                ),
                (
                    "data/cropfields/recipes/cornmeal.json",
                    br#"{"type":"minecraft:crafting_shapeless","ingredients":[{"tag":"c:crops/corn"}],"result":{"id":"cropfields:cornmeal","count":2}}"#,
                ),
            ],
        ),
        archive(
            "kitchenalia-neoforge-0.9.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("kitchenalia", "0.9", "").as_bytes(),
                ),
                (
                    "data/kitchenalia/recipes/diced_onion.json",
                    br#"{"type":"kitchenalia:cutting","ingredients":[{"tag":"c:crops/onion"}],"tool":{"tag":"c:tools/knives"},"result":[{"item":"kitchenalia:diced_onion","count":2}]}"#,
                ),
                (
                    "data/c/tags/items/tools/knives.json",
                    tag(&["kitchenalia:iron_knife", "kitchenalia:steel_knife"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "feastbound-fabric-1.5.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("feastbound", "1.5.0", "").as_bytes(),
                ),
                (
                    "data/feastbound/recipes/harvest_stew.json",
                    br#"{"type":"feastbound:cooking","ingredients":[{"tag":"c:crops/wheat"},{"tag":"c:foods/meals"}],"result":{"id":"feastbound:harvest_stew","count":1},"cookingtime":260,"experience":0.9}"#,
                ),
                (
                    "data/feastbound/food/harvest_stew.json",
                    br#"{"nutrition":12,"saturation":0.9,"eat_seconds":2.4}"#,
                ),
                (
                    "data/c/tags/items/foods/feasts.json",
                    tag(&["feastbound:harvest_stew"]).as_bytes(),
                ),
            ],
        ),
    ]
}

/// Magic, Spells & Rituals (5): embedded `megu-mod-bundle/1` documents
/// carrying spells, rituals, progression barriers and custom statuses,
/// plus pure-data magic mods.
fn magic_mods() -> Vec<ModArchive> {
    let manaweave_bundle = r#"{
  "schema": "megu-mod-bundle/1",
  "mod_id": "manaweave",
  "mc_version_range": "[1.21,)",
  "portals": [],
  "progression_barriers": [],
  "biome_regions": [],
  "creatures": [],
  "custom_channels": ["manaweave:sync"],
  "spells": [
    {
      "id": "manaweave:mana_bolt",
      "mana_cost": 12.5,
      "base_cooldown_ticks": 40,
      "cast": {"kind": "cast_time", "total_ticks": 20},
      "steps": [
        {"form": {"kind": "projectile", "speed": 1.5, "gravity": 0.0, "max_ticks": 60},
         "effects": [{"kind": "damage", "school": "arcane", "base_amount": 6.0}]}
      ]
    }
  ],
  "custom_statuses": [],
  "rituals": [
    {
      "id": "manaweave:mana_circle",
      "pattern": [{"dx": 0, "dy": 0, "dz": 0, "block": "manaweave:altar_core"}],
      "reagents": {"manaweave:mana_crystal": 2},
      "duration_ticks": 100,
      "mana_per_tick": 1.5
    }
  ]
}"#;
    let spellbound_bundle = r#"{
  "schema": "megu-mod-bundle/1",
  "mod_id": "spellbound",
  "mc_version_range": "[1.21,)",
  "portals": [],
  "progression_barriers": [],
  "biome_regions": [],
  "creatures": [],
  "custom_channels": [],
  "spells": [
    {
      "id": "spellbound:arc_missile",
      "mana_cost": 8.0,
      "base_cooldown_ticks": 20,
      "cast": {"kind": "instant"},
      "steps": [
        {"form": {"kind": "raycast_beam", "range": 24.0, "pierce_cap": 3},
         "effects": [{"kind": "damage", "school": "arcane", "base_amount": 4.0}]}
      ]
    }
  ],
  "custom_statuses": [
    {"id": "spellbound:mana_burn", "pulse_interval_ticks": 20, "damage_per_pulse": 1.0}
  ],
  "rituals": []
}"#;
    let ritualcraft_bundle = r#"{
  "schema": "megu-mod-bundle/1",
  "mod_id": "ritualcraft",
  "mc_version_range": "[1.21,)",
  "portals": [],
  "progression_barriers": [
    {
      "id": "ritualcraft:sun_seal",
      "dimension": "minecraft:overworld",
      "target_biomes": ["ritualcraft:sunless_wastes"],
      "required_advancements": ["ritualcraft:enter_wastes"],
      "block_break_protected": true,
      "block_place_protected": true,
      "enforce_effects": [
        {"effect": "minecraft:slowness", "duration_ticks": 80, "amplifier": 1}
      ],
      "periodic_damage": null
    }
  ],
  "biome_regions": [],
  "creatures": [],
  "custom_channels": ["ritualcraft:rites"],
  "rituals": [
    {
      "id": "ritualcraft:sun_altar",
      "pattern": [
        {"dx": 0, "dy": 0, "dz": 0, "block": "ritualcraft:sun_altar_core"},
        {"dx": 1, "dy": 0, "dz": 0, "block": "ritualcraft:gold_trim"}
      ],
      "reagents": {"minecraft:gold_ingot": 4},
      "duration_ticks": 200,
      "mana_per_tick": 2.0
    }
  ]
}"#;
    vec![
        archive(
            "manaweave-neoforge-2.2.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest(
                        "manaweave",
                        "2.2",
                        &neoforge_required_dep("manaweave", "jupiter", "[2.0,)"),
                    )
                    .as_bytes(),
                ),
                ("megu-bundle.json", manaweave_bundle.as_bytes()),
                (
                    "data/c/tags/items/gems/mana_crystal.json",
                    tag(&["manaweave:mana_crystal"]).as_bytes(),
                ),
                (
                    "data/manaweave/recipes/altar_core.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["G G"," G ","G G"],"key":{"G":{"tag":"c:gems/mana_crystal"}},"result":{"id":"manaweave:altar_core","count":1}}"#,
                ),
                (
                    "data/manaweave/advancement/first_ritual.json",
                    br#"{"criteria":{"got_crystal":{"trigger":"minecraft:inventory_changed","conditions":{"items":[{"items":"manaweave:mana_crystal"}]}}},"display":{"title":{"text":"First Ritual"}}}"#,
                ),
            ],
        ),
        archive(
            "spellbound-fabric-1.7.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("spellbound", "1.7.0", "").as_bytes(),
                ),
                ("megu-bundle.json", spellbound_bundle.as_bytes()),
                (
                    "data/spellbound/recipes/spell_book.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["PPP","PBP","PPP"],"key":{"P":{"item":"minecraft:paper"},"B":{"item":"minecraft:book"}},"result":{"id":"spellbound:spell_book","count":1}}"#,
                ),
            ],
        ),
        archive(
            "ritualcraft-neoforge-3.0.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest(
                        "ritualcraft",
                        "3.0",
                        &neoforge_required_dep("ritualcraft", "uranus", "[1.0,)"),
                    )
                    .as_bytes(),
                ),
                ("megu-bundle.json", ritualcraft_bundle.as_bytes()),
                (
                    "data/ritualcraft/tags/blocks/altar_frames.json",
                    tag(&["ritualcraft:sun_altar_core", "ritualcraft:gold_trim"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "arcanum-fabric-0.5.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("arcanum", "0.5.2", "").as_bytes(),
                ),
                (
                    "data/arcanum/recipes/enchant_dust.json",
                    br#"{"type":"minecraft:crafting_shapeless","ingredients":[{"tag":"c:gems/mana_crystal"},{"item":"minecraft:redstone"}],"result":{"id":"arcanum:enchant_dust","count":2}}"#,
                ),
                (
                    "data/c/tags/items/gems/arcane.json",
                    tag(&["arcanum:arcane_shard"]).as_bytes(),
                ),
                (
                    "data/arcanum/tags/items/enchanting.json",
                    tag(&["arcanum:enchant_dust", "arcanum:arcane_shard"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "hexcraft-neoforge-1.3.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("hexcraft", "1.3", "").as_bytes(),
                ),
                (
                    "data/hexcraft/advancement/hex_mastery.json",
                    br#"{"criteria":{"brew":{"trigger":"minecraft:brewed_potion","conditions":{"potion":"hexcraft:long_levitation"}}},"display":{"title":{"text":"Hex Mastery"}}}"#,
                ),
                (
                    "data/hexcraft/recipes/levitation_draught.json",
                    br#"{"type":"hexcraft:brewing","ingredient":{"item":"minecraft:phantom_membrane"},"base":{"item":"minecraft:awkward_potion"},"result":{"id":"hexcraft:levitation_draught","count":1}}"#,
                ),
                (
                    "data/hexcraft/tags/items/reagents.json",
                    tag(&["hexcraft:void_salt", "minecraft:phantom_membrane"]).as_bytes(),
                ),
            ],
        ),
    ]
}

/// Bosses, Colossal Creatures & Multi-Block Forges (4): a winged dragon +
/// sea serpent declared as real bundle creature specs, a multi-block
/// alloy forge's data surface, and a worldgen boss den.
fn creature_mods() -> Vec<ModArchive> {
    let dragon_bundle = r##"{
  "schema": "megu-mod-bundle/1",
  "mod_id": "dragonreign",
  "mc_version_range": "[1.21,)",
  "portals": [],
  "progression_barriers": [],
  "biome_regions": [],
  "creatures": [
    {
      "entity_type": "dragonreign:winged_dragon",
      "max_health": 400.0,
      "tame_items": ["minecraft:beef"],
      "tame_chance": 1.0,
      "breed_items": ["#c:crops/wheat"],
      "heal_items": [{"item": "dragonreign:dragon_meat", "heal": 20.0}],
      "baby_growth_ticks": 400,
      "breed_cooldown_ticks": 600,
      "supports_wander_mode": true,
      "rideable_when_tamed": true,
      "movement": {"kind": "gliding_flight", "ground_speed": 0.3, "fly_speed": 0.9, "glide_fall_scale": 0.2}
    }
  ],
  "custom_channels": ["dragonreign:roar"]
}"##;
    let serpent_bundle = r#"{
  "schema": "megu-mod-bundle/1",
  "mod_id": "serpentdeep",
  "mc_version_range": "[1.21,)",
  "portals": [],
  "progression_barriers": [],
  "biome_regions": [],
  "creatures": [
    {
      "entity_type": "serpentdeep:sea_serpent",
      "max_health": 300.0,
      "tame_items": [],
      "tame_chance": 0.0,
      "breed_items": [],
      "heal_items": [],
      "supports_wander_mode": false,
      "rideable_when_tamed": false,
      "movement": {"kind": "aquatic", "swim_speed": 1.2, "suffocates_in_air_ticks": 100}
    }
  ],
  "custom_channels": []
}"#;
    vec![
        archive(
            "dragonreign-neoforge-2.7.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest(
                        "dragonreign",
                        "2.7",
                        &format!(
                            "{}{}",
                            neoforge_required_dep("dragonreign", "architectury", "[13.0,)"),
                            neoforge_required_dep("dragonreign", "cloth_config", "[15.0,)"),
                        ),
                    )
                    .as_bytes(),
                ),
                ("megu-bundle.json", dragon_bundle.as_bytes()),
                (
                    "data/dragonreign/tags/items/dragon_scales.json",
                    tag(&["dragonreign:fire_scale", "dragonreign:frost_scale"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "serpentdeep-fabric-1.9.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("serpentdeep", "1.9.0", "").as_bytes(),
                ),
                ("megu-bundle.json", serpent_bundle.as_bytes()),
                (
                    "data/serpentdeep/tags/blocks/serpent_lairs.json",
                    tag(&["serpentdeep:tide_altar"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "forgecolossus-neoforge-4.4.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("forgecolossus", "4.4", "").as_bytes(),
                ),
                (
                    "data/forgecolossus/recipes/alloy_forge.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["FFF","F F","FFF"],"key":{"F":{"tag":"c:stones/refractory"}},"result":{"id":"forgecolossus:forge_controller","count":1}}"#,
                ),
                (
                    "data/c/tags/items/stones/refractory.json",
                    tag(&["forgecolossus:refractory_brick"]).as_bytes(),
                ),
                (
                    "data/forgecolossus/tags/blocks/forge_frame.json",
                    tag(&["forgecolossus:refractory_bricks", "forgecolossus:forge_controller"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "behemothpeaks-fabric-0.7.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("behemothpeaks", "0.7.0", "").as_bytes(),
                ),
                (
                    "data/behemothpeaks/worldgen/structure/peak_den.json",
                    br#"{"type":"minecraft:jigsaw","start_pool":"behemothpeaks:peak_den","size":3,"max_distance_from_center":80}"#,
                ),
                (
                    "data/behemothpeaks/tags/items/behemoth_drops.json",
                    tag(&["behemothpeaks:behemoth_pelt", "behemothpeaks:behemoth_horn"]).as_bytes(),
                ),
                (
                    "data/behemothpeaks/recipes/pelt_cloak.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["PPP","P P"],"key":{"P":{"item":"behemothpeaks:behemoth_pelt"}},"result":{"id":"behemothpeaks:pelt_cloak","count":1}}"#,
                ),
            ],
        ),
    ]
}

/// Worldgen, Biomes, Ores & Dimensions (5): real `dimension_type` +
/// `dimension` pairs, `c:ores/*` tags, worldgen features, NeoForge
/// `biome_modifier` documents, and decorative light blocks.
fn worldgen_mods() -> Vec<ModArchive> {
    vec![
        archive(
            "zephyria-fabric-2.1.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("zephyria", "2.1.0", "").as_bytes(),
                ),
                (
                    "data/zephyria/dimension_type/skyreach.json",
                    br#"{"min_y":0,"height":256,"has_skylight":true,"has_ceiling":false,"natural":true,"ambient_light":0.7,"effects":"minecraft:overworld","coordinate_scale":1.0}"#,
                ),
                (
                    "data/zephyria/dimension/skyreach.json",
                    br#"{"type":"zephyria:skyreach","generator":{"type":"minecraft:noise","settings":"zephyria:skyreach_noise","biome_source":{"type":"minecraft:fixed","biome":"zephyria:wind_peaks"}}}"#,
                ),
                (
                    "data/zephyria/worldgen/biome/wind_peaks.json",
                    br#"{"temperature":0.3,"downfall":0.4,"precipitation":"rain","effects":{"sky_color":7907327,"fog_color":12638463,"water_color":4159204,"water_fog_color":329011}}"#,
                ),
            ],
        ),
        archive(
            "oremancy-neoforge-1.6.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("oremancy", "1.6", "").as_bytes(),
                ),
                (
                    "data/c/tags/items/ores/mythril.json",
                    tag(&["oremancy:mythril_ore", "oremancy:deepslate_mythril_ore"]).as_bytes(),
                ),
                (
                    "data/oremancy/worldgen/configured_feature/mythril_vein.json",
                    br#"{"type":"minecraft:ore","config":{"size":9,"discard_chance_on_air_exposure":0.0,"targets":[{"target":{"predicate_type":"minecraft:tag_match","tag":"minecraft:stone_ore_replaceables"},"state":{"Name":"oremancy:mythril_ore"}}]}}"#,
                ),
                (
                    "data/oremancy/worldgen/placed_feature/mythril_vein.json",
                    br#"{"feature":"oremancy:mythril_vein","placement":[{"type":"minecraft:count","count":8},{"type":"minecraft:height_range","height":{"type":"minecraft:uniform","min_inclusive":{"above_bottom":0},"max_inclusive":{"below_top":64}}}]}"#,
                ),
            ],
        ),
        archive(
            "biomeweaver-fabric-3.5.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("biomeweaver", "3.5.0", "").as_bytes(),
                ),
                (
                    "data/biomeweaver/worldgen/biome/whisperwoods.json",
                    br#"{"temperature":0.6,"downfall":0.8,"precipitation":"rain","effects":{"sky_color":8034067,"fog_color":12638463,"water_color":4159204,"water_fog_color":329011,"foliage_color":5877296}}"#,
                ),
                (
                    "data/biomeweaver/neoforge/biome_modifier/add_spawns.json",
                    br#"{"type":"neoforge:add_spawns","biomes":"biomeweaver:whisperwoods","spawners":{"type":"minecraft:entity","value":"biomeweaver:whisper_stag","weight":12,"minCount":1,"maxCount":3}}"#,
                ),
            ],
        ),
        archive(
            "voidreach-neoforge-1.0.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("voidreach", "1.0", "").as_bytes(),
                ),
                (
                    "data/voidreach/dimension_type/the_void.json",
                    br#"{"min_y":-64,"height":384,"has_skylight":false,"has_ceiling":false,"natural":false,"ambient_light":0.0,"effects":"minecraft:the_end","coordinate_scale":4.0}"#,
                ),
                (
                    "data/voidreach/dimension/the_void.json",
                    br#"{"type":"voidreach:the_void","generator":{"type":"minecraft:noise","settings":"minecraft:end","biome_source":{"type":"minecraft:fixed","biome":"voidreach:void_wastes"}}}"#,
                ),
            ],
        ),
        archive(
            "glowhaven-fabric-2.2.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("glowhaven", "2.2.0", "").as_bytes(),
                ),
                (
                    "data/glowhaven/recipes/glow_lantern.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":[" G ","GLG"," G "],"key":{"G":{"tag":"c:stones/glowstone_bricks"},"L":{"item":"minecraft:glowstone_dust"}},"result":{"id":"glowhaven:glow_lantern","count":1}}"#,
                ),
                (
                    "data/c/tags/items/stones/glowstone_bricks.json",
                    tag(&["glowhaven:glowstone_bricks"]).as_bytes(),
                ),
                (
                    "assets/glowhaven/blockstates/glow_lantern.json",
                    br#"{"variants":{"":{"model":"glowhaven:block/glow_lantern"}}}"#,
                ),
            ],
        ),
    ]
}

/// QoL / HUD / Tooltip / Audio / Resource (4): NeoForge + Fabric QoL
/// metadata, EN+JA lang files, `sounds.json`, and a real 16x16 RGBA PNG.
fn qol_mods() -> Vec<ModArchive> {
    let png = png_16x16_rgba();
    vec![
        archive(
            "hudplus-neoforge-1.1.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("hudplus", "1.1", "").as_bytes(),
                ),
                (
                    "assets/hudplus/lang/en_us.json",
                    br#"{"hudplus.title":"HUD+","hudplus.armor.label":"Armor"}"#,
                ),
                (
                    "assets/hudplus/lang/ja_jp.json",
                    r#"{"hudplus.title":"HUD+","hudplus.armor.label":"防具"}"#.as_bytes(),
                ),
                (
                    "assets/hudplus/sounds.json",
                    br#"{"hudplus.chime":{"sounds":[{"name":"hudplus:chime"}]}}"#,
                ),
                ("assets/hudplus/textures/gui/hud.png", png.as_slice()),
                (
                    "data/hudplus/recipes/hud_lens.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":["GGG","G G","GGG"],"key":{"G":{"item":"minecraft:glass_pane"}},"result":{"id":"hudplus:hud_lens","count":1}}"#,
                ),
            ],
        ),
        archive(
            "tooltipper-fabric-0.6.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("tooltipper", "0.6.0", "").as_bytes(),
                ),
                (
                    "assets/tooltipper/lang/en_us.json",
                    br#"{"tooltipper.shift_hint":"Hold SHIFT for details"}"#,
                ),
                (
                    "data/tooltipper/tags/items/tooltips.json",
                    tag(&["tooltipper:insight_lens"]).as_bytes(),
                ),
            ],
        ),
        archive(
            "soundscapes-neoforge-2.3.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("soundscapes", "2.3", "").as_bytes(),
                ),
                (
                    "assets/soundscapes/sounds.json",
                    br#"{"soundscapes.forge_clang":{"sounds":[{"name":"soundscapes:forge_clang"}]},"soundscapes.rain_soft":{"sounds":[{"name":"soundscapes:rain_soft"}]}}"#,
                ),
                (
                    "assets/soundscapes/lang/en_us.json",
                    br#"{"soundscapes.subtitle.forge_clang":"Forge clangs"}"#,
                ),
                (
                    "data/soundscapes/recipes/echo_chime.json",
                    br#"{"type":"minecraft:crafting_shaped","pattern":[" N ","NBN"," N "],"key":{"N":{"item":"minecraft:iron_nugget"},"B":{"item":"minecraft:bell"}},"result":{"id":"soundscapes:echo_chime","count":1}}"#,
                ),
            ],
        ),
        archive(
            "resourcery-fabric-1.4.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("resourcery", "1.4.0", "").as_bytes(),
                ),
                ("assets/resourcery/textures/block/gem_block.png", png.as_slice()),
                (
                    "data/minecraft/tags/items/planks.json",
                    tag(&["resourcery:gem_planks"]).as_bytes(),
                ),
                (
                    "assets/resourcery/models/block/gem_block.json",
                    br#"{"parent":"minecraft:block/cube_all","textures":{"all":"resourcery:block/gem_block"}}"#,
                ),
                (
                    "assets/resourcery/items/gem_block.json",
                    br#"{"model":{"type":"minecraft:model","model":"resourcery:block/gem_block"}}"#,
                ),
                (
                    "assets/resourcery/lang/ja_jp.json",
                    r#"{"block.resourcery.gem_block":"宝石ブロック"}"#.as_bytes(),
                ),
            ],
        ),
    ]
}

/// Superseded performance mods (4): synthetic renamed JARs whose
/// superseded ids live in the loader manifest — the engine must detect
/// and skip them before a single `.class` byte is read.
fn perf_mods() -> Vec<ModArchive> {
    vec![
        archive(
            "fastmath-1.0.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("lithium", "0.15.0", "").as_bytes(),
                ),
                (
                    "com/fastmath/TickOpt.class",
                    plain_class("com/fastmath/TickOpt").as_slice(),
                ),
            ],
        ),
        archive(
            "quickdraw-2.0.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("sodium", "0.6.13", "").as_bytes(),
                ),
                (
                    "com/quickdraw/RenderOpt.class",
                    plain_class("com/quickdraw/RenderOpt").as_slice(),
                ),
            ],
        ),
        archive(
            "memtrim-7.1.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("ferritecore", "7.1.0", "").as_bytes(),
                ),
                (
                    "com/memtrim/StateDedup.class",
                    plain_class("com/memtrim/StateDedup").as_slice(),
                ),
            ],
        ),
        archive(
            "bootboost-5.0.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("modernfix", "5.24.0", "").as_bytes(),
                ),
                (
                    "com/bootboost/LaunchOpt.class",
                    plain_class("com/bootboost/LaunchOpt").as_slice(),
                ),
            ],
        ),
    ]
}

/// Config-library shims (4): loader manifests plus subset-verifiable
/// `.class` members only — utility libraries with zero data surface.
fn shim_mods() -> Vec<ModArchive> {
    vec![
        archive(
            "jupiter-2.0.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("jupiter", "2.0.0", "").as_bytes(),
                ),
                (
                    "com/jupiter/JupiterConfig.class",
                    safe_class("com/jupiter/JupiterConfig").as_slice(),
                ),
            ],
        ),
        archive(
            "uranus-1.4.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("uranus", "1.4.0", "").as_bytes(),
                ),
                (
                    "com/uranus/UranusOptions.class",
                    safe_class("com/uranus/UranusOptions").as_slice(),
                ),
            ],
        ),
        archive(
            "architectury-13.0.jar",
            &[
                (
                    "META-INF/neoforge.mods.toml",
                    neoforge_manifest("architectury", "13.0", "").as_bytes(),
                ),
                (
                    "com/architectury/ArchBridge.class",
                    safe_class("com/architectury/ArchBridge").as_slice(),
                ),
            ],
        ),
        archive(
            "cloth-config-15.0.jar",
            &[
                (
                    "fabric.mod.json",
                    fabric_manifest("cloth_config", "15.0.0", "").as_bytes(),
                ),
                (
                    "com/clothconfig/ClothScreen.class",
                    safe_class("com/clothconfig/ClothScreen").as_slice(),
                ),
            ],
        ),
    ]
}

/// Hostile siblings (2 — must never poison the other 38):
/// `evilprobe-6.6.jar` smuggles a `../evil.class` traversal member next to
/// perfectly valid `assets/` content, and `truncated-9.9.jar` is a
/// truncated archive with no end-of-central-directory record.
fn hostile_mods() -> Vec<ModArchive> {
    let good = archive(
        "truncated-9.9.jar",
        &[(
            "fabric.mod.json",
            fabric_manifest("truncmod", "9.9.9", "").as_bytes(),
        )],
    );
    let truncated_bytes = good.bytes[..good.bytes.len() / 2].to_vec();
    vec![
        archive(
            "evilprobe-6.6.jar",
            &[
                (
                    "../evil.class",
                    safe_class("com/evil/Evil").as_slice(),
                ),
                (
                    "fabric.mod.json",
                    fabric_manifest("evilprobe", "6.6.6", "").as_bytes(),
                ),
                (
                    "assets/evilprobe/lang/en_us.json",
                    br#"{"evilprobe.title":"Definitely Harmless"}"#,
                ),
            ],
        ),
        ModArchive {
            file_name: "truncated-9.9.jar",
            bytes: truncated_bytes,
            members: Vec::new(),
        },
    ]
}

/// Standalone native `megu-mod-bundle/1` documents — the format the live
/// `scan_bundles_dir` installs from `mods/` (the same native-mod surface
/// the brief's `.mmod` concept maps to in this tree).
fn native_bundle_docs() -> [(&'static str, &'static str); 2] {
    [
        (
            "skylark_native.bundle.json",
            r##"{"schema":"megu-mod-bundle/1","mod_id":"skylark_native","mc_version_range":"[1.21,)","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[{"entity_type":"skylark_native:skylark","max_health":12.0,"tame_items":["#c:crops/wheat"],"tame_chance":0.5,"breed_items":[],"heal_items":[],"supports_wander_mode":true,"can_shoulder_perch":true,"rideable_when_tamed":false,"movement":{"kind":"gliding_flight","ground_speed":0.2,"fly_speed":0.7,"glide_fall_scale":0.15}}],"custom_channels":["skylark_native:song"]}"##,
        ),
        (
            "ember_native.bundle.json",
            r#"{"schema":"megu-mod-bundle/1","mod_id":"ember_native","mc_version_range":"[1.21,)","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":["ember_native:embers"],"custom_statuses":[{"id":"ember_native:ember_glow","pulse_interval_ticks":40,"absorption_per_level":2.0}]}"#,
        ),
    ]
}

/// Non-`.jar` siblings that must be ignored by the JAR scan without ever
/// being read: a `.jar.disabled`, a `.mmod.disabled`, and the two native
/// bundle documents.
fn write_non_jar_siblings(dir: &Path) -> usize {
    let mut count = 0usize;
    let disabled_jar = archive(
        "abandoned-1.0.jar.disabled",
        &[(
            "fabric.mod.json",
            fabric_manifest("abandoned", "1.0.0", "").as_bytes(),
        )],
    );
    fs::write(dir.join(disabled_jar.file_name), &disabled_jar.bytes).unwrap();
    count += 1;
    fs::write(
        dir.join("legacy_native.mmod.disabled"),
        br#"{"schema":"megu-mod-bundle/1","mod_id":"legacy_native","mc_version_range":"[1.21,)"}"#,
    )
    .unwrap();
    count += 1;
    for (name, doc) in native_bundle_docs() {
        fs::write(dir.join(name), doc).unwrap();
        count += 1;
    }
    count
}

/// Every valid mod JAR: 6 tech + 6 cooking + 5 magic + 4 creature + 5
/// worldgen + 4 QoL + 4 superseded perf + 4 config-library shims = 38.
fn valid_mods() -> Vec<ModArchive> {
    let mut mods = Vec::with_capacity(VALID_JAR_COUNT);
    mods.extend(tech_mods());
    mods.extend(cooking_mods());
    mods.extend(magic_mods());
    mods.extend(creature_mods());
    mods.extend(worldgen_mods());
    mods.extend(qol_mods());
    mods.extend(perf_mods());
    mods.extend(shim_mods());
    assert_eq!(mods.len(), VALID_JAR_COUNT);
    mods
}

/// `mods/` holding every valid JAR plus the non-JAR siblings — the layout
/// the whole-pass scan adapts in one deterministic `Ok` run.
fn write_clean_dir() -> (Scratch, usize) {
    let scratch = Scratch::new("clean");
    for m in valid_mods() {
        fs::write(scratch.path(m.file_name), &m.bytes).unwrap();
    }
    let non_jar = write_non_jar_siblings(&scratch.0);
    (scratch, non_jar)
}

/// `mods/` holding the clean pack plus both hostile JAR siblings — the
/// layout `auto_adapt_mods_dir` must reject fail-closed while the live
/// per-JAR recovery path adapts every good mod anyway.
fn write_full_dir() -> (Scratch, usize) {
    let (scratch, non_jar) = write_clean_dir();
    for m in hostile_mods() {
        fs::write(scratch.path(m.file_name), &m.bytes).unwrap();
    }
    (scratch, non_jar)
}

fn sorted_jar_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.to_ascii_lowercase().ends_with(".jar").then_some(name)
        })
        .collect();
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
    names
}

fn adapt_each_jar(dir: &Path) -> Vec<(String, Result<String, String>)> {
    sorted_jar_names(dir)
        .into_iter()
        .map(|name| {
            let bytes = fs::read(dir.join(&name)).unwrap();
            let verdict = auto_adapt_jar_bytes(&bytes, &name, &AutoAdaptOptions::default())
                .map(|r| {
                    r.mod_id()
                        .map(str::to_string)
                        .unwrap_or_else(|| "<none>".to_string())
                });
            (name, verdict)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Whole-pack load: discovery, tiers, superseded skips, bounded latency
// ---------------------------------------------------------------------------

#[test]
fn full_pack_loads_bounded_with_zero_collisions() {
    let (scratch, non_jar) = write_clean_dir();
    let opts = AutoAdaptOptions::default();

    let start = Instant::now();
    let report = auto_adapt_mods_dir(&scratch.0, &opts)
        .unwrap_or_else(|e| panic!("38-JAR pack must adapt cleanly: {e}"));
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "full pack adaptation took {elapsed:?} — exceeds the 5s bound"
    );
    eprintln!("[kitchen-sink] 38-JAR adaptation: {elapsed:?}");

    assert_eq!(report.jars.len(), VALID_JAR_COUNT);
    assert_eq!(report.non_jar_entries, non_jar);

    // Tier shape: 4 superseded perf mods skipped, 5 Tier-3 (4 config
    // shims carrying bytecode and gearforged's Java entrypoint class with
    // no liftable mixin surface), every other mod promoted to a validated
    // native bundle. The recursive JiJ container is native because its
    // nested archives are extracted declaratively rather than bridged.
    assert_eq!(report.tier_count(AdaptationTier::AutoSkipped), 4);
    assert_eq!(report.tier_count(AdaptationTier::Tier2LiftedShim), 0);
    assert_eq!(report.tier_count(AdaptationTier::Tier3FallbackRequired), 5);
    assert_eq!(report.tier_count(AdaptationTier::NativeBundle), 29);

    // A Tier-3 jar is still fully discovered: gearforged's extracted data
    // surface (recipes + tags + blockstates) rides along with its blocker
    // instead of being silently dropped.
    let gearforged = report
        .jars
        .iter()
        .find(|j| j.mod_id() == Some("gearforged"))
        .expect("gearforged missing from the report");
    assert!(
        gearforged
            .blockers
            .iter()
            .any(|b| b.contains("Java bytecode")),
        "{:?}",
        gearforged.blockers
    );
    // Both ingredient/result and shaped `pattern`/`key` recipe forms
    // lift into the parsed bundle — all three gearforged recipes land.
    assert!(gearforged.data_bundle.recipes.len() >= 3);
    assert!(!gearforged.data_bundle.tags.is_empty());
    assert!(!gearforged.data_bundle.blockstates.is_empty());

    // All four superseded perf mods are caught and skipped *before*
    // bytecode work: `mixin_scan`/`culinary`/`dimensions` all stay `None`
    // and the audit trail names the superseded reason.
    for jar in &report.jars {
        if let Some(id) = jar.mod_id() {
            if SUPERSEDED_IDS.contains(&id) {
                assert_eq!(jar.tier, AdaptationTier::AutoSkipped, "{}", jar.jar);
                assert!(
                    jar.mixin_scan.is_none(),
                    "{}: superseded mod must skip before bytecode read",
                    jar.jar
                );
                assert!(
                    jar.audit_notes
                        .iter()
                        .any(|n| n.contains("BUILTIN_SUPERSEDED_PERF_MODS")),
                    "{}: missing superseded audit note",
                    jar.jar
                );
            }
        }
    }
    for id in SUPERSEDED_IDS {
        assert!(
            report.jars.iter().any(|j| j.mod_id() == Some(id)),
            "superseded mod '{id}' not classified"
        );
    }

    // Zero ID collisions: every declared mod id is unique, and the report
    // carries no duplicate-declared-id note.
    let mut ids = BTreeSet::new();
    for jar in &report.jars {
        assert!(
            !jar.audit_notes.is_empty(),
            "{}: adaptation produced no evidence trail",
            jar.jar
        );
        if let Some(id) = jar.mod_id() {
            assert!(ids.insert(id.to_string()), "duplicate mod id '{id}'");
        }
    }
    assert!(
        !report
            .audit_notes
            .iter()
            .any(|n| n.contains("also declares")),
        "unexpected duplicate mod-id note(s): {:?}",
        report.audit_notes
    );

    // Every culinary/dimension/data family actually landed somewhere:
    // aggregated culinary counters, dimension discovery, and data-bundle
    // contents are all non-trivial across the pack.
    let cooking: usize = report
        .jars
        .iter()
        .filter_map(|j| j.culinary.as_ref())
        .map(|c| c.cooking_recipes)
        .sum();
    let cutting: usize = report
        .jars
        .iter()
        .filter_map(|j| j.culinary.as_ref())
        .map(|c| c.cutting_recipes)
        .sum();
    let loot_mods: usize = report
        .jars
        .iter()
        .filter_map(|j| j.culinary.as_ref())
        .map(|c| c.loot_modifiers)
        .sum();
    let food_specs: usize = report
        .jars
        .iter()
        .filter_map(|j| j.culinary.as_ref())
        .map(|c| c.food_specs)
        .sum();
    assert!(cooking >= 3, "expected >=3 cooking recipes, got {cooking}");
    assert!(cutting >= 2, "expected >=2 cutting recipes, got {cutting}");
    assert!(loot_mods >= 3, "expected >=3 loot modifiers, got {loot_mods}");
    assert!(food_specs >= 2, "expected >=2 food specs, got {food_specs}");

    let dim_mods: Vec<&str> = report
        .jars
        .iter()
        .filter(|j| {
            j.dimensions
                .as_ref()
                .is_some_and(|d| d.is_dimension_mod())
        })
        .filter_map(|j| j.mod_id())
        .collect();
    assert!(
        dim_mods.contains(&"zephyria") && dim_mods.contains(&"voidreach"),
        "dimension mods not discovered: {dim_mods:?}"
    );

    // Real cross-mod content — no mod id was hardcoded anywhere: distinct
    // namespaces across the pack's extracted data bundles.
    let namespaces: BTreeSet<&str> = report
        .jars
        .iter()
        .flat_map(|j| j.data_bundle.namespaces.iter().map(String::as_str))
        .collect();
    assert!(
        namespaces.len() >= 25,
        "expected >=25 distinct data namespaces, got {}",
        namespaces.len()
    );
    let total_recipes: usize = report
        .jars
        .iter()
        .map(|j| j.data_bundle.recipes.len())
        .sum();
    assert!(total_recipes >= 20, "expected >=20 recipes, got {total_recipes}");
}

// ---------------------------------------------------------------------------
// 2. Hostile siblings: fail-closed dir scan + live-style per-JAR recovery
// ---------------------------------------------------------------------------

#[test]
fn hostile_siblings_fail_closed_and_never_poison_the_pack() {
    let (scratch, _non_jar) = write_full_dir();
    let opts = AutoAdaptOptions::default();

    // The whole-pass scan is documented fail-closed: ONE hostile member
    // aborts adaptation of the directory (the live bridge then retries
    // per JAR — exactly what `scan_mods_dir` does in production).
    let err = auto_adapt_mods_dir(&scratch.0, &opts)
        .expect_err("a dir containing a traversal/corrupt JAR must fail closed");
    assert!(
        err.contains("evilprobe-6.6.jar") && err.contains("unsafe ZIP member path"),
        "unexpected failure: {err}"
    );

    // Per-JAR recovery (mirrors `src/live_mod_bridge.rs::scan_mods_dir`):
    // every valid mod still adapts; only the hostile pair errors.
    let results = adapt_each_jar(&scratch.0);
    let ok: Vec<&(String, Result<String, String>)> =
        results.iter().filter(|(_, r)| r.is_ok()).collect();
    let errs: Vec<&(String, Result<String, String>)> =
        results.iter().filter(|(_, r)| r.is_err()).collect();
    assert_eq!(ok.len(), VALID_JAR_COUNT, "{results:?}");
    assert_eq!(errs.len(), HOSTILE_JAR_COUNT, "{results:?}");

    let err_text = |name: &str| {
        results
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, r)| r.clone().err())
            .unwrap_or_else(|| panic!("{name} must fail"))
    };
    assert!(
        err_text("evilprobe-6.6.jar").contains("unsafe ZIP member path"),
        "{}",
        err_text("evilprobe-6.6.jar")
    );
    assert!(
        err_text("truncated-9.9.jar").contains("not a readable archive"),
        "{}",
        err_text("truncated-9.9.jar")
    );

    // And the recovered mod ids match a clean run 1:1 — the hostile pair
    // stole nothing and confused nothing.
    let clean: BTreeSet<String> = ok
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok().cloned())
        .collect();
    let (clean_scratch, _) = write_clean_dir();
    let clean_report = auto_adapt_mods_dir(&clean_scratch.0, &opts).unwrap();
    let expected: BTreeSet<String> = clean_report
        .jars
        .iter()
        .filter_map(|j| j.mod_id().map(str::to_string))
        .collect();
    assert_eq!(clean, expected, "per-JAR recovery diverged from clean scan");
}

// ---------------------------------------------------------------------------
// 3. Unified cross-mod tag index
// ---------------------------------------------------------------------------

#[test]
fn unified_tag_index_merges_c_forge_neoforge_aliases_across_all_mods() {
    // Feed every valid JAR through one index — the same unified view a
    // live boot builds, Jar-in-Jar tag files folded in recursively.
    let mods = valid_mods();
    let mut index = UnifiedTagIndex::new();
    let mut tag_files = 0usize;
    let mut nested_jars = 0usize;
    for m in &mods {
        let stats = index
            .ingest_jar_bytes(&m.bytes, m.file_name)
            .unwrap_or_else(|e| panic!("{}: tag ingest failed: {}", m.file_name, e));
        tag_files += stats.tag_files;
        nested_jars += stats.nested_jars;
    }
    assert!(tag_files >= 15, "expected >=15 tag files, got {tag_files}");
    assert_eq!(nested_jars, 2, "jijforge's two nested JiJ levels");

    // `forge:` and `neoforge:` aliases land on `c:` — merged across mods:
    let brass = index.resolve(TagDomain::Item, "c:ingots/brass").unwrap();
    assert!(brass.contains("conduitry:brass_ingot"), "{brass:?}");
    assert!(brass.contains("brassworks:brass_ingot"), "{brass:?}");
    assert!(brass.contains("brassworks:brass_sheet"), "{brass:?}");
    assert_eq!(
        index.resolve(TagDomain::Item, "forge:ingots/brass").unwrap(),
        brass
    );
    assert_eq!(
        index
            .resolve(TagDomain::Item, "neoforge:ingots/brass")
            .unwrap(),
        brass
    );

    let steel = index.resolve(TagDomain::Item, "c:ingots/steel").unwrap();
    assert!(steel.contains("gearforged:steel_ingot"), "{steel:?}");
    assert_eq!(
        index.resolve(TagDomain::Item, "forge:ingots/steel").unwrap(),
        steel
    );

    // A nested `#forge:` ref inside another mod's `c:` tag resolves
    // through the alias layer — cross-mod, cross-namespace, zero drops.
    let refined = index.resolve(TagDomain::Item, "c:ingots/refined").unwrap();
    assert!(refined.contains("smeltmaster:coke_dust"), "{refined:?}");
    assert!(refined.contains("gearforged:steel_ingot"), "{refined:?}");

    // Point probes answer the same memberships.
    assert!(index.matches(TagDomain::Item, "c:crops/wheat", "cropfields:golden_wheat"));
    assert!(index.matches(TagDomain::Item, "c:tools/knives", "kitchenalia:steel_knife"));
    assert!(!index.matches(TagDomain::Item, "c:tools/knives", "conduitry:item_conduit"));

    // Every literal every mod declared resolves — nothing dropped in the
    // merge. `tag_id_for_member_path` classifies the same surface
    // `ingest_jar_bytes` consumed.
    for m in &mods {
        for (path, bytes) in &m.members {
            let Some((domain, tag_id)) = tag_id_for_member_path(path) else {
                continue;
            };
            let canon = canonical_common_tag_id(&tag_id);
            let resolved = index.resolve(domain, &canon).unwrap();
            let parsed = parse_tag_file_json(bytes, path)
                .unwrap_or_else(|e| panic!("tag file {path}: {}", e.reason()));
            for entry in &parsed.entries {
                if let TagFileEntry::Required(TagRef::Id(id))
                | TagFileEntry::Optional(TagRef::Id(id)) = entry
                {
                    assert!(
                        resolved.contains(id),
                        "literal '{id}' from {path} missing in '{canon}' resolution"
                    );
                }
            }
        }
    }

    // `minecraft:` ids are never rewritten, and a datapack-style
    // `data/minecraft/tags/...` document merges under its own namespace —
    // coexisting with, never colliding into, the `c:` families.
    assert_eq!(
        canonical_common_tag_id("minecraft:planks"),
        "minecraft:planks"
    );
    assert_eq!(
        canonical_common_tag_id("forge:ingots/brass"),
        "c:ingots/brass"
    );
    assert_eq!(
        canonical_common_tag_id("neoforge:ingots/brass"),
        "c:ingots/brass"
    );
    assert_eq!(
        canonical_common_tag_id("forge:plates/brass"),
        "c:plates/brass"
    );
    let planks = index.resolve(TagDomain::Item, "minecraft:planks").unwrap();
    assert!(planks.contains("resourcery:gem_planks"), "{planks:?}");
    assert!(!planks.contains("brassworks:brass_ingot"), "{planks:?}");

    // Both domains populated pack-wide.
    assert!(index.tag_ids(TagDomain::Item).len() >= 15);
    assert!(index.tag_ids(TagDomain::Block).len() >= 5);
    // Non-tag and non-item/block/fluid paths never classify.
    assert!(tag_id_for_member_path("data/c/tags/entity_types/foo.json").is_none());
    assert!(tag_id_for_member_path("assets/x/models/y.json").is_none());

    // `extract_mod_data` sees the same tag surface per JAR, and the
    // adapter's flat tag table folds back into the same index.
    let mut total_tag_members = 0usize;
    let mut adapter_merged = 0usize;
    for m in &mods {
        let bundle = extract_mod_data(&m.bytes, m.file_name).unwrap();
        total_tag_members += bundle.tags.len();
        adapter_merged += index.merge_auto_adapter_tags(&bundle.tags);
    }
    assert!(
        total_tag_members >= 15,
        "expected >=15 extracted tag documents, got {total_tag_members}"
    );
    assert!(adapter_merged >= 15);
    let brass_after = index.resolve(TagDomain::Item, "c:ingots/brass").unwrap();
    assert!(brass_after.is_superset(&brass));

    // And the culinary registry's merged index resolves cross-mod items:
    // feastbound's `#c:crops/wheat` ingredient matches cropfields' member
    // declared in a different JAR.
    let mut culinary = CulinaryRegistry::new();
    for m in cooking_mods() {
        culinary.load_from_jar_bytes(&m.bytes).unwrap();
    }
    assert!(
        tag_contains(&culinary.tags, "c:crops/wheat", "cropfields:golden_wheat"),
        "cross-mod culinary tag resolution failed"
    );
    assert!(
        tag_contains(&culinary.tags, "c:tools/knives", "kitchenalia:steel_knife"),
        "tool tag resolution failed"
    );
}

// ---------------------------------------------------------------------------
// 4. Cross-mod capability bus chain
// ---------------------------------------------------------------------------

#[test]
fn cross_mod_item_flow_machine_to_multiblock_forge_with_rollback() {
    // The chain, wired across four different mods' endpoints on ONE
    // `BlockCapabilityBus`: the gearforged supply chest feeds conduitry's
    // conveyor buffer, which feeds forgecolossus's 3x3x3
    // `MultiBlockForgeRuntime` (itself registered as the item endpoint);
    // the smelted alloy lands in an output chest — with
    // simulate-then-commit rollback on incompatible items.
    let dim = "minecraft:overworld";
    let chest_a = BlockCoord::new(dim, 0, 64, 0);
    let conveyor_b = BlockCoord::new(dim, 4, 64, 0);
    let forge_c = BlockCoord::new(dim, 8, 63, 0);
    let chest_d = BlockCoord::new(dim, 12, 64, 0);
    let pot_b = BlockCoord::new(dim, 4, 65, 0); // hearthdelight pot on the same bus

    let mut bus = BlockCapabilityBus::new();
    let mut supply = SimpleSlotInventory::new(6, 64);
    supply.set_slot(0, Some(CapabilityItemStack::new("gearforged:steel_ingot", 48)));
    supply.set_slot(1, Some(CapabilityItemStack::new("forgecolossus:flux_powder", 8)));
    supply.set_slot(2, Some(CapabilityItemStack::new("hearthdelight:onion", 6)));
    bus.register_item_handler(chest_a.clone(), Box::new(supply));
    bus.register_item_handler(conveyor_b.clone(), Box::new(SimpleSlotInventory::new(4, 64)));
    bus.register_item_handler(chest_d.clone(), Box::new(SimpleSlotInventory::new(4, 64)));
    bus.register_item_handler(pot_b.clone(), Box::new(CookingPotItemAdapter::new()));

    // The 3x3x3 forge: refractory shell, one intake-port aperture on the
    // +Z face, validated against a synthetic world.
    let spec = MultiBlockForgeSpec {
        cells: forge_shell_3x3(
            ForgeCellExpect::Tag("c:stones/refractory".to_string()),
            ForgeCellExpect::Block("forgecolossus:intake_port".to_string()),
            [0, 0, 1],
        ),
        heat_elements: vec![ElementKind::Fire],
        heat_capacity: 100.0,
        heat_per_breath_tick: 5.0,
        idle_heat_decay_per_tick: 0.0,
        recipes: vec![AlloyRecipe {
            base_ingot: "gearforged:steel_ingot".to_string(),
            catalyst_item: "forgecolossus:flux_powder".to_string(),
            element: ElementKind::Fire,
            heat_cost: 20.0,
            smelt_ticks: 10,
            output_item: "forgecolossus:embersteel_ingot".to_string(),
            output_count: 2,
        }],
    };
    let refractory = || BlockProbe {
        id: "forgecolossus:refractory_bricks".to_string(),
        tags: BTreeSet::from(["c:stones/refractory".to_string()]),
    };
    let mut world: BTreeMap<[i32; 3], BlockProbe> = BTreeMap::new();
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                if (dx, dy, dz) == (0, 0, 0) {
                    continue;
                }
                let probe = if [dx, dy, dz] == [0, 0, 1] {
                    BlockProbe {
                        id: "forgecolossus:intake_port".to_string(),
                        tags: BTreeSet::new(),
                    }
                } else {
                    refractory()
                };
                world.insert([8 + dx, 63 + dy, dz], probe);
            }
        }
    }
    // One missing brick rejects the whole structure — fail-closed.
    world.remove(&[9, 64, 0]);
    assert!(!MultiBlockForgeRuntime::new([8, 63, 0])
        .revalidate(&spec, &mut |p| world.get(&p).cloned()));
    world.insert([9, 64, 0], refractory());

    let mut forge = MultiBlockForgeRuntime::new([8, 63, 0]);
    assert!(forge.revalidate(&spec, &mut |p| world.get(&p).cloned()));
    assert!(forge.structure_valid);
    bus.register_item_handler(forge_c.clone(), Box::new(forge));

    // Mod B culinary endpoint on the same bus: onions land in the pot's
    // ingredient face through its side.
    let moved = bus.transfer_item_between(
        &chest_a,
        None,
        &pot_b,
        Some(FaceDirection::North),
        6,
        Some(&|s: &CapabilityItemStack| s.item_id == "hearthdelight:onion"),
    );
    assert_eq!(moved, 6);
    {
        let pot = bus
            .item_endpoint(&pot_b)
            .unwrap()
            .as_any()
            .downcast_ref::<CookingPotItemAdapter>()
            .unwrap();
        assert_eq!(
            pot.pot().inputs[0].as_ref().map(|(id, _)| id.as_str()),
            Some("hearthdelight:onion")
        );
    }

    // A → B → C: 24 steel ingots ride the conveyor into the forge's
    // input buffer; the flux catalyst goes straight from A.
    let moved = bus.transfer_item_between(&chest_a, None, &conveyor_b, None, 24, None);
    assert_eq!(moved, 24);
    let moved = bus.transfer_item_between(&conveyor_b, None, &forge_c, None, 24, None);
    assert_eq!(moved, 24);
    let moved = bus.transfer_item_between(
        &chest_a,
        None,
        &forge_c,
        None,
        8,
        Some(&|s: &CapabilityItemStack| s.item_id == "forgecolossus:flux_powder"),
    );
    assert_eq!(moved, 8);

    // Fire breath charges the reservoir; the smelt then runs inside the
    // registered endpoint (downcast through the bus handle).
    {
        let forge = bus
            .item_endpoint_mut(&forge_c)
            .expect("forge endpoint missing")
            .as_any_mut()
            .downcast_mut::<MultiBlockForgeRuntime>()
            .expect("endpoint is not the forge runtime");
        assert!(forge.apply_breath(&spec, ElementKind::Fire, 8));
        assert_eq!(forge.heat, 40.0);
        assert_eq!(forge.input_count("gearforged:steel_ingot"), 24);
        assert_eq!(forge.input_count("forgecolossus:flux_powder"), 8);
        let events: Vec<_> = (0..12).map(|_| forge.tick(&spec)).collect();
        assert!(
            events.iter().any(|e| e.smelt_started == Some(0)),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| e.smelt_completed == Some(0)),
            "{events:?}"
        );
        assert_eq!(forge.output_count("forgecolossus:embersteel_ingot"), 2);
    }

    // C → D: the finished alloy rides out through the same bus.
    let moved = bus.transfer_item_between(&forge_c, None, &chest_d, None, 8, None);
    assert_eq!(moved, 2);
    {
        let out = bus
            .item_endpoint(&chest_d)
            .unwrap()
            .as_any()
            .downcast_ref::<SimpleSlotInventory>()
            .unwrap();
        assert_eq!(out.total_items(), 2);
    }

    // Rollback: `minecraft:dirt` is no recipe input — the forge refuses
    // it outright and the probe never leaves the source.
    {
        let chest = bus
            .item_endpoint_mut(&chest_a)
            .unwrap()
            .as_any_mut()
            .downcast_mut::<SimpleSlotInventory>()
            .unwrap();
        chest.set_slot(3, Some(CapabilityItemStack::new("minecraft:dirt", 16)));
    }
    let moved = bus.transfer_item_between(
        &chest_a,
        None,
        &forge_c,
        None,
        64,
        Some(&|s: &CapabilityItemStack| s.item_id == "minecraft:dirt"),
    );
    assert_eq!(moved, 0, "the forge must refuse non-recipe items");
    {
        let chest = bus
            .item_endpoint(&chest_a)
            .unwrap()
            .as_any()
            .downcast_ref::<SimpleSlotInventory>()
            .unwrap();
        assert_eq!(chest.slot(3).map(|s| s.count), Some(16));
        assert_eq!(chest.total_items(), 40); // 24 steel + 16 dirt, nothing voided
    }

    // Partial rollback: a nearly-full conveyor swallows only what fits —
    // the remainder stays at the source.
    {
        let conv = bus
            .item_endpoint_mut(&conveyor_b)
            .unwrap()
            .as_any_mut()
            .downcast_mut::<SimpleSlotInventory>()
            .unwrap();
        conv.set_slot(0, Some(CapabilityItemStack::new("minecraft:dirt", 60)));
        conv.set_slot(1, Some(CapabilityItemStack::new("minecraft:dirt", 64)));
        conv.set_slot(2, Some(CapabilityItemStack::new("minecraft:dirt", 64)));
        conv.set_slot(3, Some(CapabilityItemStack::new("minecraft:dirt", 64)));
    }
    let moved = bus.transfer_item_between(
        &chest_a,
        None,
        &conveyor_b,
        None,
        64,
        Some(&|s: &CapabilityItemStack| s.item_id == "minecraft:dirt"),
    );
    assert_eq!(moved, 4, "only the free capacity moves");
    {
        let chest = bus
            .item_endpoint(&chest_a)
            .unwrap()
            .as_any()
            .downcast_ref::<SimpleSlotInventory>()
            .unwrap();
        assert_eq!(
            chest.slot(3).map(|s| s.count),
            Some(12),
            "remainder stays at the source"
        );
    }

    // The same chain under the tick runtime: an unchanged machine
    // fingerprint is skipped in O(1); a committed wake fires exactly.
    let mut runtime: ModTickRuntime<String, u64> = ModTickRuntime::new();
    assert!(runtime.is_dirty(&"conduitry:port".to_string(), 7));
    runtime.commit("conduitry:port".to_string(), 7, Some(120));
    assert!(
        !runtime.is_dirty(&"conduitry:port".to_string(), 7),
        "unchanged fingerprint must skip re-evaluation"
    );
    runtime.commit("forgecolossus:hatch".to_string(), 9, Some(120));
    let mut out = Vec::new();
    let report = runtime.tick(120, 8, 1000, |_| 1, &mut out);
    assert_eq!(report.items, 2, "both wakes must be due at tick 120");
    assert_eq!(out.len(), 2);
}

// ---------------------------------------------------------------------------
// 5. Colossal creature: growth stages, multi-part hitboxes, breath, flight
// ---------------------------------------------------------------------------

#[test]
fn colossal_creature_growth_hitbox_breath_and_mounted_flight() {
    // The five-anchor growth curve a colossal boss mod declares:
    // hatchling (0.35x) through ancient (2.2x), flight unlocked at stage
    // 3 — exactly the contract dragonreign's creature docs encode.
    let curve = GrowthStageCurve::new(
        vec![
            GrowthStageSpec {
                min_age_ticks: 0,
                hitbox_scale: 0.35,
                max_health_multiplier: 0.4,
                melee_damage_multiplier: 0.4,
                breath_range_blocks: 0.0,
            },
            GrowthStageSpec {
                min_age_ticks: 2_000,
                hitbox_scale: 0.6,
                max_health_multiplier: 0.6,
                melee_damage_multiplier: 0.6,
                breath_range_blocks: 6.0,
            },
            GrowthStageSpec {
                min_age_ticks: 6_000,
                hitbox_scale: 1.0,
                max_health_multiplier: 1.0,
                melee_damage_multiplier: 1.0,
                breath_range_blocks: 10.0,
            },
            GrowthStageSpec {
                min_age_ticks: 12_000,
                hitbox_scale: 1.6,
                max_health_multiplier: 1.5,
                melee_damage_multiplier: 1.4,
                breath_range_blocks: 14.0,
            },
            GrowthStageSpec {
                min_age_ticks: 20_000,
                hitbox_scale: 2.2,
                max_health_multiplier: 2.0,
                melee_damage_multiplier: 1.8,
                breath_range_blocks: 16.0,
            },
        ],
        3,
    );
    assert_eq!(curve.stages.len(), 5);
    // Growth stages tick 1 → 5 across the declared age anchors.
    assert_eq!(curve.stage_for_age_ticks(0), 1);
    assert_eq!(curve.stage_for_age_ticks(1_999), 1);
    assert_eq!(curve.stage_for_age_ticks(2_000), 2);
    assert_eq!(curve.stage_for_age_ticks(5_999), 2);
    assert_eq!(curve.stage_for_age_ticks(6_000), 3);
    assert_eq!(curve.stage_for_age_ticks(11_999), 3);
    assert_eq!(curve.stage_for_age_ticks(12_000), 4);
    assert_eq!(curve.stage_for_age_ticks(19_999), 4);
    assert_eq!(curve.stage_for_age_ticks(20_000), 5);
    assert_eq!(curve.stage_for_age_ticks(u64::MAX), 5);
    assert!(!curve.flight_unlocked_at(1_999));
    assert!(curve.flight_unlocked_at(6_000));
    // Scalars interpolate smoothly between anchors and stay bounded.
    assert!((0..=20_000)
        .step_by(400)
        .all(|age| (0.35..=2.2).contains(&curve.hitbox_scale_at(age))));
    assert_eq!(curve.breath_range_at(0), 0.0);
    assert!(curve.breath_range_at(20_000) <= 16.0);

    // The colossal hitbox layout — head weak point, armored torso, left
    // wing, tail — evaluated at the stage-4 scale.
    let hitbox = MultiPartHitbox {
        parts: vec![
            PartOffsetSpec {
                kind: PartKind::Head,
                local_offset: [0.0, 2.0, 4.0],
                half_extents: [1.0, 1.0, 1.0],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::Torso,
                local_offset: [0.0, 1.0, 0.0],
                half_extents: [1.5, 1.0, 1.5],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::LeftWing,
                local_offset: [3.0, 1.5, 0.0],
                half_extents: [1.5, 0.5, 2.0],
                damage_multiplier: None,
            },
            PartOffsetSpec {
                kind: PartKind::Tail(1),
                local_offset: [0.0, 0.5, -4.0],
                half_extents: [0.75, 0.75, 1.5],
                damage_multiplier: Some(0.5),
            },
        ],
    };
    let scale = curve.hitbox_scale_at(12_000); // stage 4
    let aabbs = hitbox.evaluate([0.0, 0.0, 0.0], 0.0, 0.0, scale);
    assert_eq!(aabbs.len(), 4);
    // Distinct ray resolutions: head vs wing vs tail each return their
    // own part and damage multiplier; a clean miss and a zero direction
    // fail closed with `None`.
    let head = hitbox
        .hit_test(
            &aabbs,
            [0.0, f64::from(2.0 * scale), 20.0],
            [0.0, 0.0, -1.0],
            64.0,
        )
        .expect("head ray must land");
    assert_eq!(head.kind, PartKind::Head);
    assert_eq!(head.damage_multiplier, 1.25, "head is the default weak point");
    let wing = hitbox
        .hit_test(
            &aabbs,
            [f64::from(3.0 * scale), f64::from(1.5 * scale), 10.0],
            [0.0, 0.0, -1.0],
            64.0,
        )
        .expect("wing ray must land");
    assert_eq!(wing.kind, PartKind::LeftWing);
    assert_eq!(wing.damage_multiplier, 1.0);
    let tail = hitbox
        .hit_test(
            &aabbs,
            [0.0, f64::from(0.5 * scale), -20.0],
            [0.0, 0.0, 1.0],
            64.0,
        )
        .expect("tail ray must land");
    assert_eq!(tail.kind, PartKind::Tail(1));
    assert_eq!(
        tail.damage_multiplier, 0.5,
        "the spec override beats the kind default"
    );
    assert!(hitbox
        .hit_test(&aabbs, [0.0, 500.0, 0.0], [0.0, 0.0, -1.0], 64.0)
        .is_none());
    assert!(hitbox
        .hit_test(&aabbs, [0.0, 0.0, 0.0], [0.0, 0.0, 0.0], 64.0)
        .is_none());

    // Mounted flight: takeoff is refused before the growth curve's
    // unlock stage and engages the instant the mount is old enough.
    let flyer = MountableFlyerSpec {
        flight_unlock_stage: 3,
        ..MountableFlyerSpec::default()
    };
    let mut mount = MountableFlyerState::new_grounded([0.0, 64.0, 0.0], 0.0, flyer.max_stamina);
    let ev = step_mountable_flyer(
        &mut mount,
        &flyer,
        &MountableFlyerInput {
            growth_stage: 1,
            ascend: true,
            on_ground: true,
            ..MountableFlyerInput::default()
        },
    );
    assert!(ev.takeoff_refused_low_stage);
    assert!(!mount.airborne);
    let ev = step_mountable_flyer(
        &mut mount,
        &flyer,
        &MountableFlyerInput {
            growth_stage: 3,
            ascend: true,
            on_ground: true,
            ..MountableFlyerInput::default()
        },
    );
    assert!(ev.took_off, "{ev:?}");
    assert!(mount.airborne);
    // Forward flight plus the rider's breath command.
    step_mountable_flyer(
        &mut mount,
        &flyer,
        &MountableFlyerInput {
            growth_stage: 3,
            forward: 1.0,
            breath_pressed: true,
            on_ground: false,
            ..MountableFlyerInput::default()
        },
    );
    assert!(mount.breath_active);
    assert!(
        mount.vel.iter().any(|v| v.abs() > 1e-9),
        "forward intent must produce motion: {:?}",
        mount.vel
    );

    // The elemental breath: a bounded cone sweep that chars terrain per
    // the declared transform table and tags in-cone entities — never
    // sweeping outside the safety budgets.
    let breath = ElementalBreathSpec::fire();
    let mut world: BTreeMap<[i32; 3], BlockProbe> = BTreeMap::new();
    for z in 4..10 {
        world.insert(
            [0, 0, z],
            BlockProbe {
                id: "minecraft:grass_block".to_string(),
                tags: BTreeSet::new(),
            },
        );
    }
    world.insert(
        [30, 0, 0],
        BlockProbe {
            id: "minecraft:stone".to_string(),
            tags: BTreeSet::new(),
        },
    );
    let outcome = evaluate_breath_cone(
        [0.0, 0.0, 0.0],
        0.0,
        0.0,
        &breath,
        &[
            BreathEntity {
                entity_id: 7,
                center: [0.5, 0.5, 6.0],
                half_extents: [0.4, 0.4, 0.4],
                conductive: false,
            },
            BreathEntity {
                entity_id: 8,
                center: [30.0, 0.0, 0.0],
                half_extents: [0.5, 0.5, 0.5],
                conductive: false,
            },
        ],
        &mut |p| world.get(&p).cloned(),
    );
    assert!(!outcome.truncated);
    assert!(
        outcome
            .block_transforms
            .iter()
            .any(|t| t.pos == [0, 0, 4] && t.to == "minecraft:dirt"),
        "{:?}",
        outcome.block_transforms
    );
    assert!(
        !outcome.block_transforms.iter().any(|t| t.pos == [30, 0, 0]),
        "out-of-cone block transformed"
    );
    assert!(outcome
        .entity_hits
        .iter()
        .any(|h| h.entity_id == 7 && !h.chained && h.damage == 2.0));
    assert!(!outcome.entity_hits.iter().any(|h| h.entity_id == 8));

    // The same spec over the engine's bounds clamps, never explodes: an
    // absurd reach/angle request collapses onto the hard caps and the
    // sweep still completes bounded work.
    let mut over_cap = breath.clone();
    over_cap.range_blocks = 5000.0;
    over_cap.cone_half_angle_rad = 99.0;
    let clamped = evaluate_breath_cone(
        [0.0, 0.0, 0.0],
        0.0,
        0.0,
        &over_cap,
        &[BreathEntity {
            entity_id: 9,
            center: [0.0, 0.0, 30.0],
            half_extents: [0.5, 0.5, 0.5],
            conductive: false,
        }],
        &mut |_| None,
    );
    assert!(
        clamped.entity_hits.iter().any(|h| h.entity_id == 9),
        "clamped 48-block reach must still tag the in-cone entity"
    );

    // Lightning's chain arc: the out-of-cone conductive target is reached
    // by hopping off the direct hit inside the cone — bounded by
    // `chain_radius`/`chain_max_targets`/`chain_damage_falloff`.
    let lightning = ElementalBreathSpec::lightning();
    let outcome = evaluate_breath_cone(
        [0.0, 0.0, 0.0],
        0.0,
        0.0,
        &lightning,
        &[
            BreathEntity {
                entity_id: 10,
                center: [0.5, 0.5, 6.0],
                half_extents: [0.4, 0.4, 0.4],
                conductive: true,
            },
            BreathEntity {
                entity_id: 11,
                center: [4.0, 0.5, 6.0],
                half_extents: [0.4, 0.4, 0.4],
                conductive: true,
            },
        ],
        &mut |_| None,
    );
    assert!(outcome
        .entity_hits
        .iter()
        .any(|h| h.entity_id == 10 && !h.chained && h.damage == 3.0));
    assert!(outcome
        .entity_hits
        .iter()
        .any(|h| h.entity_id == 11 && h.chained && h.damage == 2.4));
}

// ---------------------------------------------------------------------------
// 6. Config-library shims + EN/JA dependency recovery guidance
// ---------------------------------------------------------------------------

#[test]
fn library_shims_satisfy_dependents_and_recover_en_ja_guidance() {
    let mods = valid_mods();
    let shim_names: BTreeSet<&str> = [
        "jupiter-2.0.jar",
        "uranus-1.4.jar",
        "architectury-13.0.jar",
        "cloth-config-15.0.jar",
    ]
    .into_iter()
    .collect();

    // Whole-pack plan: every shim is classified and satisfies its
    // dependents — zero missing dependencies, zero duplicate ids.
    let full_plan = plan_dual_loader_mods_dir(
        mods.iter()
            .map(|m| (m.file_name, m.bytes.as_slice())),
    );
    assert!(
        full_plan.rejected_jars.is_empty(),
        "{:?}",
        full_plan.rejected_jars
    );
    assert!(
        full_plan.missing_dependencies.is_empty(),
        "shims must satisfy declared deps: {:?}",
        full_plan.missing_dependencies
    );
    for shim in SHIM_IDS {
        assert!(
            full_plan.unified_mods.iter().any(|m| m.mod_id == shim),
            "shim '{shim}' not in the unified table"
        );
    }
    // Both loader families coexist in one table — the pack is genuinely
    // mixed, not an echo chamber.
    for loader in [ModLoaderKind::Fabric, ModLoaderKind::NeoForge] {
        assert!(
            full_plan.unified_mods.iter().any(|m| m.loader == loader),
            "no {loader:?} mods in the unified table"
        );
    }
    assert_eq!(full_plan.skipped_superseded.len(), 4);

    // Each shim's `.class` surface verifies under the bounded subset VM —
    // the hot-reload classifier recognizes them as safe config/utility
    // libraries, exactly what their dependents assume.
    for m in &mods {
        if !shim_names.contains(m.file_name) {
            continue;
        }
        let verdict = assess_hot_reload_after_add(&m.bytes);
        assert!(
            matches!(verdict, ReloadRequirement::HotReloadable { .. }),
            "{} must be hot-reloadable, got {verdict:?}",
            m.file_name
        );
        assert!(
            verdict
                .reasons()
                .iter()
                .any(|r| r.contains("class member(s) verified")),
            "{}: missing subset-VM evidence: {:?}",
            m.file_name,
            verdict.reasons()
        );
    }

    // Drop the shims: every dependent's declared requirement surfaces in
    // the plan, and the dep resolver emits EN + JA recovery guidance.
    let without_shims: Vec<&ModArchive> = mods
        .iter()
        .filter(|m| !shim_names.contains(m.file_name))
        .collect();
    let sparse_plan = plan_dual_loader_mods_dir(
        without_shims
            .iter()
            .map(|m| (m.file_name, m.bytes.as_slice())),
    );
    let missing_ids: BTreeSet<&str> = sparse_plan
        .missing_dependencies
        .iter()
        .map(|d| d.required_mod_id.as_str())
        .collect();
    for shim in SHIM_IDS {
        assert!(
            missing_ids.contains(shim),
            "shim '{shim}' absence not detected: {:?}",
            sparse_plan.missing_dependencies
        );
    }
    let (scratch, _) = write_clean_dir();
    let recovery = plan_dep_recovery(&scratch.0, &sparse_plan, &[]);
    assert_eq!(recovery.entries.len(), 4);
    for entry in &recovery.entries {
        assert!(
            !entry.instruction_en.is_empty() && !entry.instruction_ja.is_empty(),
            "{entry:?} lacks EN/JA guidance"
        );
        assert!(
            entry
                .query_url
                .as_deref()
                .is_some_and(|u| u.starts_with("https://api.modrinth.com/")),
            "{entry:?} lacks a Modrinth recovery query"
        );
    }
    assert!(recovery.summary_en.contains("jupiter") || recovery.summary_en.contains("Missing"));
    assert!(!recovery.summary_ja.is_empty());
}

// ---------------------------------------------------------------------------
// 7. Determinism: identical reports across repeated scans
// ---------------------------------------------------------------------------

#[test]
fn adaptation_is_deterministic_across_repeated_scans() {
    let (scratch, _) = write_clean_dir();
    let opts = AutoAdaptOptions::default();
    let first = auto_adapt_mods_dir(&scratch.0, &opts).unwrap();
    let second = auto_adapt_mods_dir(&scratch.0, &opts).unwrap();

    assert_eq!(first.to_json(), second.to_json(), "report JSON drifted");

    let fingerprint = |r: &mod_adapter_core::auto_adapter::ModsDirAdaptationReport| {
        r.jars
            .iter()
            .map(|j| {
                (
                    j.jar.clone(),
                    j.mod_id().map(str::to_string),
                    j.tier.as_str(),
                    j.data_bundle.recipes.len(),
                    j.data_bundle.tags.len(),
                    j.bundle_json(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(fingerprint(&first), fingerprint(&second));
}

// ---------------------------------------------------------------------------
// 8. Recursive Jar-in-Jar discovery, bounded and classified
// ---------------------------------------------------------------------------

#[test]
fn jar_in_jar_nested_archives_verified_bounded() {
    let jij = tech_mods()
        .into_iter()
        .find(|m| m.file_name == "jijforge-neoforge-4.0.jar")
        .unwrap();

    // The bounded recursive scan opens both declared levels: the depth-1
    // `jijcore` container and its own depth-2 `jijutil` declaration.
    let scan = inspect_jar_in_jar(&jij.bytes, jij.file_name).unwrap();
    assert_eq!(scan.declared.len(), 2, "{scan:?}");
    assert_eq!(scan.declared[0].identifier, "com.jij:jijcore");
    assert_eq!(scan.declared[1].identifier, "com.jij:jijutil");
    assert_eq!(scan.extracted_members, 2);
    assert_eq!(scan.max_depth_seen, 1);
    assert!(!scan.is_empty());

    // The adapter classifies it as native: recursive JiJ extraction folds
    // the nested archives declaratively, so no JVM-bridge blocker is
    // recorded and the nested content is not silently skipped.
    let result = auto_adapt_jar_bytes(&jij.bytes, jij.file_name, &AutoAdaptOptions::default())
        .unwrap();
    assert_eq!(result.tier, AdaptationTier::NativeBundle);
    assert!(
        !result
            .blockers
            .iter()
            .any(|b| b.contains("JVM bridge")),
        "{:?}",
        result.blockers
    );
    assert_eq!(result.data_bundle.embedded_jars.len(), 2);
    let ext = &result.descriptor.as_ref().unwrap().extensions;
    assert_eq!(ext.jar_in_jar.declared.len(), 2);
    assert!(!ext.requires_jvm_bridge());
}

// ---------------------------------------------------------------------------
// 9. Unified runtime: adapted + standalone native bundles co-install
// ---------------------------------------------------------------------------

#[test]
fn all_emitted_bundles_install_into_one_unified_runtime() {
    let (scratch, _) = write_clean_dir();
    let report = auto_adapt_mods_dir(&scratch.0, &AutoAdaptOptions::default()).unwrap();

    let mut runtime = UnifiedModRuntime::new(4);
    let mut installed = 0usize;
    for jar in &report.jars {
        if let Some(bundle) = &jar.bundle {
            let summary = runtime
                .register_bundle(bundle, McVersion::CURRENT)
                .unwrap_or_else(|e| panic!("{}: bundle install: {e}", jar.jar));
            assert_eq!(summary.mod_id, bundle.mod_id);
            installed += 1;
        }
    }
    // Standalone `*.bundle.json` documents — the live `scan_bundles_dir`
    // path — install into the same registry next to adapted bundles.
    for (name, doc) in native_bundle_docs() {
        let bundle = DeclarativeModBundle::from_json(doc)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        bundle.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        runtime
            .register_bundle(&bundle, McVersion::CURRENT)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        installed += 1;
    }
    assert!(installed >= 30, "expected >=30 installed bundles");

    let ids: Vec<&str> = runtime.installed_mod_ids().iter().map(String::as_str).collect();
    for expected in [
        "manaweave",
        "spellbound",
        "ritualcraft",
        "dragonreign",
        "serpentdeep",
        "skylark_native",
        "ember_native",
    ] {
        assert!(ids.contains(&expected), "{expected} not installed: {ids:?}");
    }
    assert!(runtime.is_channel_registered("manaweave:sync"));
    assert!(runtime.is_channel_registered("dragonreign:roar"));
    assert!(runtime.is_channel_registered("skylark_native:song"));
    assert!(runtime.ritual("manaweave:mana_circle").is_some());
    assert!(runtime.ritual("ritualcraft:sun_altar").is_some());
    assert!(runtime.spell("manaweave:mana_bolt").is_some());
    assert!(runtime.spell("spellbound:arc_missile").is_some());
    assert!(runtime.custom_status("spellbound:mana_burn").is_some());
    assert!(runtime.custom_status("ember_native:ember_glow").is_some());

    // Both colossal creatures registered through the runtime — the same
    // specs the lifecycle/hitbox test drives.
    let dragon = runtime
        .creature_spec("dragonreign:winged_dragon")
        .expect("dragon spec not installed");
    assert!(dragon.rideable_when_tamed);
    let serpent = runtime
        .creature_spec("serpentdeep:sea_serpent")
        .expect("serpent spec not installed");
    assert!(matches!(
        serpent.movement,
        MovementProfile::Aquatic { .. }
    ));
    let skylark = runtime
        .creature_spec("skylark_native:skylark")
        .expect("skylark spec not installed");
    assert!(skylark.can_shoulder_perch);
}
