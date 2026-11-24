// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room regression tests for the three defects real production
//! NeoForge 1.21.1 JARs exposed (Lane REAL-MODS-FIX-1):
//!
//! 1. The JAR security scan / data-extraction member and byte bounds were
//!    sized for toy fixtures (8,192 members / 32 MiB / 2,048 classes) and
//!    rejected a real `create`-class archive — a current production build
//!    ships ~12,500 members, ~2,700 classes and ~40 MiB uncompressed.
//! 2. The mixin bytecode scanner rejected the `CONSTANT_Utf8`
//!    `class_info_index` that JVMS §4.7.16.1 requires for `c`
//!    element values — every `javac`-compiled `@Mixin(Foo.class)` stores
//!    the field descriptor `Lnet/minecraft/...;`, not a `CONSTANT_Class`.
//! 3. The behavioral engine-replacement route auto-skipped any mod whose
//!    constant pools mentioned `RenderSystem`/`com/mojang/blaze3d/` —
//!    i.e. every HUD/library/gameplay mod that draws anything client
//!    side — instead of only mods whose *dominant* capability is a
//!    native engine subsystem.
//!
//! All fixtures are synthetic bytes built here — no third-party JARs.

use std::collections::HashMap;

use mod_adapter_core::auto_adapter::{auto_adapt_jar_bytes, AdaptationTier, AutoAdaptOptions};
use mod_adapter_core::class_mixin_scanner::parse_mixin_class;
use mod_adapter_core::mixin_lifter::{InjectPoint, MixinAnnotationKind};
use mod_adapter_core::mod_router::{
    inspect_jar_security_and_capabilities, ModRouteDecision, NativeSubsystem, RouteMatchKind,
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

// ---------------------------------------------------------------------------
// JVM .class builder — enough of JVMS §4 for constant-pool + annotations
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
}

impl Cp {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            utf8: HashMap::new(),
            class: HashMap::new(),
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
        let index = self.push(CpItem::Utf8(s.as_bytes().to_vec()));
        self.utf8.insert(s.as_bytes().to_vec(), index);
        index
    }

    fn class(&mut self, internal_name: &str) -> u16 {
        if let Some(&i) = self.class.get(internal_name.as_bytes()) {
            return i;
        }
        let name_index = self.utf8(internal_name);
        let index = self.push(CpItem::Class(name_index));
        self.class.insert(internal_name.as_bytes().to_vec(), index);
        index
    }

    fn int(&mut self, v: i32) -> u16 {
        self.push(CpItem::Int(v))
    }

    fn count(&self) -> u16 {
        (self.items.len() + 1) as u16
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

/// One `element_value` for the test builder.
enum Ev {
    /// `s` string constant (`CONSTANT_Utf8`).
    S(&'static str),
    /// `Z` boolean (a `CONSTANT_Integer` slot).
    Z(bool),
    /// `c` class literal as `javac` stores it per JVMS §4.7.16.1: a
    /// `CONSTANT_Utf8` holding the field descriptor (`Lnet/...;`).
    ClsDescriptor(&'static str),
    /// `c` class literal as a `CONSTANT_Class` (non-`javac` emitters and
    /// older synthetic fixtures).
    ClsConstant(&'static str),
    /// `[` array of element values.
    Arr(Vec<Ev>),
    /// `@` nested annotation (used for `@At`).
    Nested(Ann),
}

struct Ann {
    desc: &'static str,
    elems: Vec<(&'static str, Ev)>,
}

fn ann(desc: &'static str, elems: Vec<(&'static str, Ev)>) -> Ann {
    Ann { desc, elems }
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
        Ev::ClsDescriptor(desc) => {
            out.push(b'c');
            out.extend(cp.utf8(desc).to_be_bytes());
        }
        Ev::ClsConstant(internal) => {
            out.push(b'c');
            out.extend(cp.class(internal).to_be_bytes());
        }
        Ev::Arr(items) => {
            out.push(b'[');
            out.extend((items.len() as u16).to_be_bytes());
            for item in items {
                write_ev(out, cp, item);
            }
        }
        Ev::Nested(nested) => {
            out.push(b'@');
            write_ann(out, cp, nested);
        }
    }
}

fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, ann: &Ann) {
    out.extend(cp.utf8(ann.desc).to_be_bytes());
    out.extend((ann.elems.len() as u16).to_be_bytes());
    for (name, ev) in &ann.elems {
        out.extend(cp.utf8(name).to_be_bytes());
        write_ev(out, cp, ev);
    }
}

fn anns_payload(cp: &mut Cp, anns: &[&Ann]) -> Vec<u8> {
    let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
    for ann in anns {
        write_ann(&mut payload, cp, ann);
    }
    payload
}

struct Meth {
    name: &'static str,
    anns: Vec<Ann>,
}

/// A complete `.class`: `internal` extends `java/lang/Object` with the
/// given methods (each a `()V` stub) and class-level annotations, all in
/// `RuntimeInvisibleAnnotations` — Mixin's real retention.
fn build_class(internal: &str, class_anns: Vec<Ann>, methods: Vec<Meth>) -> Vec<u8> {
    let mut cp = Cp::new();
    cp.class(internal);
    cp.class("java/lang/Object");
    cp.utf8("RuntimeInvisibleAnnotations");
    cp.utf8("()V");
    for m in &methods {
        cp.utf8(m.name);
    }

    let mut method_attrs: Vec<Vec<(u16, Vec<u8>)>> = Vec::new();
    for m in &methods {
        let mut attrs = Vec::new();
        if !m.anns.is_empty() {
            let group: Vec<&Ann> = m.anns.iter().collect();
            attrs.push((cp.utf8("RuntimeInvisibleAnnotations"), anns_payload(&mut cp, &group)));
        }
        method_attrs.push(attrs);
    }
    let mut class_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
    if !class_anns.is_empty() {
        let group: Vec<&Ann> = class_anns.iter().collect();
        class_attrs.push((cp.utf8("RuntimeInvisibleAnnotations"), anns_payload(&mut cp, &group)));
    }

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes()); // public + super
    out.extend(cp.class[internal.as_bytes()].to_be_bytes());
    out.extend(cp.class[b"java/lang/Object".as_slice()].to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces_count
    out.extend(0u16.to_be_bytes()); // fields_count
    out.extend((methods.len() as u16).to_be_bytes());
    for (m, attrs) in methods.iter().zip(&method_attrs) {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(cp.utf8[m.name.as_bytes()].to_be_bytes());
        out.extend(cp.utf8[b"()V".as_slice()].to_be_bytes());
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

/// A bare `.class` carrying arbitrary `CONSTANT_Utf8` strings — the same
/// shape the capability scanner's signature extraction sees.
fn plain_class(internal: &str, strings: &[&str]) -> Vec<u8> {
    let mut cp = Cp::new();
    cp.class(internal);
    cp.class("java/lang/Object");
    for s in strings {
        cp.utf8(s);
    }
    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(cp.class[internal.as_bytes()].to_be_bytes());
    out.extend(cp.class[b"java/lang/Object".as_slice()].to_be_bytes());
    out.extend([0u8; 8]); // interfaces, fields, methods, class attributes
    out
}

const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const INJECT: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
const AT: &str = "Lorg/spongepowered/asm/mixin/injection/At;";

fn inject_at_head(method: &'static str) -> Meth {
    Meth {
        name: "handler$inject",
        anns: vec![ann(
            INJECT,
            vec![
                ("method", Ev::S(method)),
                ("at", Ev::Nested(ann(AT, vec![("value", Ev::S("HEAD"))]))),
                ("cancellable", Ev::Z(true)),
            ],
        )],
    }
}

// ---------------------------------------------------------------------------
// Bug 2 — JVMS §4.7.16.1 `c` element values
// ---------------------------------------------------------------------------

/// The `javac` encoding of `@Mixin(Foo.class)`: `class_info_index` points
/// at a `CONSTANT_Utf8` holding `Lnet/minecraft/...;` — the shape every
/// real NeoForge mixin class carries and the one the scanner used to
/// reject with "wrong tag for @Mixin target class".
#[test]
fn mixin_class_literal_utf8_descriptor_parses() {
    let class = build_class(
        "com/example/mixin/CampfireBaleMixin",
        vec![ann(
            MIXIN,
            vec![("value", Ev::ClsDescriptor("Lnet/minecraft/world/level/block/CampfireBlock;"))],
        )],
        vec![inject_at_head("isSmokeSource")],
    );
    let sites = parse_mixin_class(&class).expect("javac 'c' Utf8 descriptor parses");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].target_class, "net/minecraft/world/level/block/CampfireBlock");
    assert_eq!(sites[0].target_method, "isSmokeSource");
    assert_eq!(sites[0].annotation, MixinAnnotationKind::Inject);
    assert_eq!(sites[0].at, Some(InjectPoint::Head));
}

/// `@Mixin(value = {A.class, B.class})` — an array of `c` values where
/// each `class_info_index` is a `CONSTANT_Utf8` descriptor — plus a
/// `targets` `s` string, all in one annotation.
#[test]
fn mixin_value_array_and_targets_string_parse() {
    let class = build_class(
        "com/example/mixin/MultiMixin",
        vec![ann(
            MIXIN,
            vec![
                (
                    "value",
                    Ev::Arr(vec![
                        Ev::ClsDescriptor("Lnet/minecraft/world/entity/Entity;"),
                        Ev::ClsDescriptor("Lnet/minecraft/server/level/ServerEntity;"),
                    ]),
                ),
                ("targets", Ev::S("net.minecraft.world.level.ChunkMap")),
            ],
        )],
        vec![inject_at_head("tick")],
    );
    let sites = parse_mixin_class(&class).expect("array of 'c' + 's' targets parses");
    let mut targets: Vec<&str> = sites.iter().map(|s| s.target_class.as_str()).collect();
    targets.sort();
    assert_eq!(
        targets,
        [
            "net.minecraft.world.level.ChunkMap",
            "net/minecraft/server/level/ServerEntity",
            "net/minecraft/world/entity/Entity"
        ]
    );
    assert!(sites.iter().all(|s| s.target_method == "tick"));
}

/// Emitters that store a `CONSTANT_Class` at `class_info_index` keep
/// working — the descriptor-vs-internal-name difference resolves to the
/// same target.
#[test]
fn mixin_class_literal_constant_class_still_parses() {
    let class = build_class(
        "com/example/mixin/LegacyMixin",
        vec![ann(
            MIXIN,
            vec![(
                "value",
                Ev::Arr(vec![
                    Ev::ClsConstant("net/minecraft/world/entity/LivingEntity"),
                    Ev::ClsDescriptor("Lnet/minecraft/world/entity/Player;"),
                ]),
            )],
        )],
        vec![inject_at_head("hurt")],
    );
    let sites = parse_mixin_class(&class).expect("mixed CONSTANT_Class + Utf8 'c' parses");
    let mut targets: Vec<&str> = sites.iter().map(|s| s.target_class.as_str()).collect();
    targets.sort();
    assert_eq!(
        targets,
        [
            "net/minecraft/world/entity/LivingEntity",
            "net/minecraft/world/entity/Player"
        ]
    );
}

/// A `c` element value pointing at neither `CONSTANT_Utf8` nor
/// `CONSTANT_Class` still fails closed — the relaxed resolution must not
/// accept garbage.
#[test]
fn mixin_class_literal_wrong_tag_fails_closed() {
    let mut cp = Cp::new();
    cp.class("com/example/mixin/BadMixin");
    cp.class("java/lang/Object");
    cp.utf8("RuntimeInvisibleAnnotations");
    let bad_index = cp.int(0); // 'c' -> CONSTANT_Integer: hard error
    let mixin_desc = cp.utf8(MIXIN);
    let value_name = cp.utf8("value");

    // Hand-assemble: annotation payload referencing the Integer slot.
    let mut payload = (1u16).to_be_bytes().to_vec();
    payload.extend(mixin_desc.to_be_bytes());
    payload.extend((1u16).to_be_bytes());
    payload.extend(value_name.to_be_bytes());
    payload.push(b'c');
    payload.extend(bad_index.to_be_bytes());

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(cp.class[b"com/example/mixin/BadMixin".as_slice()].to_be_bytes());
    out.extend(cp.class[b"java/lang/Object".as_slice()].to_be_bytes());
    out.extend([0u8; 6]); // interfaces, fields, methods
    out.extend(1u16.to_be_bytes()); // attributes_count
    out.extend(cp.utf8[b"RuntimeInvisibleAnnotations".as_slice()].to_be_bytes());
    out.extend((payload.len() as u32).to_be_bytes());
    out.extend(&payload);

    let err = parse_mixin_class(&out).unwrap_err();
    assert!(
        err.to_string().contains("wrong tag"),
        "expected wrong-tag failure, got {err}"
    );
}

// ---------------------------------------------------------------------------
// Bug 1 — production-scale archive bounds
// ---------------------------------------------------------------------------

/// A `create`-scale archive (~9,000 members — past the old 8,192 cap)
/// scans cleanly; a member-count bomb past the new 32,768 cap still
/// fails closed.
#[test]
fn jar_member_bounds_cover_production_jars_but_reject_bombs() {
    let mut members: Vec<(String, &[u8])> = Vec::new();
    let payload: &[u8] = b"x";
    for i in 0..9_000 {
        members.push((format!("assets/pack/{i}.txt"), payload));
    }
    let class = plain_class("pack/Mod", &["com/mojang/blaze3d/systems/RenderSystem"]);
    members.push(("pack/Mod.class".to_string(), &class));
    let refs: Vec<(&str, &[u8])> = members.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    let bytes = jar(&refs);
    let report = inspect_jar_security_and_capabilities(&bytes, "big-pack.jar")
        .expect("9,000-member JAR scans cleanly");
    assert_eq!(report.signals.class_files_inspected, 1);

    let mut members: Vec<(String, &[u8])> = Vec::new();
    for i in 0..32_769 {
        members.push((format!("bomb/{i}.txt"), payload));
    }
    let refs: Vec<(&str, &[u8])> = members.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    let bytes = jar(&refs);
    let err = inspect_jar_security_and_capabilities(&bytes, "member-bomb.jar").unwrap_err();
    assert!(
        err.contains("member count exceeds"),
        "expected member-count rejection, got {err}"
    );
}

// ---------------------------------------------------------------------------
// Bug 3 — behavioral engine-replacement route
// ---------------------------------------------------------------------------

/// A HUD/gameplay mod that merely imports `RenderSystem`, `blaze3d`, and
/// LWJGL in a couple of client classes — the appleskin/balm shape — must
/// NOT be auto-skipped as a `wgpu_render_backend` engine replacement.
#[test]
fn hud_mod_render_imports_are_not_an_engine_replacement() {
    let render_class = plain_class(
        "hud/Overlay",
        &["com/mojang/blaze3d/systems/RenderSystem", "org/lwjgl/opengl/GL11"],
    );
    let hud_class = plain_class("hud/Icon", &["com/mojang/blaze3d/vertex/PoseStack"]);
    let members: Vec<(&str, &[u8])> = vec![
        ("hud/Overlay.class", &render_class),
        ("hud/Icon.class", &hud_class),
    ];
    let mut plain: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..20 {
        plain.push((
            format!("hud/logic/FoodStats{i}.class"),
            plain_class(&format!("hud/logic/FoodStats{i}"), &["net/minecraft/world/food/FoodData"]),
        ));
    }
    let all: Vec<(&str, &[u8])> = members
        .iter()
        .map(|(n, b)| (*n, *b))
        .chain(plain.iter().map(|(n, b)| (n.as_str(), b.as_slice())))
        .collect();
    let bytes = jar(&all);

    let report = inspect_jar_security_and_capabilities(&bytes, "appleskin-like.jar").unwrap();
    // The render hooks are still *reported* — only the routing changes.
    assert!(!report.signals.render_engine_hooks.is_empty());
    assert_eq!(report.signals.render_hook_classes, 2);
    assert_eq!(report.signals.class_files_inspected, 22);
    assert!(
        matches!(report.decision, ModRouteDecision::StandardPipeline),
        "{:?}",
        report.decision
    );

    let result =
        auto_adapt_jar_bytes(&bytes, "appleskin-like.jar", &AutoAdaptOptions::default()).unwrap();
    assert_ne!(result.tier, AdaptationTier::AutoSkipped);
}

/// A library mod with gameplay data (`data/*/tags`) plus a stray
/// `RenderSystem` import — the balm shape — lands in `hybrid_split`, not
/// an auto-skip.
#[test]
fn library_mod_with_tags_data_is_not_auto_skipped() {
    let render_class = plain_class("lib/Icon", &["com/mojang/blaze3d/systems/RenderSystem"]);
    let tag = br#"{"values":["minecraft:iron_ingot"]}"#;
    let mut classes: Vec<(String, Vec<u8>)> = vec![("lib/Icon.class".into(), render_class)];
    for i in 0..10 {
        classes.push((
            format!("lib/api/Service{i}.class"),
            plain_class(&format!("lib/api/Service{i}"), &["net/blay09/mods/balm/Balm"]),
        ));
    }
    let mut members: Vec<(&str, &[u8])> =
        classes.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
    members.push(("data/c/tags/item/ingots.json", tag.as_slice()));

    let bytes = jar(&members);
    let report = inspect_jar_security_and_capabilities(&bytes, "balm-like.jar").unwrap();
    assert!(report.signals.has_gameplay_content);
    assert!(
        matches!(
            report.decision,
            ModRouteDecision::HybridSplit {
                client_render_fallback: NativeSubsystem::WgpuRenderBackend,
                ..
            }
        ),
        "{:?}",
        report.decision
    );

    let result =
        auto_adapt_jar_bytes(&bytes, "balm-like.jar", &AutoAdaptOptions::default()).unwrap();
    assert_ne!(result.tier, AdaptationTier::AutoSkipped);
}

/// Control: a JAR whose classes overwhelmingly hook the render pipeline
/// still behaviorally routes to `wgpu_render_backend` — the signature
/// was tightened, not removed.
#[test]
fn render_dominant_jar_still_routes_to_wgpu_backend() {
    let engine_a = plain_class(
        "engine/Pipeline",
        &[
            "org/lwjgl/opengl/GL11",
            "com/mojang/blaze3d/systems/RenderSystem",
            "net/minecraft/client/renderer/LevelRenderer",
        ],
    );
    let engine_b = plain_class(
        "engine/Sections",
        &["net/minecraft/client/renderer/chunk/SectionRenderDispatcher", "GlStateManager"],
    );
    let util = plain_class("engine/Util", &["java/lang/Math"]);
    let bytes = jar(&[
        ("engine/Pipeline.class", &engine_a),
        ("engine/Sections.class", &engine_b),
        ("engine/Util.class", &util),
    ]);
    let report = inspect_jar_security_and_capabilities(&bytes, "future-renderer.jar").unwrap();
    assert_eq!(report.signals.render_hook_classes, 2);
    assert!(
        matches!(
            report.decision,
            ModRouteDecision::RouteToNativeSubsystem {
                subsystem: NativeSubsystem::WgpuRenderBackend,
                matched_by: RouteMatchKind::BehavioralCapability,
                ..
            }
        ),
        "{:?}",
        report.decision
    );
}

/// Control: stray `ChunkMap`/`ServerChunkCache` references (a mixin
/// accessor or two — the balm/waystones shape) must not auto-skip to the
/// native server engine either.
#[test]
fn scattered_server_engine_imports_are_not_an_engine_replacement() {
    let accessor = plain_class("lib/ChunkMapAccessor", &["net/minecraft/server/level/ChunkMap"]);
    let mut members: Vec<(String, Vec<u8>)> = vec![("lib/ChunkMapAccessor.class".into(), accessor)];
    for i in 0..8 {
        members.push((
            format!("lib/core/Thing{i}.class"),
            plain_class(&format!("lib/core/Thing{i}"), &["net/minecraft/world/item/Item"]),
        ));
    }
    let refs: Vec<(&str, &[u8])> = members.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
    let bytes = jar(&refs);
    let report = inspect_jar_security_and_capabilities(&bytes, "waystones-like.jar").unwrap();
    assert_eq!(report.signals.server_hook_classes, 1);
    assert!(
        matches!(report.decision, ModRouteDecision::StandardPipeline),
        "{:?}",
        report.decision
    );
}
