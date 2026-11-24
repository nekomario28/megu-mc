// SPDX-License-Identifier: LGPL-3.0-or-later

use mod_adapter_core::auto_adapter::{auto_adapt_jar_bytes, AdaptationTier, AutoAdaptOptions};
use mod_adapter_core::hard_mod_primitives::EngineCompatDisposition;
use mod_adapter_core::mod_router::{
    inspect_jar_security_and_capabilities, route_mod_jar, ModRouteDecision, NativeSubsystem,
    RouteMatchKind, SecurityThreatKind,
};

use megu_core::hash::crc32_ieee as crc32;

fn jar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for (name, payload) in members {
        let offset = out.len() as u32;
        let crc = crc32(payload);
        let size = payload.len() as u32;
        let name_len = name.len() as u16;
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes());
        out.extend([0u8; 8]);
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(name_len.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(*payload);

        directory.extend(0x0201_4b50u32.to_le_bytes());
        directory.extend(20u16.to_le_bytes());
        directory.extend(10u16.to_le_bytes());
        directory.extend([0u8; 8]);
        directory.extend(crc.to_le_bytes());
        directory.extend(size.to_le_bytes());
        directory.extend(size.to_le_bytes());
        directory.extend(name_len.to_le_bytes());
        directory.extend([0u8; 12]);
        directory.extend(offset.to_le_bytes());
        directory.extend(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend(&directory);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((members.len() as u16).to_le_bytes());
    out.extend((members.len() as u16).to_le_bytes());
    out.extend((directory.len() as u32).to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn manifest(id: &str) -> Vec<u8> {
    format!(r#"{{"schemaVersion":1,"id":"{id}","version":"1.0","environment":"*"}}"#).into_bytes()
}

fn class_with_utf8(strings: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(0xcafe_babeu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(((strings.len() + 5) as u16).to_be_bytes());
    for (index, string) in ["sample/Mod", "java/lang/Object"].into_iter().enumerate() {
        out.push(1);
        out.extend((string.len() as u16).to_be_bytes());
        out.extend(string.as_bytes());
        out.push(7);
        out.extend(((index * 2 + 1) as u16).to_be_bytes());
    }
    for string in strings {
        out.push(1);
        out.extend((string.len() as u16).to_be_bytes());
        out.extend(string.as_bytes());
    }
    out.extend(0x0021u16.to_be_bytes()); // public, super
    out.extend(2u16.to_be_bytes()); // this_class
    out.extend(4u16.to_be_bytes()); // super_class
    out.extend([0u8; 8]); // interfaces, fields, methods, class attributes
    out
}

// ---------------------------------------------------------------------------
// Structured constant-pool builder: real CONSTANT_Class /
// CONSTANT_NameAndType / CONSTANT_Methodref entries — security fixtures
// must carry the same resolved references a javac-compiled class
// carries, not bare UTF-8 strings.
// ---------------------------------------------------------------------------

enum CpItem {
    Utf8(String),
    Class(u16),
    NameAndType(u16, u16),
    MethodRef(u16, u16),
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
    fn method_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let class_index = self.class(owner);
        let name_index = self.utf8(name);
        let desc_index = self.utf8(desc);
        let nat_index = self.push(CpItem::NameAndType(name_index, desc_index));
        self.push(CpItem::MethodRef(class_index, nat_index))
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
                CpItem::NameAndType(name_index, desc_index) => {
                    out.push(12);
                    out.extend(name_index.to_be_bytes());
                    out.extend(desc_index.to_be_bytes());
                }
                CpItem::MethodRef(class_index, nat_index) => {
                    out.push(10);
                    out.extend(class_index.to_be_bytes());
                    out.extend(nat_index.to_be_bytes());
                }
            }
        }
        out
    }
}

/// A minimal structurally-valid `.class` whose constant pool carries
/// exactly the entries `fill` adds (on top of the `sample/Mod` /
/// `java/lang/Object` class constants every class needs).
fn class_with_pool(fill: impl FnOnce(&mut Cp)) -> Vec<u8> {
    let mut cp = Cp::default();
    let this_idx = cp.class("sample/Mod");
    let super_idx = cp.class("java/lang/Object");
    fill(&mut cp);
    let mut out = Vec::new();
    out.extend(0xcafe_babeu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(((cp.0.len() + 1) as u16).to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes()); // public, super
    out.extend(this_idx.to_be_bytes()); // this_class
    out.extend(super_idx.to_be_bytes()); // super_class
    out.extend([0u8; 8]); // interfaces, fields, methods, class attributes
    out
}

fn adapt(bytes: &[u8], name: &str) -> mod_adapter_core::auto_adapter::JarAdaptationResult {
    auto_adapt_jar_bytes(bytes, name, &AutoAdaptOptions::default()).unwrap()
}

fn expect_native_route(
    route: &ModRouteDecision,
    expected_subsystem: NativeSubsystem,
    expected_match: RouteMatchKind,
) {
    assert!(
        matches!(
            route,
            ModRouteDecision::RouteToNativeSubsystem { subsystem, matched_by, .. }
                if *subsystem == expected_subsystem && *matched_by == expected_match
        ),
        "{route:?}"
    );
}

#[test]
fn known_render_mods_are_scanned_and_superseded() {
    for id in [
        "acedium",
        "accedium",
        "vulkanmod",
        "optifine",
        "optifabric",
        "canvas",
    ] {
        let manifest = manifest(id);
        let bytes = jar(&[("fabric.mod.json", &manifest)]);
        let result = adapt(&bytes, &format!("{id}-1.21.1.jar"));
        assert_eq!(result.tier, AdaptationTier::AutoSkipped, "{id}");
        assert!(result.mixin_scan.is_none());
        expect_native_route(
            &result.route,
            NativeSubsystem::WgpuRenderBackend,
            RouteMatchKind::KnownCatalogId,
        );
        assert!(result
            .audit_notes
            .iter()
            .any(|n| n.contains("BUILTIN_SUPERSEDED_PERF_MODS")));
        assert!(result
            .to_json()
            .contains("\"matched_by\": \"known_catalog_id\""));
    }
}

#[test]
fn known_superseded_mod_cannot_force_gameplay_execution() {
    let manifest = manifest("sodium");
    let class = class_with_utf8(&["com/mojang/blaze3d/systems/RenderSystem"]);
    let bytes = jar(&[
        ("fabric.mod.json", &manifest),
        ("sodium/Render.class", &class),
        ("data/sodium/recipes/item.json", br#"{}"#),
    ]);
    let result = adapt(&bytes, "sodium-0.6.13.jar");
    assert_eq!(result.tier, AdaptationTier::AutoSkipped);
    expect_native_route(
        &result.route,
        NativeSubsystem::WgpuRenderBackend,
        RouteMatchKind::KnownCatalogId,
    );
}

#[test]
fn unknown_render_id_is_routed_by_bytecode_signature() {
    let manifest = manifest("future_vk_accel_2028");
    let class = class_with_utf8(&[
        "org/lwjgl/opengl/GL11",
        "com/mojang/blaze3d/systems/RenderSystem",
    ]);
    let bytes = jar(&[
        ("fabric.mod.json", &manifest),
        ("future/Render.class", &class),
    ]);
    let result = adapt(&bytes, "totally-unknown-renderer-99.jar");
    assert_eq!(result.tier, AdaptationTier::AutoSkipped);
    expect_native_route(
        &result.route,
        NativeSubsystem::WgpuRenderBackend,
        RouteMatchKind::BehavioralCapability,
    );
    assert_eq!(
        result.engine_compat,
        EngineCompatDisposition::NativeAccelerated {
            native_subsystem: "wgpu_render_backend",
        }
    );
    assert!(result.mixin_scan.is_none());
    assert!(result
        .audit_notes
        .iter()
        .any(|n| n.contains("unknown engine-replacement")));
    assert!(result.format_human().contains("behavioral_capability"));
}

#[test]
fn unknown_light_engine_is_native_accelerated() {
    let class = class_with_utf8(&["net/minecraft/world/level/lighting/LevelLightEngine"]);
    let bytes = jar(&[("future/Light.class", &class)]);
    let result = adapt(&bytes, "future-illumination-2028.jar");
    assert_eq!(result.tier, AdaptationTier::AutoSkipped);
    expect_native_route(
        &result.route,
        NativeSubsystem::LightEngine,
        RouteMatchKind::BehavioralCapability,
    );
}

#[test]
fn unknown_server_chunk_and_network_mod_is_native_accelerated() {
    let class = class_with_utf8(&["net/minecraft/server/level/ChunkMap", "CompressionDecoder"]);
    let bytes = jar(&[("future/Chunk.class", &class)]);
    let result = adapt(&bytes, "totally-unknown-engine.jar");
    assert_eq!(result.tier, AdaptationTier::AutoSkipped);
    expect_native_route(
        &result.route,
        NativeSubsystem::ServerChunkAndNetEngine,
        RouteMatchKind::BehavioralCapability,
    );
}

#[test]
fn gameplay_content_with_render_hooks_stays_in_standard_pipeline() {
    let manifest = manifest("coolmod");
    let class = class_with_utf8(&["com/mojang/blaze3d/systems/RenderSystem"]);
    let recipe = br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:stick"],"result":{"id":"minecraft:diamond_sword","count":1}}"#;
    let bytes = jar(&[
        ("fabric.mod.json", &manifest),
        ("cool/Render.class", &class),
        ("data/coolmod/recipes/sword.json", recipe),
    ]);
    let result = adapt(&bytes, "coolmod-1.0.jar");
    assert!(matches!(
        result.route,
        ModRouteDecision::HybridSplit {
            client_render_fallback: NativeSubsystem::WgpuRenderBackend,
            ..
        }
    ));
    assert_eq!(
        result.tier,
        AdaptationTier::NativeBundle,
        "{:?}",
        result.blockers
    );
    assert!(result.bundle.is_some());
    assert!(result
        .audit_notes
        .iter()
        .any(|n| n.contains("gameplay continues")));
    assert!(result.to_json().contains("\"decision\": \"hybrid_split\""));
}

#[test]
fn spoofed_known_render_mod_with_embedded_native_is_quarantined() {
    for member in ["lib/native.dll", "lib/native.so"] {
        let manifest = manifest("sodium");
        let bytes = jar(&[("fabric.mod.json", &manifest), (member, b"PE\0\0")]);
        let result = adapt(&bytes, "sodium-0.6.13.jar");
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(matches!(
            result.route,
            ModRouteDecision::SecurityQuarantine { .. }
        ));
        assert!(result.bundle.is_none() && result.mixin_scan.is_none());
        assert!(result
            .blockers
            .iter()
            .all(|b| b.contains("SECURITY_QUARANTINE")));
        assert_eq!(
            result.security_threats[0].kind,
            SecurityThreatKind::EmbeddedNativeOrExecutable
        );
        assert_eq!(result.security_threats[0].member, member);
        assert!(result.to_json().contains("\"security_threats\""));
        assert!(result.format_human().contains("must never execute"));
    }
}

#[test]
fn malicious_class_constants_are_quarantined_with_exact_member_and_kind() {
    for (reference, indicator, expected) in [
        (
            "java/lang/ProcessBuilder",
            "java/lang/ProcessBuilder",
            SecurityThreatKind::ProcessExecution,
        ),
        (
            "javax/naming/InitialContext",
            "javax/naming/InitialContext",
            SecurityThreatKind::JndiLookup,
        ),
        (
            "sun/misc/Unsafe",
            "sun/misc/Unsafe",
            SecurityThreatKind::UnsafeMemoryAccess,
        ),
        (
            "java/net/URLClassLoader",
            "java/net/URLClassLoader",
            SecurityThreatKind::DynamicCodeLoading,
        ),
        (
            "com/sun/jna/Native",
            "com/sun/jna/",
            SecurityThreatKind::NativeLibraryLoad,
        ),
    ] {
        let class = class_with_pool(|cp| {
            cp.class(reference);
        });
        let bytes = jar(&[("hidden/Evil.class", &class)]);
        // Unknown display name: superseded ids resolve to a native
        // subsystem before the *bytecode* quarantine verdict (their
        // constant pools never execute), so a detection pin must use an
        // id the catalog does not know.
        let result = adapt(&bytes, "totally-unknown-mod.jar");
        assert!(matches!(
            result.route,
            ModRouteDecision::SecurityQuarantine { .. }
        ));
        assert!(
            result.security_threats.iter().any(|t| t.kind == expected
                && t.member == "hidden/Evil.class"
                && t.indicator == indicator),
            "{reference}: {:?}",
            result.security_threats
        );
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
    }
}

#[test]
fn direct_network_and_heap_memory_segment_do_not_falsely_quarantine_mods() {
    let lucene_vec = class_with_pool(|cp| {
        cp.class("java/lang/foreign/MemorySegment");
    });
    let lucene_posix = class_with_pool(|cp| {
        cp.class("java/lang/foreign/Linker");
    });
    let net_class = class_with_pool(|cp| {
        cp.class("java/net/URLConnection");
    });
    let bytes = jar(&[
        ("org/apache/lucene/VectorScorer.class", &lucene_vec),
        ("org/apache/lucene/store/PosixNativeAccess.class", &lucene_posix),
        ("example/Credits.class", &net_class),
    ]);
    let result = adapt(&bytes, "benign-top-mod.jar");
    assert!(
        !matches!(result.route, ModRouteDecision::SecurityQuarantine { .. }),
        "MemorySegment, JDK-restricted Linker, and URLConnection must not hard-quarantine a mod: {:?}",
        result.route
    );
    assert!(result
        .security_threats
        .iter()
        .any(|t| t.kind == SecurityThreatKind::DirectNetworkAccess));
    assert!(result
        .security_threats
        .iter()
        .any(|t| t.kind == SecurityThreatKind::ForeignLinkerAccess));
    assert!(result
        .audit_notes
        .iter()
        .any(|n| n.contains("SANDBOX_NETWORK_BLOCKED")));
}

#[test]
fn combined_security_indicators_require_both_owner_and_method() {
    for (owner, method, kind) in [
        (
            "java/lang/Runtime",
            "exec",
            SecurityThreatKind::ProcessExecution,
        ),
        (
            "java/lang/System",
            "loadLibrary",
            SecurityThreatKind::NativeLibraryLoad,
        ),
        (
            "java/lang/ClassLoader",
            "defineClass",
            SecurityThreatKind::DynamicCodeLoading,
        ),
    ] {
        let malicious = class_with_pool(|cp| {
            cp.method_ref(owner, method, "()V");
        });
        let result = adapt(&jar(&[("test/E.class", &malicious)]), "unknown.jar");
        assert!(result.security_threats.iter().any(|t| t.kind == kind));
        let owner_only = class_with_pool(|cp| {
            cp.class(owner);
        });
        let report =
            route_mod_jar(&jar(&[("test/E.class", &owner_only)]), "unknown.jar", None).unwrap();
        assert!(!matches!(
            report.decision,
            ModRouteDecision::SecurityQuarantine { .. }
        ));
    }
}

#[test]
fn corrupt_classes_and_archives_fail_closed_without_panicking() {
    for bytes in [
        jar(&[("bad/Truncated.class", &b"\xca\xfe\xba\xbe"[..])]),
        jar(&[("bad/WrongMagic.class", &b"not a class"[..])]),
        jar(&[("../escape.class", &class_with_utf8(&[]))]),
    ] {
        assert!(
            auto_adapt_jar_bytes(&bytes, "sodium-0.6.13.jar", &AutoAdaptOptions::default())
                .is_err()
        );
    }
    assert!(auto_adapt_jar_bytes(&[], "sodium-0.6.13.jar", &AutoAdaptOptions::default()).is_err());
    assert!(
        auto_adapt_jar_bytes(b"not a zip", "newmod.jar", &AutoAdaptOptions::default()).is_err()
    );
}

#[test]
fn class_scan_does_not_interpret_resource_text_as_java_code() {
    let bytes = jar(&[(
        "readme.txt",
        b"java/lang/ProcessBuilder org/lwjgl/opengl/GL11",
    )]);
    let report = inspect_jar_security_and_capabilities(&bytes, "data-only.jar").unwrap();
    assert!(matches!(
        report.decision,
        ModRouteDecision::StandardPipeline
    ));
    assert_eq!(report.signals.class_files_inspected, 0);
}

#[test]
fn corrupted_resource_crc_fails_closed_even_for_known_catalog_id() {
    let mut bytes = jar(&[("asset/readme.txt", b"payload")]);
    let payload_offset = bytes
        .windows(b"payload".len())
        .position(|window| window == b"payload")
        .expect("fixture contains payload");
    bytes[payload_offset] ^= 1;
    assert!(auto_adapt_jar_bytes(&bytes, "sodium-0.6.13.jar", &AutoAdaptOptions::default()).is_err());
}
