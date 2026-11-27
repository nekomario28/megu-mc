// SPDX-License-Identifier: LGPL-3.0-or-later

//! End-to-end tests for structural capability inspection and the
//! `--structural` CLI mode. Fixtures are synthesized byte-by-byte in process:
//! real stored ZIP archives and real JVMS class files, no external mod jars.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_core::hash::crc32_ieee as crc32;
use mod_adapter_core::structural_compat::{
    CapabilityRequirement, CompatBlockerKind, CompatLoadVerdict, InspectableModUnit,
    RequirementSource, WorldProfile,
};

// ---------------------------------------------------------------------------
// Minimal JVM class builder (enough for real annotation bytecode)
// ---------------------------------------------------------------------------

enum CpItem {
    Utf8(Vec<u8>),
    Class(u16),
    Int(i32),
}

struct Cp {
    items: Vec<CpItem>,
    utf8: HashMap<Vec<u8>, u16>,
    class: HashMap<Vec<u8>, u16>,
    ints: HashMap<i32, u16>,
}

impl Cp {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            utf8: HashMap::new(),
            class: HashMap::new(),
            ints: HashMap::new(),
        }
    }
    fn push(&mut self, item: CpItem) -> u16 {
        let index = (self.items.len() + 1) as u16;
        self.items.push(item);
        index
    }
    fn utf8(&mut self, s: &str) -> u16 {
        if let Some(&i) = self.utf8.get(s.as_bytes()) {
            return i;
        }
        let i = self.push(CpItem::Utf8(s.as_bytes().to_vec()));
        self.utf8.insert(s.as_bytes().to_vec(), i);
        i
    }
    fn utf8_idx(&self, s: &str) -> u16 {
        self.utf8[s.as_bytes()]
    }
    fn class(&mut self, s: &str) -> u16 {
        if let Some(&i) = self.class.get(s.as_bytes()) {
            return i;
        }
        let name = self.utf8(s);
        let i = self.push(CpItem::Class(name));
        self.class.insert(s.as_bytes().to_vec(), i);
        i
    }
    fn class_idx(&self, s: &str) -> u16 {
        self.class[s.as_bytes()]
    }
    fn int(&mut self, v: i32) -> u16 {
        if let Some(&i) = self.ints.get(&v) {
            return i;
        }
        let i = self.push(CpItem::Int(v));
        self.ints.insert(v, i);
        i
    }
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.items {
            match item {
                CpItem::Utf8(bytes) => {
                    out.push(1);
                    out.extend((bytes.len() as u16).to_be_bytes());
                    out.extend(bytes);
                }
                CpItem::Class(name) => {
                    out.push(7);
                    out.extend(name.to_be_bytes());
                }
                CpItem::Int(v) => {
                    out.push(3);
                    out.extend(v.to_be_bytes());
                }
            }
        }
        out
    }
    fn count(&self) -> u16 {
        (self.items.len() + 1) as u16
    }
}

enum EV {
    Cls(String),
    Arr(Vec<EV>),
}

struct Ann {
    desc: String,
    elems: Vec<(String, EV)>,
}

fn ann(desc: &str, elems: Vec<(&str, EV)>) -> Ann {
    Ann {
        desc: desc.to_string(),
        elems: elems.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}
fn cls(v: &str) -> EV {
    EV::Cls(v.to_string())
}

fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &EV) {
    match ev {
        EV::Cls(v) => {
            out.push(b'c');
            out.extend(cp.class(v).to_be_bytes());
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

fn anns_payload(cp: &mut Cp, anns: &[&Ann]) -> Vec<u8> {
    let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
    for a in anns {
        write_ann(&mut payload, cp, a);
    }
    payload
}

struct Meth {
    name: String,
    anns: Vec<Ann>,
}

fn meth(name: &str, anns: Vec<Ann>) -> Meth {
    Meth {
        name: name.to_string(),
        anns,
    }
}

fn build_class(internal: &str, class_anns: Vec<Ann>, methods: Vec<Meth>) -> Vec<u8> {
    let mut cp = Cp::new();
    cp.class(internal);
    cp.class("java/lang/Object");
    for s in ["Code", "RuntimeVisibleAnnotations", "()V"] {
        cp.utf8(s);
    }
    for m in &methods {
        cp.utf8(&m.name);
    }
    cp.int(0);

    let mut method_attrs = Vec::new();
    for m in &methods {
        let mut attrs = Vec::new();
        let code = {
            let mut p = Vec::new();
            p.extend(1u16.to_be_bytes());
            p.extend(1u16.to_be_bytes());
            p.extend(1u32.to_be_bytes());
            p.push(0xB1);
            p.extend(0u16.to_be_bytes());
            p.extend(0u16.to_be_bytes());
            p
        };
        attrs.push((cp.utf8("Code"), code));
        if !m.anns.is_empty() {
            let refs: Vec<&Ann> = m.anns.iter().collect();
            attrs.push((
                cp.utf8("RuntimeVisibleAnnotations"),
                anns_payload(&mut cp, &refs),
            ));
        }
        method_attrs.push(attrs);
    }
    let mut class_attrs = Vec::new();
    if !class_anns.is_empty() {
        let refs: Vec<&Ann> = class_anns.iter().collect();
        class_attrs.push((
            cp.utf8("RuntimeVisibleAnnotations"),
            anns_payload(&mut cp, &refs),
        ));
    }

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(cp.class_idx(internal).to_be_bytes());
    out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces
    out.extend(0u16.to_be_bytes()); // fields
    out.extend((methods.len() as u16).to_be_bytes());
    for (m, attrs) in methods.iter().zip(&method_attrs) {
        out.extend(0x0001u16.to_be_bytes());
        out.extend(cp.utf8_idx(&m.name).to_be_bytes());
        out.extend(cp.utf8_idx("()V").to_be_bytes());
        out.extend((attrs.len() as u16).to_be_bytes());
        for (name, payload) in attrs {
            out.extend(name.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
    }
    out.extend((class_attrs.len() as u16).to_be_bytes());
    for (name, payload) in &class_attrs {
        out.extend(name.to_be_bytes());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    out
}

// ---------------------------------------------------------------------------
// ZIP and CLI helpers
// ---------------------------------------------------------------------------

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

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
fn fixture_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "megu-structural-{}-{}-{tag}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn run_cli(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_megu-mod-check"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn plain_probe_class() -> Vec<u8> {
    build_class(
        "com/example/Probe",
        vec![],
        vec![
            meth("fluidCapabilityProbe", vec![]),
            meth("recipeProbe", vec![]),
        ],
    )
}

fn full_shape(verdict: CompatLoadVerdict) -> (Vec<String>, usize, usize) {
    match verdict {
        CompatLoadVerdict::Full {
            capabilities_used,
            inferred_count,
            declared_count,
        } => (
            capabilities_used.into_iter().map(|c| c.name).collect(),
            inferred_count,
            declared_count,
        ),
        other => panic!("expected full verdict, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Contract and profile tests
// ---------------------------------------------------------------------------

#[test]
fn clean_names_contract_rev_shim_and_too_new_rejection() {
    let mut profile = WorldProfile::vanilla_1_21_1();
    profile.capabilities.insert("recipe".to_string(), 2);

    let mut unit = InspectableModUnit::empty("demo");
    unit.declared_requirements
        .push(CapabilityRequirement::declared("recipe", Some(1), false));
    let verdict = unit.evaluate(&profile);
    let CompatLoadVerdict::Full {
        capabilities_used, ..
    } = verdict
    else {
        panic!("older contract must load through shim")
    };
    assert_eq!(capabilities_used.len(), 1);
    assert_eq!(capabilities_used[0].name, "recipe");
    assert_eq!(capabilities_used[0].min_rev, Some(1));
    assert_eq!(capabilities_used[0].supported_rev, 2);
    assert!(capabilities_used[0].shimmed);
    assert!(!capabilities_used[0].name.ends_with("_v1"));

    let mut too_new = InspectableModUnit::empty("demo");
    too_new
        .declared_requirements
        .push(CapabilityRequirement::declared("recipe", Some(3), false));
    let CompatLoadVerdict::Blocked { blockers } = too_new.evaluate(&profile) else {
        panic!("too-new contract must fail closed")
    };
    assert!(blockers
        .iter()
        .any(|b| b.kind == CompatBlockerKind::UnsupportedContractRev
            && b.required_rev == Some(3)
            && b.supported_rev == Some(2)));

    let mut versioned = InspectableModUnit::empty("demo");
    versioned
        .declared_requirements
        .push(CapabilityRequirement::declared("recipe_v1", None, false));
    let CompatLoadVerdict::Blocked { blockers } = versioned.evaluate(&profile) else {
        panic!("version-suffixed capability must fail closed")
    };
    assert!(blockers
        .iter()
        .any(|b| b.kind == CompatBlockerKind::InvalidCapabilityName));
}

#[test]
fn missing_required_core_capability_blocks() {
    let mut profile = WorldProfile::vanilla_1_21_1();
    profile.capabilities.remove("block_registry");
    let mut unit = InspectableModUnit::empty("demo");
    unit.declared_requirements
        .push(CapabilityRequirement::declared(
            "block_registry",
            None,
            false,
        ));
    let CompatLoadVerdict::Blocked { blockers } = unit.evaluate(&profile) else {
        panic!("missing core capability must block")
    };
    assert_eq!(blockers[0].kind, CompatBlockerKind::MissingCapability);
    assert_eq!(blockers[0].capability.as_deref(), Some("block_registry"));
}

// ---------------------------------------------------------------------------
// Raw archive inference and feature-scoped partial loading
// ---------------------------------------------------------------------------

#[test]
fn jar_infers_bytecode_and_json_capabilities_without_manifest_requires() {
    let recipe = br#"{
      "type":"minecraft:crafting_shapeless",
      "ingredients":[{"item":"minecraft:stone"}],
      "result":{"id":"minecraft:stick","count":1}
    }"#;
    let probe = plain_probe_class();
    let archive = jar(&[
        ("com/example/Probe.class", probe.as_slice()),
        ("data/demo/recipes/stick.json", recipe.as_slice()),
    ]);
    let unit = InspectableModUnit::from_jar_bytes(&archive, "probe.jar").unwrap();

    assert!(unit
        .inferred_requirements
        .iter()
        .any(|req| req.name == "fluid_bus"
            && matches!(req.source, RequirementSource::InferredBytecode { .. })));
    assert!(unit.features.iter().any(|feature| {
        feature.id == "recipe:demo:stick"
            && feature.required_capabilities.iter().any(|req| {
                req.name == "recipe"
                    && matches!(req.source, RequirementSource::InferredDatapack { .. })
            })
    }));

    let (used, inferred, declared) = full_shape(unit.evaluate(&WorldProfile::vanilla_1_21_1()));
    assert!(used.contains(&"fluid_bus".to_string()));
    assert!(used.contains(&"recipe".to_string()));
    assert!(inferred >= 2);
    assert_eq!(declared, 0);
}

fn partial_recipe_archive() -> Vec<u8> {
    let core = br#"{
      "type":"minecraft:crafting_shapeless",
      "ingredients":[{"item":"minecraft:stone"}],
      "result":{"id":"minecraft:stick","count":1}
    }"#;
    let future = br#"{
      "type":"minecraft:crafting_shapeless",
      "neoforge:conditions":[{"type":"neoforge:mod_loaded","modid":"futuremod"}],
      "ingredients":[{"item":"minecraft:dried_ghast"}],
      "result":{"id":"minecraft:stick","count":1}
    }"#;
    jar(&[
        ("data/demo/recipes/core_stick.json", core.as_slice()),
        (
            "data/demo/recipes/compat/future_ghast.json",
            future.as_slice(),
        ),
    ])
}

#[test]
fn optional_newer_recipe_is_only_skipped_feature_in_partial() {
    let archive = partial_recipe_archive();
    let unit = InspectableModUnit::from_jar_bytes(&archive, "recipes.jar").unwrap();
    let verdict = unit.evaluate(&WorldProfile::vanilla_1_21_1());
    let CompatLoadVerdict::Partial {
        skipped_features,
        active_features,
        reason_summary,
    } = verdict
    else {
        panic!("26.3-only optional recipe must produce partial on 1.21.1")
    };
    assert_eq!(skipped_features.len(), 1);
    assert_eq!(
        skipped_features[0].feature_id,
        "recipe:demo:compat/future_ghast"
    );
    assert_eq!(
        skipped_features[0].unsupported_references,
        vec!["minecraft:dried_ghast"]
    );
    assert!(active_features.contains(&"recipe:demo:core_stick".to_string()));
    assert!(reason_summary.contains("minecraft:dried_ghast"));

    let future_verdict = unit.evaluate(&WorldProfile::vanilla_26_3());
    assert!(matches!(future_verdict, CompatLoadVerdict::Full { .. }));
}

// ---------------------------------------------------------------------------
// Fail-closed worldgen and core Mixin blockers
// ---------------------------------------------------------------------------

#[test]
fn worldgen_epoch_mismatch_and_missing_epoch_fail_closed() {
    let biome = br#"{"effects":{"sky_color":0}}"#;
    let manifest = b"worldgen_epoch = \"mc-26.3-v1\"\n";
    let mismatched = jar(&[
        ("megu-mod.toml", manifest.as_slice()),
        ("data/demo/worldgen/biome/azure.json", biome.as_slice()),
    ]);
    let unit = InspectableModUnit::from_jar_bytes(&mismatched, "worldgen.jar").unwrap();
    let CompatLoadVerdict::Blocked { blockers } = unit.evaluate(&WorldProfile::vanilla_1_21_1())
    else {
        panic!("worldgen epoch mismatch must block")
    };
    assert!(blockers
        .iter()
        .any(|b| b.kind == CompatBlockerKind::WorldgenEpochMismatch));

    let missing = jar(&[("data/demo/worldgen/biome/azure.json", biome.as_slice())]);
    let unit = InspectableModUnit::from_jar_bytes(&missing, "worldgen-noepoch.jar").unwrap();
    let CompatLoadVerdict::Blocked { blockers } = unit.evaluate(&WorldProfile::vanilla_1_21_1())
    else {
        panic!("missing worldgen epoch must block")
    };
    assert!(blockers
        .iter()
        .any(|b| b.kind == CompatBlockerKind::MissingWorldgenEpoch));
}

#[test]
fn overwrite_core_mixin_in_jar_fails_closed() {
    let config = br#"{
      "required":true,
      "package":"com.badmod.mixin",
      "mixins":["LivingEntityMixin"]
    }"#;
    let mixin_ann = ann(
        "Lorg/spongepowered/asm/mixin/Mixin;",
        vec![(
            "value",
            EV::Arr(vec![cls("net/minecraft/world/entity/LivingEntity")]),
        )],
    );
    let overwrite = ann("Lorg/spongepowered/asm/mixin/Overwrite;", vec![]);
    let class = build_class(
        "com/badmod/mixin/LivingEntityMixin",
        vec![mixin_ann],
        vec![meth("hurt", vec![overwrite])],
    );
    let archive = jar(&[
        ("bad.mixins.json", config.as_slice()),
        ("com/badmod/mixin/LivingEntityMixin.class", class.as_slice()),
    ]);
    let unit = InspectableModUnit::from_jar_bytes(&archive, "overwrite.jar").unwrap();
    assert!(unit
        .core_mixin_issues
        .iter()
        .any(|issue| issue.annotation == "@Overwrite"));
    let CompatLoadVerdict::Blocked { blockers } = unit.evaluate(&WorldProfile::vanilla_1_21_1())
    else {
        panic!("core overwrite mixin must block")
    };
    assert!(blockers
        .iter()
        .any(|b| b.kind == CompatBlockerKind::UnliftableCoreMixin
            && b.message.contains("@Overwrite")));
}

// ---------------------------------------------------------------------------
// Deterministic JSON and CLI integration
// ---------------------------------------------------------------------------

#[test]
fn verdict_json_is_deterministic_and_schema_versioned() {
    let archive = partial_recipe_archive();
    let unit = InspectableModUnit::from_jar_bytes(&archive, "recipes.jar").unwrap();
    let verdict = unit.evaluate(&WorldProfile::vanilla_1_21_1());
    let once = verdict.to_json();
    let twice = unit.evaluate(&WorldProfile::vanilla_1_21_1()).to_json();
    assert_eq!(once, twice);
    let parsed = megu_core::json::parse_json(&once).unwrap();
    assert_eq!(
        parsed.get("schema_version").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        parsed.get("verdict").and_then(|v| v.as_str()),
        Some("partial")
    );
    let reparsed = megu_core::json::parse_json(&parsed.to_json_string()).unwrap();
    assert_eq!(parsed, reparsed);
}

#[test]
fn structural_cli_reads_bundles_and_preserves_exit_contract() {
    let dir = fixture_dir("cli");
    let bundle = br#"{
      "schema":"megu-mod-bundle/1",
      "mod_id":"demo",
      "mc_version_range":"[1.21,)",
      "portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[],
      "requires":[{"name":"recipe","min_rev":1}]
    }"#;
    let bundle_path = write_fixture(&dir, "demo.mmod", bundle);
    let (code, stdout, stderr) =
        run_cli(&["--structural", bundle_path.to_str().unwrap(), "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let parsed = megu_core::json::parse_json(&stdout).unwrap();
    assert_eq!(parsed.get("verdict").and_then(|v| v.as_str()), Some("full"));
    assert_eq!(
        parsed.get("schema_version").and_then(|v| v.as_u64()),
        Some(1)
    );

    let bad_biome = br#"{"effects":{"sky_color":0}}"#;
    let archive = jar(&[("data/demo/worldgen/biome/azure.json", bad_biome.as_slice())]);
    let jar_path = write_fixture(&dir, "worldgen.jar", &archive);
    let (code, stdout, _) = run_cli(&["--structural", jar_path.to_str().unwrap(), "--json"]);
    assert_eq!(code, 2);
    assert!(
        stdout.contains("missing_worldgen_epoch"),
        "stdout: {stdout}"
    );

    let (code, _, stderr) = run_cli(&[
        "--scan-jar",
        jar_path.to_str().unwrap(),
        "--target-version",
        "26.3",
    ]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--target-version only applies to --structural"));
}

#[test]
fn bundle_structural_metadata_roundtrips_and_drives_feature_scoped_partial() {
    let document = br#"{
      "schema":"megu-mod-bundle/1",
      "mod_id":"demo",
      "mc_version_range":"[1.21,)",
      "requires":["recipe",{"name":"fluid_bus","min_rev":1,"optional":false}],
      "worldgen_epoch":"mc-1.21.1-v1",
      "optional_features":[{
        "id":"recipe:demo:future_ghast",
        "kind":"recipe",
        "references":["minecraft:dried_ghast"],
        "isolated":true,
        "affects_worldgen":false,
        "affects_core_state":false
      }],
      "portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]
    }"#;
    let bundle = mod_adapter_core::unified_bundle::DeclarativeModBundle::from_json(
        std::str::from_utf8(document).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle.requires.len(), 2);
    assert_eq!(bundle.optional_features.len(), 1);
    let once = bundle.to_json();
    let reparsed =
        mod_adapter_core::unified_bundle::DeclarativeModBundle::from_json(&once).unwrap();
    assert_eq!(reparsed, bundle);
    assert_eq!(reparsed.to_json(), once);

    let unit = InspectableModUnit::from_bundle(&bundle);
    let CompatLoadVerdict::Partial {
        skipped_features, ..
    } = unit.evaluate(&WorldProfile::vanilla_1_21_1())
    else {
        panic!("declared isolated newer-ID feature must be skipped")
    };
    assert_eq!(skipped_features[0].feature_id, "recipe:demo:future_ghast");
    assert_eq!(
        skipped_features[0].unsupported_references,
        vec!["minecraft:dried_ghast"]
    );
}

#[test]
fn versioned_bundle_capability_and_bare_optional_gap_fail_closed() {
    let bad = br#"{
      "schema":"megu-mod-bundle/1",
      "mod_id":"demo",
      "mc_version_range":"[1.21,)",
      "requires":["recipe_v1"],
      "portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]
    }"#;
    assert!(
        mod_adapter_core::unified_bundle::DeclarativeModBundle::from_json(
            std::str::from_utf8(bad).unwrap()
        )
        .is_err()
    );

    let mut profile = WorldProfile::vanilla_1_21_1();
    profile.capabilities.remove("client_hud");
    let mut unit = InspectableModUnit::empty("demo");
    unit.declared_requirements
        .push(CapabilityRequirement::declared("client_hud", None, true));
    let CompatLoadVerdict::Blocked { blockers } = unit.evaluate(&profile) else {
        panic!("bare optional capability is not an isolated feature boundary")
    };
    assert!(blockers
        .iter()
        .any(|b| b.kind == CompatBlockerKind::MissingCapability));
}

fn wasm_with_imports(imports: &[(&str, &str)]) -> Vec<u8> {
    fn leb(mut value: usize, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }
    let mut payload = Vec::new();
    leb(imports.len(), &mut payload);
    for (module, name) in imports {
        leb(module.len(), &mut payload);
        payload.extend(module.as_bytes());
        leb(name.len(), &mut payload);
        payload.extend(name.as_bytes());
        payload.push(0); // function import
        leb(0, &mut payload);
    }
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    wasm.push(2);
    leb(payload.len(), &mut wasm);
    wasm.extend(payload);
    wasm
}

#[test]
fn wasm_host_imports_infer_exact_capability_evidence() {
    let wasm = wasm_with_imports(&[("megu", "host_get_block"), ("megu", "host_emit_chat")]);
    let archive = jar(&[("native.wasm", wasm.as_slice())]);
    let unit = InspectableModUnit::from_jar_bytes(&archive, "wasm.jar").unwrap();
    assert!(unit
        .inferred_requirements
        .iter()
        .any(|req| req.name == "block_registry"
            && matches!(&req.source, RequirementSource::InferredWasmImport { module, name }
            if module == "megu" && name == "host_get_block")));
    assert!(unit
        .inferred_requirements
        .iter()
        .any(|req| req.name == "chat"
            && matches!(&req.source, RequirementSource::InferredWasmImport { module, name }
            if module == "megu" && name == "host_emit_chat")));
    let (used, _, _) = full_shape(unit.evaluate(&WorldProfile::vanilla_1_21_1()));
    assert!(used.contains(&"block_registry".to_string()));
    assert!(used.contains(&"chat".to_string()));
}

#[test]
fn evaluated_resource_conditions_drive_recipe_optionality() {
    let guaranteed = br#"{
      "type":"minecraft:crafting_shapeless",
      "neoforge:conditions":[{"type":"neoforge:true"}],
      "ingredients":[{"item":"minecraft:stone"}],
      "result":{"id":"minecraft:stick","count":1}
    }"#;
    let unavailable = br#"{
      "type":"minecraft:crafting_shapeless",
      "neoforge:conditions":[{"type":"neoforge:mod_loaded","modid":"absent_dependency"}],
      "ingredients":[{"item":"minecraft:stone"}],
      "result":{"id":"minecraft:stick","count":1}
    }"#;
    let fabric_unavailable = br#"{
      "type":"minecraft:crafting_shapeless",
      "fabric:load_conditions":[{"condition":"fabric:all_mods_loaded","values":["absent_dependency"]}],
      "ingredients":[{"item":"minecraft:stone"}],
      "result":{"id":"minecraft:stick","count":1}
    }"#;
    let archive = jar(&[
        ("data/demo/recipes/guaranteed.json", guaranteed.as_slice()),
        ("data/demo/recipes/unavailable.json", unavailable.as_slice()),
        ("data/demo/recipes/fabric_unavailable.json", fabric_unavailable.as_slice()),
    ]);
    let unit = InspectableModUnit::from_jar_bytes(&archive, "conditions.jar").unwrap();
    let optional = |id: &str| {
        unit.features
            .iter()
            .find(|feature| feature.id == id)
            .unwrap_or_else(|| panic!("missing feature {id}"))
            .optional
    };
    assert!(!optional("recipe:demo:guaranteed"));
    assert!(optional("recipe:demo:unavailable"));
    assert!(optional("recipe:demo:fabric_unavailable"));
}
