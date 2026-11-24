// SPDX-License-Identifier: LGPL-3.0-or-later

//! Clean-room regression tests for the two false-positive
//! security-quarantine defects the 26-mod real Top-100 1.21.1 gauntlet
//! exposed (Lane GAUNTLET-FIX-26):
//!
//! 1. The constant-pool security scan matched owner/method indicators
//!    over *any* UTF-8 entries, so a class calling `System.arraycopy`
//!    beside an unrelated `load()` method looked like `System#load`.
//!    Owner+name pairs now resolve through real
//!    `CONSTANT_Methodref`/`CONSTANT_InterfaceMethodref` entries and
//!    class indicators through real `CONSTANT_Class` entries.
//! 2. Superseded engine-replacement mods (`sodium`, `lithium`, `iris`,
//!    ...) were quarantined by their own bytecode even though megu
//!    never executes a single byte of them — the verified catalog id
//!    now resolves to a native subsystem before the bytecode quarantine
//!    verdict (embedded native/executable *members* still quarantine).
//!
//! All fixtures are synthetic bytes built here — no third-party JARs.

use mod_adapter_core::auto_adapter::{auto_adapt_jar_bytes, AdaptationTier, AutoAdaptOptions};
use mod_adapter_core::mod_router::{
    inspect_jar_security_and_capabilities, ModRouteDecision, NativeSubsystem, RouteMatchKind,
    SecurityThreatKind,
};

// ---------------------------------------------------------------------------
// ZIP/JAR builder (stored members, real central directory + EOCD)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Structured constant-pool builder: real CONSTANT_Class /
// CONSTANT_NameAndType / CONSTANT_Methodref /
// CONSTANT_InterfaceMethodref entries — a security fixture must carry the
// resolved references a javac-compiled class carries, not bare strings.
// ---------------------------------------------------------------------------

enum CpItem {
    Utf8(String),
    Class(u16),
    NameAndType(u16, u16),
    MethodRef(u16, u16),
    IfaceMethodRef(u16, u16),
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
    fn name_and_type(&mut self, name: &str, desc: &str) -> u16 {
        let name_index = self.utf8(name);
        let desc_index = self.utf8(desc);
        self.push(CpItem::NameAndType(name_index, desc_index))
    }
    fn method_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let class_index = self.class(owner);
        let nat_index = self.name_and_type(name, desc);
        self.push(CpItem::MethodRef(class_index, nat_index))
    }
    fn iface_method_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let class_index = self.class(owner);
        let nat_index = self.name_and_type(name, desc);
        self.push(CpItem::IfaceMethodRef(class_index, nat_index))
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
                CpItem::IfaceMethodRef(class_index, nat_index) => {
                    out.push(11);
                    out.extend(class_index.to_be_bytes());
                    out.extend(nat_index.to_be_bytes());
                }
            }
        }
        out
    }
}

/// A minimal structurally-valid `.class` whose constant pool carries
/// exactly the entries `fill` adds on top of the `internal` /
/// `java/lang/Object` class constants every class needs.
fn class_with_pool(internal: &str, fill: impl FnOnce(&mut Cp)) -> Vec<u8> {
    let mut cp = Cp::default();
    let this_idx = cp.class(internal);
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

// ---------------------------------------------------------------------------
// Bug 1: exact Methodref owner+name matching.
// ---------------------------------------------------------------------------

#[test]
fn methodref_security_scan_allows_system_arraycopy_plus_unrelated_load_and_quarantines_real_system_load(
) {
    // The exact false-positive shape the live gauntlet exposed: a real
    // Methodref to `System.arraycopy` (or `nanoTime`) beside an unrelated
    // `load` method — the co-occurring UTF-8s fabricated `System#load`.
    let benign = class_with_pool("example/Benign", |cp| {
        cp.method_ref(
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        );
        cp.method_ref("com/example/Chunk", "load", "()V");
        cp.utf8("java/lang/System#load"); // a literal is not a reference
        cp.utf8("()Ljava/net/Socket;"); // a descriptor is not a reference
    });
    for (display, bytes) in [
        ("inspect", jar(&[("example/Benign.class", &benign)])),
        (
            "adapt",
            jar(&[
                ("fabric.mod.json", manifest("somegameplaymod").as_slice()),
                ("example/Benign.class", &benign),
                (
                    "data/somegameplaymod/recipes/item.json",
                    br#"{"type":"minecraft:crafting_shapeless"}"#,
                ),
            ]),
        ),
    ] {
        let report =
            inspect_jar_security_and_capabilities(&bytes, "clean.jar").expect("clean class parses");
        assert!(
            !matches!(report.decision, ModRouteDecision::SecurityQuarantine { .. }),
            "{display}: {:?}",
            report.decision
        );
    }
    let adapted = adapt(
        &jar(&[
            ("fabric.mod.json", manifest("somegameplaymod").as_slice()),
            ("example/Benign.class", &benign),
            (
                "data/somegameplaymod/recipes/item.json",
                br#"{"type":"minecraft:crafting_shapeless","ingredients":["minecraft:stick"],"result":{"id":"minecraft:diamond_sword","count":1}}"#,
            ),
        ]),
        "somegameplaymod-1.0.jar",
    );
    assert!(
        adapted.security_threats.is_empty(),
        "{:?}",
        adapted.security_threats
    );
    assert!(
        !matches!(adapted.route, ModRouteDecision::SecurityQuarantine { .. }),
        "{:?}",
        adapted.route
    );
    assert!(
        adapted
            .blockers
            .iter()
            .all(|b| !b.contains("SECURITY_QUARANTINE")),
        "{:?}",
        adapted.blockers
    );

    // The same UTF-8 strings without any resolved Methodref — the pure
    // historical false-positive shape — must not quarantine either.
    let stray_strings = class_with_pool("example/Stray", |cp| {
        cp.utf8("java/lang/System");
        cp.utf8("load");
        cp.utf8("java/lang/Runtime");
        cp.utf8("exec");
    });
    let report = inspect_jar_security_and_capabilities(
        &jar(&[("example/Stray.class", &stray_strings)]),
        "stray.jar",
    )
    .unwrap();
    assert!(!matches!(
        report.decision,
        ModRouteDecision::SecurityQuarantine { .. }
    ));

    // Every real paired reference still strictly quarantines — 100%
    // security preserved.
    type Case = (fn(&mut Cp), SecurityThreatKind, &'static str);
    let cases: [Case; 8] = [
        (
            |cp: &mut Cp| {
                cp.method_ref("java/lang/System", "load", "(Ljava/lang/String;)V");
            },
            SecurityThreatKind::NativeLibraryLoad,
            "java/lang/System#load",
        ),
        (
            |cp: &mut Cp| {
                cp.method_ref("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V");
            },
            SecurityThreatKind::NativeLibraryLoad,
            "java/lang/System#loadLibrary",
        ),
        (
            |cp: &mut Cp| {
                cp.iface_method_ref("java/lang/System", "load", "(Ljava/lang/String;)V");
            },
            SecurityThreatKind::NativeLibraryLoad,
            "java/lang/System#load",
        ),
        (
            |cp: &mut Cp| {
                cp.method_ref(
                    "java/lang/Runtime",
                    "exec",
                    "(Ljava/lang/String;)Ljava/lang/Process;",
                );
            },
            SecurityThreatKind::ProcessExecution,
            "java/lang/Runtime#exec",
        ),
        (
            |cp: &mut Cp| {
                cp.method_ref("java/lang/ClassLoader", "defineClass", "([BII)Ljava/lang/Class;");
            },
            SecurityThreatKind::DynamicCodeLoading,
            "java/lang/ClassLoader#defineClass",
        ),
        (
            |cp: &mut Cp| {
                cp.class("java/lang/ProcessBuilder");
            },
            SecurityThreatKind::ProcessExecution,
            "java/lang/ProcessBuilder",
        ),
        (
            |cp: &mut Cp| {
                cp.class("java/lang/ProcessBuilder$Redirect");
            },
            SecurityThreatKind::ProcessExecution,
            "java/lang/ProcessBuilder",
        ),
        (
            |cp: &mut Cp| {
                cp.class("sun/misc/Unsafe");
            },
            SecurityThreatKind::UnsafeMemoryAccess,
            "sun/misc/Unsafe",
        ),
    ];
    for (fill, kind, indicator) in cases {
        let hostile = class_with_pool("example/Evil", fill);
        let report = inspect_jar_security_and_capabilities(
            &jar(&[("example/Evil.class", &hostile)]),
            "evil.jar",
        )
        .unwrap();
        let ModRouteDecision::SecurityQuarantine { threats } = &report.decision else {
            panic!("{indicator} must quarantine: {:?}", report.decision);
        };
        assert!(
            threats
                .iter()
                .any(|t| t.kind == kind
                    && t.member == "example/Evil.class"
                    && t.indicator == indicator),
            "{indicator}: {threats:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Bug 2: superseded perf/shader mods auto-skip before bytecode quarantine.
// ---------------------------------------------------------------------------

#[test]
fn superseded_perf_mod_auto_skips_before_quarantine_since_zero_bytecode_runs() {
    // The real gauntlet shapes: sodium probes `Runtime#exec` + JNA,
    // lithium and iris touch `sun/misc/Unsafe`, iris also pairs
    // `System#load`. None of it ever executes under megu.
    for (jar_name, hostile) in [
        (
            "sodium-neoforge-0.8.13+mc1.21.1.jar",
            class_with_pool("net/caffeinemc/mods/sodium/Probe", |cp| {
                cp.method_ref(
                    "java/lang/Runtime",
                    "exec",
                    "(Ljava/lang/String;)Ljava/lang/Process;",
                );
                cp.class("com/sun/jna/Platform");
            }),
        ),
        (
            "lithium-neoforge-0.15.4+mc1.21.1.jar",
            class_with_pool("net/caffeinemc/mods/lithium/ChunkConstants", |cp| {
                cp.class("sun/misc/Unsafe");
            }),
        ),
        (
            "iris-neoforge-1.8.14+mc1.21.1.jar",
            class_with_pool("net/irisshaders/iris/Iris", |cp| {
                cp.class("sun/misc/Unsafe");
                cp.method_ref("java/lang/System", "load", "(Ljava/lang/String;)V");
            }),
        ),
    ] {
        let manifest_id = jar_name.split('-').next().unwrap();
        let bytes = jar(&[
            ("fabric.mod.json", manifest(manifest_id).as_slice()),
            ("x/Hostile.class", hostile.as_slice()),
        ]);
        let result = adapt(&bytes, jar_name);
        assert_eq!(
            result.tier,
            AdaptationTier::AutoSkipped,
            "{jar_name}: {:?}",
            result.blockers
        );
        assert!(
            matches!(
                result.route,
                ModRouteDecision::RouteToNativeSubsystem {
                    matched_by: RouteMatchKind::KnownCatalogId,
                    ..
                }
            ),
            "{jar_name}: {:?}",
            result.route
        );
        assert!(
            result.security_threats.is_empty(),
            "{jar_name}: bytecode never runs, threats are not the verdict"
        );
        assert!(result
            .audit_notes
            .iter()
            .any(|n| n.contains("BUILTIN_SUPERSEDED_PERF_MODS")));
    }

    // A renamed JAR whose *manifest* declares the superseded id resolves
    // the same way — the verified id is checked before the bytecode
    // quarantine verdict, not only the filename.
    let renamed = jar(&[
        ("fabric.mod.json", manifest("lithium").as_slice()),
        (
            "x/Hostile.class",
            class_with_pool("x/Hostile", |cp| {
                cp.class("sun/misc/Unsafe");
            })
            .as_slice(),
        ),
    ]);
    let result = adapt(&renamed, "totally-not-lithium.jar");
    assert_eq!(result.tier, AdaptationTier::AutoSkipped, "{:?}", result.blockers);
    assert_eq!(result.mod_id(), Some("lithium"));

    // An unknown mod carrying the very same bytecode is still strictly
    // quarantined — the skip is the catalog id, not a weaker scan.
    let unknown = jar(&[
        ("fabric.mod.json", manifest("mysterymod").as_slice()),
        (
            "x/Hostile.class",
            class_with_pool("x/Hostile", |cp| {
                cp.method_ref(
                    "java/lang/Runtime",
                    "exec",
                    "(Ljava/lang/String;)Ljava/lang/Process;",
                );
                cp.class("sun/misc/Unsafe");
            })
            .as_slice(),
        ),
    ]);
    let result = adapt(&unknown, "mysterymod-1.0.jar");
    assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
    assert!(matches!(
        result.route,
        ModRouteDecision::SecurityQuarantine { .. }
    ));
    assert!(result
        .security_threats
        .iter()
        .any(|t| t.kind == SecurityThreatKind::ProcessExecution));
    assert!(result
        .security_threats
        .iter()
        .any(|t| t.kind == SecurityThreatKind::UnsafeMemoryAccess));

    // But a *hostile archive member* renamed after a known mod still
    // quarantines — the skip covers bytecode only, never executable
    // payloads smuggled inside the ZIP.
    let spoofed = jar(&[
        ("fabric.mod.json", manifest("sodium").as_slice()),
        ("lib/native.dll", b"MZ\x90\0"),
    ]);
    let result = adapt(&spoofed, "sodium-0.8.13.jar");
    assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
    assert!(matches!(
        result.route,
        ModRouteDecision::SecurityQuarantine { .. }
    ));
    assert_eq!(
        result.security_threats[0].kind,
        SecurityThreatKind::EmbeddedNativeOrExecutable
    );

    // Routing keeps its per-mod subsystem mapping through the skip.
    for (id, subsystem) in [
        ("starlight", NativeSubsystem::LightEngine),
        ("c2me", NativeSubsystem::ServerChunkAndNetEngine),
        ("ferritecore", NativeSubsystem::CompactRegistryAndMemory),
        ("sodium", NativeSubsystem::WgpuRenderBackend),
        ("iris", NativeSubsystem::WgpuRenderBackend),
    ] {
        let bytes = jar(&[("fabric.mod.json", manifest(id).as_slice())]);
        let report = mod_adapter_core::mod_router::route_mod_jar(
            &bytes,
            &format!("{id}-1.21.1.jar"),
            Some(id),
        )
        .unwrap();
        assert!(
            matches!(
                report.decision,
                ModRouteDecision::RouteToNativeSubsystem {
                    subsystem: s,
                    matched_by: RouteMatchKind::KnownCatalogId,
                    ..
                } if s == subsystem
            ),
            "{id}: {:?}",
            report.decision
        );
    }
}
