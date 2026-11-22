// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane MOD-TIER2-SINYTRA-FREE: the last two Tier 2 gaps between Fabric
//! and NeoForge, without Sinytra Connector.
//!
//! 1. Runtime Tiny v2 mapping loader + clean-room canonical
//!    Intermediary→Mojmap hook resolver (`intermediary_map` +
//!    `lift_mixin_sites_with_mappings`): a Fabric Mixin written against
//!    Intermediary names resolves to the exact same `StandardEventHook`
//!    as the equivalent NeoForge Mojmap Mixin.
//! 2. Non-blocking `@Shadow`/`@Unique` declarations: they reference or
//!    mark members, never inject — recorded as declarations in scan
//!    telemetry, never counted as Tier-3 blockers, while every mutating
//!    annotation (`@Overwrite`, `@Redirect`, `@ModifyVariable`, ...) stays
//!    strictly fail closed.

use std::collections::{BTreeSet, HashMap};

use mod_adapter_core::class_mixin_scanner::scan_jar_mixins;
use mod_adapter_core::fallback::{classify_mod_with_mixins, DiscoveredMod, ExecutionTier};
use mod_adapter_core::intermediary_map::{parse_tiny_v2, TinyV2MappingTable};
use mod_adapter_core::mixin_lifter::{
    lift_mixin_sites, lift_mixin_sites_with_mappings, InjectPoint, MixinAnnotationKind,
    MixinLiftVerdict, MixinMemberKind, MixinSite, StandardEventHook, UnliftableReason,
};
use mod_adapter_core::version_compat::McVersion;

// ---------------------------------------------------------------------------
// JVM .class file builder (minimal variant supporting annotated fields)
// ---------------------------------------------------------------------------

enum CpItem {
    Utf8(Vec<u8>),
    Class(u16),
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

    fn utf8_idx(&self, s: &str) -> u16 {
        self.utf8[s.as_bytes()]
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

    fn class_idx(&self, internal_name: &str) -> u16 {
        self.class[internal_name.as_bytes()]
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
            }
        }
        out
    }
}

#[derive(Clone)]
enum EV {
    S(String),
    Cls(String),
    Ann(Box<Ann>),
    Arr(Vec<EV>),
}

#[derive(Clone)]
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

fn s(v: &str) -> EV {
    EV::S(v.to_string())
}

fn cls(internal: &str) -> EV {
    EV::Cls(internal.to_string())
}

fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &EV) {
    match ev {
        EV::S(v) => {
            out.push(b's');
            out.extend(cp.utf8(v).to_be_bytes());
        }
        EV::Cls(name) => {
            out.push(b'c');
            out.extend(cp.class(name).to_be_bytes());
        }
        EV::Ann(nested) => {
            out.push(b'@');
            write_ann(out, cp, nested);
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

fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, ann: &Ann) {
    out.extend(cp.utf8(&ann.desc).to_be_bytes());
    out.extend((ann.elems.len() as u16).to_be_bytes());
    for (name, ev) in &ann.elems {
        out.extend(cp.utf8(name).to_be_bytes());
        write_ev(out, cp, ev);
    }
}

fn anns_payload(cp: &mut Cp, anns: &[Ann]) -> Vec<u8> {
    let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
    for ann in anns {
        write_ann(&mut payload, cp, ann);
    }
    payload
}

#[derive(Clone)]
struct Member {
    name: String,
    descriptor: String,
    anns: Vec<Ann>,
}

fn member(name: &str, descriptor: &str, anns: Vec<Ann>) -> Member {
    Member {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        anns,
    }
}

/// Assemble a `.class` file: class annotations + annotated fields +
/// annotated `()V` methods, all mixin annotations in the CLASS-retention
/// `RuntimeInvisibleAnnotations` attribute `javac` emits.
fn build_class(
    internal: &str,
    class_anns: Vec<Ann>,
    fields: Vec<Member>,
    methods: Vec<Member>,
) -> Vec<u8> {
    let mut cp = Cp::new();
    cp.class(internal);
    cp.class("java/lang/Object");
    for s in ["Code", "SourceFile", "RuntimeInvisibleAnnotations", "()V"] {
        cp.utf8(s);
    }
    for m in fields.iter().chain(&methods) {
        cp.utf8(&m.name);
        cp.utf8(&m.descriptor);
    }

    let member_attrs = |members: &[Member], cp: &mut Cp| -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for m in members {
            let mut bytes = Vec::new();
            let mut attrs: Vec<(u16, Vec<u8>)> = vec![];
            if !m.anns.is_empty() {
                attrs.push((
                    cp.utf8("RuntimeInvisibleAnnotations"),
                    anns_payload(cp, &m.anns),
                ));
            }
            // methods get a real Code attribute ahead of annotations.
            if m.descriptor.starts_with('(') {
                let mut code = Vec::new();
                code.extend(1u16.to_be_bytes());
                code.extend(1u16.to_be_bytes());
                code.extend(1u32.to_be_bytes());
                code.push(0xB1);
                code.extend(0u16.to_be_bytes());
                code.extend(0u16.to_be_bytes());
                attrs.insert(0, (cp.utf8("Code"), code));
            }
            bytes.extend((attrs.len() as u16).to_be_bytes());
            for (name_index, payload) in attrs {
                bytes.extend(name_index.to_be_bytes());
                bytes.extend((payload.len() as u32).to_be_bytes());
                bytes.extend(payload);
            }
            out.push(bytes);
        }
        out
    };
    let field_bodies = member_attrs(&fields, &mut cp);
    let method_bodies = member_attrs(&methods, &mut cp);

    let mut class_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
    if !class_anns.is_empty() {
        class_attrs.push((
            cp.utf8("RuntimeInvisibleAnnotations"),
            anns_payload(&mut cp, &class_anns),
        ));
    }
    class_attrs.push((
        cp.utf8("SourceFile"),
        cp.utf8("Dummy.java").to_be_bytes().to_vec(),
    ));

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(cp.class_idx(internal).to_be_bytes());
    out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
    out.extend(0u16.to_be_bytes());

    out.extend((fields.len() as u16).to_be_bytes());
    for (f, body) in fields.iter().zip(&field_bodies) {
        out.extend(0x0002u16.to_be_bytes()); // private
        out.extend(cp.utf8_idx(&f.name).to_be_bytes());
        out.extend(cp.utf8_idx(&f.descriptor).to_be_bytes());
        out.extend(body);
    }
    out.extend((methods.len() as u16).to_be_bytes());
    for (m, body) in methods.iter().zip(&method_bodies) {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(cp.utf8_idx(&m.name).to_be_bytes());
        out.extend(cp.utf8_idx(&m.descriptor).to_be_bytes());
        out.extend(body);
    }
    out.extend((class_attrs.len() as u16).to_be_bytes());
    for (name_index, payload) in &class_attrs {
        out.extend(name_index.to_be_bytes());
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    out
}

// ---------------------------------------------------------------------------
// ZIP/JAR builder (stored members)
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
// Mixin fixture helpers
// ---------------------------------------------------------------------------

const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const INJECT: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
const AT: &str = "Lorg/spongepowered/asm/mixin/injection/At;";
const SHADOW: &str = "Lorg/spongepowered/asm/mixin/Shadow;";
const UNIQUE: &str = "Lorg/spongepowered/asm/mixin/Unique;";

fn at(value: &str) -> EV {
    EV::Ann(Box::new(ann(AT, vec![("value", s(value))])))
}

fn mixin_on(target: &str) -> Ann {
    ann(MIXIN, vec![("value", EV::Arr(vec![cls(target)]))])
}

fn inject_at(method: &str, point: &str) -> Ann {
    ann(INJECT, vec![("method", s(method)), ("at", at(point))])
}

fn site(target_class: &str, target_method: &str, point: InjectPoint) -> MixinSite {
    MixinSite {
        mixin_class: "com/sine/mixin/StandardMixin".into(),
        target_class: target_class.into(),
        target_method: target_method.into(),
        annotation: MixinAnnotationKind::Inject,
        at: Some(point),
        cancellable: false,
        captures_locals: false,
        handler_name: "handler".into(),
        handler_descriptor: "()V".into(),
    }
}

fn hook_of(verdict: &MixinLiftVerdict) -> StandardEventHook {
    match verdict {
        MixinLiftVerdict::FullyLiftable(bindings) if bindings.len() == 1 => bindings[0].hook,
        other => panic!("expected exactly one lifted binding, got {other:?}"),
    }
}

fn mixin_only_mod(mod_id: &str) -> DiscoveredMod {
    DiscoveredMod {
        mod_id: mod_id.into(),
        has_java_bytecode: true,
        shimcheck_compatible: false,
        has_unsupported_mixin_or_at: true,
        rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".into()],
        mc_version_range: None,
    }
}

fn no_ids() -> BTreeSet<String> {
    BTreeSet::new()
}

// ---------------------------------------------------------------------------
// Deliverable 1: Tiny v2 loader + canonical Intermediary->Mojmap resolver
// ---------------------------------------------------------------------------

/// A clean-room Tiny v2 document mapping `net/minecraft/class_1657`
/// (Player) + `method_5643`/`method_5773` and `net/minecraft/class_1309`
/// (LivingEntity) + `method_5643` to canonical `named` names must drive
/// Fabric Intermediary `@Inject` sites to the EXACT SAME
/// `StandardEventHook` as NeoForge Mojmap `@Inject` sites.
#[test]
fn tiny_v2_parser_maps_intermediary_class_and_method_to_canonical_hook() {
    let tiny = "tiny\t2\t0\tintermediary\tnamed\n\
        c\tnet/minecraft/class_1309\tnet/minecraft/world/entity/LivingEntity\n\
        \tm\t(Lnet/minecraft/class_1282;F)Z\tmethod_5643\thurt\n\
        c\tnet/minecraft/class_1657\tnet/minecraft/world/entity/player/Player\n\
        \tm\t()V\tmethod_5773\ttick\n\
        \tm\t(Lnet/minecraft/class_1282;F)Z\tmethod_5643\thurt\n";
    let table = parse_tiny_v2(tiny).expect("clean-room tiny v2 parses");
    assert_eq!(
        table.canonical_class("net/minecraft/class_1657"),
        "net/minecraft/world/entity/player/Player"
    );
    assert_eq!(table.canonical_method("method_5643"), "hurt");

    // Intermediary `class_1309#method_5643` HEAD == Mojmap
    // `LivingEntity#hurt` HEAD — the exact same hook.
    let fabric = lift_mixin_sites_with_mappings(
        &[site(
            "net/minecraft/class_1309",
            "method_5643",
            InjectPoint::Head,
        )],
        &table,
    );
    let neoforge = lift_mixin_sites(&[site(
        "net/minecraft/world/entity/LivingEntity",
        "hurt",
        InjectPoint::Head,
    )]);
    assert_eq!(hook_of(&fabric), StandardEventHook::EntityDamagePre);
    assert_eq!(hook_of(&fabric), hook_of(&neoforge));

    // Intermediary `class_1657#method_5773` HEAD == Mojmap `Player#tick`.
    let fabric = lift_mixin_sites_with_mappings(
        &[site(
            "net/minecraft/class_1657",
            "method_5773",
            InjectPoint::Head,
        )],
        &table,
    );
    let neoforge = lift_mixin_sites(&[site(
        "net/minecraft/world/entity/player/Player",
        "tick",
        InjectPoint::Head,
    )]);
    assert_eq!(hook_of(&fabric), StandardEventHook::PlayerTickPre);
    assert_eq!(hook_of(&fabric), hook_of(&neoforge));

    // `class_1657#method_5643` (Player.hurt) is deliberately NOT a
    // liftable hook: normalization maps it to `Player#hurt`, which the
    // fail-closed target table rejects — identically in both dialects.
    for (class, method) in [
        ("net/minecraft/class_1657", "method_5643"),
        ("net/minecraft/world/entity/player/Player", "hurt"),
    ] {
        match lift_mixin_sites_with_mappings(&[site(class, method, InjectPoint::Head)], &table) {
            MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
                assert_eq!(unliftable.len(), 1);
                assert_eq!(unliftable[0].1, UnliftableReason::UnknownTarget);
            }
            other => panic!("{class}#{method} must stay unliftable, got {other:?}"),
        }
    }

    // Built-in canonical anchors cover the standard hooks even when no
    // `.tiny` document is supplied: `lift_mixin_sites` alone normalizes
    // `class_3218#method_18765` to `ServerLevel#tick`.
    let no_file = lift_mixin_sites(&[site(
        "net/minecraft/class_3218",
        "method_18765",
        InjectPoint::Head,
    )]);
    assert_eq!(hook_of(&no_file), StandardEventHook::ServerTickPre);
    let empty = TinyV2MappingTable::default();
    assert!(empty.is_empty());
    assert_eq!(
        empty.canonical_class("net/minecraft/class_2344"),
        "net/minecraft/world/level/block/FarmBlock"
    );
    assert_eq!(empty.canonical_method("method_10125"), "fallOn");

    // Fail-closed parsing: bad headers, wrong column counts, member
    // rows outside a class, and the 16 MiB bound all reject.
    for bad in [
        "tiny\t1\t9\tintermediary\tnamed\n",
        "v1\tofficial\tintermediary\tnamed\n",
        "tiny\t2\t0\tintermediary\n",
        "tiny\t2\t0\tintermediary\tnamed\n\tm\t()V\tmethod_1\ttick\n",
        "tiny\t2\t0\tintermediary\tnamed\nc\ta\n",
    ] {
        assert!(parse_tiny_v2(bad).is_err(), "{bad:?}");
    }

    // End-to-end: a `*.tiny` member inside the mod JAR feeds the scan's
    // name normalization — an Intermediary class unknown to the built-in
    // anchors still lifts once the shipped mapping names its canonical
    // class.
    let jar_tiny = "tiny\t2\t0\tintermediary\tnamed\n\
        c\tnet/minecraft/class_9999\tnet/minecraft/world/entity/LivingEntity\n\
        \tm\t(Lnet/minecraft/class_1282;F)Z\tmethod_5643\thurt\n";
    let mixin_class = build_class(
        "com/sine/mixin/IntermediaryMixin",
        vec![mixin_on("net/minecraft/class_9999")],
        vec![],
        vec![member(
            "sine$onDamage",
            "()V",
            vec![inject_at("method_5643", "HEAD")],
        )],
    );
    let jar_bytes = jar(&[
        (
            "sine.mixins.json",
            br#"{"package":"com.sine.mixin","mixins":["IntermediaryMixin"]}"#.as_slice(),
        ),
        (
            "com/sine/mixin/IntermediaryMixin.class",
            mixin_class.as_slice(),
        ),
        ("mappings/mappings.tiny", jar_tiny.as_bytes()),
    ]);
    let report = scan_jar_mixins(&jar_bytes).expect("JAR with tiny v2 mappings scans");
    assert_eq!(report.tiny_mappings_loaded, 1);
    assert_eq!(report.tiny_mappings_skipped, 0);
    assert_eq!(report.sites.len(), 1);
    match &report.verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => {
            assert_eq!(bindings.len(), 1);
            assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);
        }
        other => panic!("shipped mappings must lift the intermediary target, got {other:?}"),
    }

    // A malformed `.tiny` member is recorded skipped (never silently
    // used); the built-in anchors still apply.
    let jar_bytes = jar(&[
        (
            "com/sine/mixin/IntermediaryMixin.class",
            mixin_class.as_slice(),
        ),
        ("mappings/mappings.tiny", b"v1\tbroken".as_slice()),
    ]);
    let report = scan_jar_mixins(&jar_bytes).expect("bad tiny member never fails the scan");
    assert_eq!(report.tiny_mappings_loaded, 0);
    assert_eq!(report.tiny_mappings_skipped, 1);
    match &report.verdict {
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
            assert_eq!(unliftable[0].1, UnliftableReason::UnknownTarget);
        }
        other => panic!("unmapped intermediary class stays Tier 3, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Deliverable 2: non-blocking @Shadow / @Unique declarations
// ---------------------------------------------------------------------------

/// A mixin class whose members are `@Shadow` + `@Unique` plus one
/// liftable `@Inject(HEAD)` lifts cleanly to Tier 2 with
/// `declarations_skipped == 2`; adding any mutating annotation
/// (`@Overwrite`, `@Redirect`, `@ModifyVariable`) immediately fails
/// closed to Tier 3.
#[test]
fn shadow_and_unique_declarations_do_not_block_tier2_while_overwrite_and_redirect_fail_closed() {
    assert_eq!(
        MixinAnnotationKind::Shadow.member_kind(),
        MixinMemberKind::Declaration
    );
    assert_eq!(
        MixinAnnotationKind::Unique.member_kind(),
        MixinMemberKind::Declaration
    );
    assert_eq!(
        MixinAnnotationKind::Overwrite.member_kind(),
        MixinMemberKind::Injection
    );

    let base_methods = vec![
        member(
            "getFoodData",
            "()Lnet/minecraft/world/food/FoodData;",
            vec![ann(SHADOW, vec![("method", s("method_7261"))])],
        ),
        member("sine$nutrition", "()V", vec![ann(UNIQUE, vec![])]),
        member("sine$onHurt", "()V", vec![inject_at("hurt", "HEAD")]),
    ];
    let base_fields = vec![member("hunger", "I", vec![ann(SHADOW, vec![])])];
    let build = |extra: Vec<Member>| {
        let mut methods = base_methods.clone();
        methods.extend(extra);
        build_class(
            "com/sine/mixin/PlayerMixin",
            vec![mixin_on("net/minecraft/world/entity/LivingEntity")],
            base_fields.clone(),
            methods,
        )
    };

    // @Shadow (method + field) and @Unique members are declarations:
    // recorded, never sites, never blockers.
    let jar_bytes = jar(&[("com/sine/mixin/PlayerMixin.class", build(vec![]).as_slice())]);
    let report = scan_jar_mixins(&jar_bytes).expect("declaration-only mixin scans");
    assert_eq!(report.sites.len(), 1);
    assert_eq!(report.sites[0].annotation, MixinAnnotationKind::Inject);
    assert_eq!(report.declarations_skipped, 3);
    assert_eq!(report.declarations.len(), 3);
    let has_decl =
        |kind: &MixinAnnotationKind| report.declarations.iter().any(|d| &d.annotation == kind);
    assert!(has_decl(&MixinAnnotationKind::Shadow));
    assert!(has_decl(&MixinAnnotationKind::Unique));
    let shadow = report
        .declarations
        .iter()
        .find(|d| d.annotation == MixinAnnotationKind::Shadow && d.member_name == "getFoodData")
        .expect("method @Shadow declaration recorded");
    assert_eq!(shadow.target_member, "method_7261");
    assert_eq!(
        shadow.target_classes,
        vec!["net/minecraft/world/entity/LivingEntity".to_string()]
    );
    assert!(report
        .declarations
        .iter()
        .any(|d| d.member_name == "hunger" && d.target_member == "hunger"));

    // One lifted site + only declarations => FullyLiftable => Tier 2.
    let MixinLiftVerdict::FullyLiftable(bindings) = &report.verdict else {
        panic!(
            "declarations must not block lifting, got {:?}",
            report.verdict
        );
    };
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);
    let (tier, diagnostic, _) = classify_mod_with_mixins(
        &mixin_only_mod("sine"),
        &report.sites,
        &no_ids(),
        &no_ids(),
        McVersion::V1_21_1,
    );
    assert_eq!(tier, ExecutionTier::BoundedJvmShim, "{diagnostic}");

    // Every mutating annotation stays a hard Tier-3 blocker, declarations
    // or not.
    for (desc, label) in [
        ("Lorg/spongepowered/asm/mixin/Overwrite;", "@Overwrite"),
        (
            "Lorg/spongepowered/asm/mixin/injection/Redirect;",
            "@Redirect",
        ),
        (
            "Lorg/spongepowered/asm/mixin/injection/ModifyVariable;",
            "@ModifyVariable",
        ),
    ] {
        let extra = vec![member(
            "sine$bad",
            "()V",
            vec![ann(desc, vec![("method", s("hurt")), ("at", at("HEAD"))])],
        )];
        let jar_bytes = jar(&[("com/sine/mixin/PlayerMixin.class", build(extra).as_slice())]);
        let report = scan_jar_mixins(&jar_bytes).expect("mutating mixin still scans");
        match &report.verdict {
            MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
                assert!(
                    unliftable
                        .iter()
                        .any(|(_, r)| *r == UnliftableReason::UnsupportedAnnotation),
                    "{label} must fail closed as UnsupportedAnnotation: {unliftable:?}"
                );
            }
            other => panic!("{label} must keep the mod at Tier 3, got {other:?}"),
        }
        let (tier, _, _) = classify_mod_with_mixins(
            &mixin_only_mod("sine"),
            &report.sites,
            &no_ids(),
            &no_ids(),
            McVersion::V1_21_1,
        );
        assert_eq!(tier, ExecutionTier::RequiresFallback, "{label}");
    }

    // Defense in depth: a manually constructed site carrying a
    // declaration annotation still fails closed in the lifter itself.
    let mut shadowed = site(
        "net/minecraft/world/entity/LivingEntity",
        "hurt",
        InjectPoint::Head,
    );
    shadowed.annotation = MixinAnnotationKind::Shadow;
    match lift_mixin_sites(&[shadowed]) {
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
            assert_eq!(unliftable[0].1, UnliftableReason::UnsupportedAnnotation);
        }
        other => panic!("hand-built @Shadow site must fail closed, got {other:?}"),
    }
}
