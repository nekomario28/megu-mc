// SPDX-License-Identifier: LGPL-3.0-or-later

//! Real-bytecode end-to-end tests for `class_mixin_scanner`.
//!
//! Every fixture below is a genuine JVMS §4 `.class` file hand-assembled
//! byte-by-byte (constant pool with dual-slot `Long`/`Double` entries,
//! `Code`/`ConstantValue`/`SourceFile` attributes mixed between the
//! annotation attributes, both `RuntimeVisibleAnnotations` and
//! `RuntimeInvisibleAnnotations`) packed into real ZIP archives — no
//! `javac`, no external JARs, no crates.

use std::collections::{BTreeSet, HashMap};

use mod_adapter_core::class_mixin_scanner::{
    parse_mixin_class, scan_jar_mixins, scan_jar_mixins_with_options, scan_jar_path,
    ClassScanError, JarScanOptions,
};
use mod_adapter_core::fallback::{classify_mod_with_mixins, DiscoveredMod, ExecutionTier};
use mod_adapter_core::mixin_lifter::{
    InjectPoint, MixinAnnotationKind, MixinLiftVerdict, StandardEventHook, UnliftableReason,
};
use mod_adapter_core::version_compat::McVersion;

// ---------------------------------------------------------------------------
// JVM .class file builder
// ---------------------------------------------------------------------------

/// One constant-pool item; `Wide` is the unusable second slot of a
/// `Long`/`Double` entry.
enum CpItem {
    Utf8(Vec<u8>),
    Class(u16),
    Int(i32),
    Long,
    Wide,
}

/// Constant-pool assembler with deduplication; `Long` entries consume two
/// indices so every built class exercises dual-slot skipping.
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
        self.utf8_bytes(s.as_bytes())
    }

    fn utf8_bytes(&mut self, bytes: &[u8]) -> u16 {
        if let Some(&i) = self.utf8.get(bytes) {
            return i;
        }
        let index = self.push(CpItem::Utf8(bytes.to_vec()));
        self.utf8.insert(bytes.to_vec(), index);
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

    fn int(&mut self, v: i32) -> u16 {
        if let Some(&i) = self.ints.get(&v) {
            return i;
        }
        let index = self.push(CpItem::Int(v));
        self.ints.insert(v, index);
        index
    }

    fn int_idx(&self, v: i32) -> u16 {
        self.ints[&v]
    }

    /// Insert an unreferenced `CONSTANT_Long` — two slots, matching the
    /// pool padding a real `javac` class full of constants carries.
    fn pad_long(&mut self) {
        self.push(CpItem::Long);
        self.push(CpItem::Wide);
    }

    /// Number of constant-pool indices (entries + wide placeholders).
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
                CpItem::Long => {
                    out.push(5);
                    out.extend(123_456_789i64.to_be_bytes());
                }
                CpItem::Wide => {}
            }
        }
        out
    }
}

/// One annotation `element_value` for the test builder.
enum EV {
    /// `s` string constant.
    S(String),
    /// `Z` boolean (stored as a `CONSTANT_Integer`).
    Z(bool),
    /// `c` class literal (internal name).
    Cls(String),
    /// `e` enum constant (type descriptor, constant name).
    Enum(String, String),
    /// `@` nested annotation.
    Ann(Box<Ann>),
    /// `[` array.
    Arr(Vec<EV>),
}

/// One `annotation` structure plus the attribute it belongs in.
struct Ann {
    /// Type descriptor, e.g. `Lorg/spongepowered/asm/mixin/injection/Inject;`.
    desc: String,
    elems: Vec<(String, EV)>,
    /// `true` → `RuntimeInvisibleAnnotations` (Mixin's real CLASS
    /// retention), `false` → `RuntimeVisibleAnnotations`.
    invisible: bool,
}

fn ann(desc: &str, elems: Vec<(&str, EV)>, invisible: bool) -> Ann {
    Ann {
        desc: desc.to_string(),
        elems: elems
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        invisible,
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
        EV::Z(b) => {
            out.push(b'Z');
            out.extend(cp.int(i32::from(*b)).to_be_bytes());
        }
        EV::Cls(name) => {
            out.push(b'c');
            out.extend(cp.class(name).to_be_bytes());
        }
        EV::Enum(ty, name) => {
            out.push(b'e');
            out.extend(cp.utf8(ty).to_be_bytes());
            out.extend(cp.utf8(name).to_be_bytes());
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

/// `Runtime{Visible,Invisible}Annotations` attribute payload for a group
/// of same-visibility annotations.
fn anns_payload(cp: &mut Cp, anns: &[&Ann]) -> Vec<u8> {
    let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
    for ann in anns {
        write_ann(&mut payload, cp, ann);
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

/// Assemble a complete `.class` file: public class `internal` extending
/// `java/lang/Object`, one private `int dummy` field carrying a
/// `ConstantValue` attribute (field-attribute skipping), each method a
/// `()V` stub with a real `Code` attribute (non-annotation attribute
/// skipping) plus its annotation attributes, class-level annotations,
/// and a `SourceFile` attribute.
fn build_class(internal: &str, class_anns: Vec<Ann>, methods: Vec<Meth>) -> Vec<u8> {
    let mut cp = Cp::new();
    // Intern every name the body needs, then pad the pool with a dual-slot
    // Long to prove the parser advances two slots.
    cp.class(internal);
    cp.class("java/lang/Object");
    for s in [
        "Code",
        "ConstantValue",
        "SourceFile",
        "RuntimeVisibleAnnotations",
        "RuntimeInvisibleAnnotations",
        "dummy",
        "I",
        "()V",
        "Dummy.java",
    ] {
        cp.utf8(s);
    }
    for m in &methods {
        cp.utf8(&m.name);
    }
    cp.int(7); // ConstantValue payload

    // Encode annotation attribute payloads first (interning as we go) so
    // the pool is complete before it is serialized.
    let mut method_attrs: Vec<Vec<(u16, Vec<u8>)>> = Vec::new();
    for m in &methods {
        let mut attrs = Vec::new();
        // A real Code attribute ahead of the annotations.
        let code = {
            let mut p = Vec::new();
            p.extend(1u16.to_be_bytes()); // max_stack
            p.extend(1u16.to_be_bytes()); // max_locals
            p.extend(1u32.to_be_bytes()); // code_length
            p.push(0xB1); // return
            p.extend(0u16.to_be_bytes()); // exception_table_length
            p.extend(0u16.to_be_bytes()); // attributes_count
            p
        };
        attrs.push((cp.utf8("Code"), code));
        let (invisible, visible): (Vec<&Ann>, Vec<&Ann>) =
            m.anns.iter().partition(|a| a.invisible);
        for (name, group) in [
            ("RuntimeInvisibleAnnotations", invisible),
            ("RuntimeVisibleAnnotations", visible),
        ] {
            if !group.is_empty() {
                attrs.push((cp.utf8(name), anns_payload(&mut cp, &group)));
            }
        }
        method_attrs.push(attrs);
    }
    let (invisible, visible): (Vec<&Ann>, Vec<&Ann>) =
        class_anns.iter().partition(|a| a.invisible);
    let mut class_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
    for (name, group) in [
        ("RuntimeInvisibleAnnotations", invisible),
        ("RuntimeVisibleAnnotations", visible),
    ] {
        if !group.is_empty() {
            class_attrs.push((cp.utf8(name), anns_payload(&mut cp, &group)));
        }
    }
    // SourceFile — one more non-annotation attribute to skip.
    class_attrs.push((cp.utf8("SourceFile"), cp.utf8("Dummy.java").to_be_bytes().to_vec()));
    cp.pad_long();

    // Serialize: header + pool, then the body.
    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // minor
    out.extend(61u16.to_be_bytes()); // major (Java 17)
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes()); // public + super
    out.extend(cp.class_idx(internal).to_be_bytes());
    out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces_count

    out.extend(1u16.to_be_bytes()); // fields_count
    out.extend(0x0002u16.to_be_bytes()); // private
    out.extend(cp.utf8_idx("dummy").to_be_bytes());
    out.extend(cp.utf8_idx("I").to_be_bytes());
    out.extend(1u16.to_be_bytes()); // one attribute
    out.extend(cp.utf8_idx("ConstantValue").to_be_bytes());
    out.extend(2u32.to_be_bytes());
    out.extend(cp.int_idx(7).to_be_bytes());

    out.extend((methods.len() as u16).to_be_bytes());
    for (m, attrs) in methods.iter().zip(&method_attrs) {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(cp.utf8_idx(&m.name).to_be_bytes());
        out.extend(cp.utf8_idx("()V").to_be_bytes());
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

// ---------------------------------------------------------------------------
// ZIP/JAR builder (stored members, real central directory + EOCD)
// ---------------------------------------------------------------------------

use megu_core::hash::crc32_ieee as crc32;

/// Pack `entries` (name → bytes) into a real ZIP archive with stored
/// (uncompressed) members.
fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
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
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra len
        out.extend(name);
        out.extend(*data);

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
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes()); // extra len
        central.extend(0u16.to_le_bytes()); // comment len
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_be_bytes()); // internal attrs
        central.extend(0u32.to_le_bytes()); // external attrs
        central.extend(offset.to_le_bytes());
        central.extend(name);
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

// ---------------------------------------------------------------------------
// Mixin fixture helpers
// ---------------------------------------------------------------------------

const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
const INJECT: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
const AT: &str = "Lorg/spongepowered/asm/mixin/injection/At;";

fn at(value: &str) -> EV {
    EV::Ann(Box::new(ann(AT, vec![("value", s(value))], true)))
}

/// `@Mixin` class annotation (CLASS retention → invisible attribute).
fn mixin(elems: Vec<(&str, EV)>) -> Ann {
    ann(MIXIN, elems, true)
}

fn inject(elems: Vec<(&str, EV)>) -> Ann {
    ann(INJECT, elems, true)
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Spec case 1: a JAR with `safe.mixins.json` registering
/// `@Mixin(LivingEntity.class)` + `@Inject(method = "hurt",
/// at = @At("HEAD"), cancellable = true)` lifts fully and promotes the
/// mod Tier 3 → Tier 2.
#[test]
fn safe_inject_jar_scans_and_promotes_tier3_to_tier2() {
    let config = br#"{
        "required": true,
        "package": "com.safemod.mixin",
        "compatibilityLevel": "JAVA_17",
        "minVersion": "0.8",
        "mixins": ["LivingEntityMixin"],
        "injectors": { "defaultRequire": 1 }
    }"#;
    let class = build_class(
        "com/safemod/mixin/LivingEntityMixin",
        vec![mixin(vec![(
            "value",
            EV::Arr(vec![cls("net/minecraft/world/entity/LivingEntity")]),
        )])],
        vec![meth(
            "safemod$onHurt",
            vec![inject(vec![
                ("method", s("hurt")),
                ("at", at("HEAD")),
                ("cancellable", EV::Z(true)),
            ])],
        )],
    );
    let jar = jar(&[
        ("safe.mixins.json", config.as_slice()),
        ("com/safemod/mixin/LivingEntityMixin.class", class.as_slice()),
        ("com/safemod/Unrelated.class", build_class("com/safemod/Unrelated", vec![], vec![]).as_slice()),
    ]);

    let report = scan_jar_mixins(&jar).expect("safe mixin JAR scans cleanly");
    assert_eq!(report.mixin_configs_found, 1);
    // Only the registered class is parsed — the unregistered class is
    // not part of the applied mixin set.
    assert_eq!(report.classes_scanned, 1);
    assert_eq!(report.sites.len(), 1);
    let site = &report.sites[0];
    assert_eq!(site.mixin_class, "com/safemod/mixin/LivingEntityMixin");
    assert_eq!(site.target_class, "net/minecraft/world/entity/LivingEntity");
    assert_eq!(site.target_method, "hurt");
    assert_eq!(site.annotation, MixinAnnotationKind::Inject);
    assert_eq!(site.at, Some(InjectPoint::Head));
    assert!(site.cancellable);
    assert!(!site.captures_locals);

    let MixinLiftVerdict::FullyLiftable(bindings) = &report.verdict else {
        panic!("expected fully liftable verdict, got {:?}", report.verdict);
    };
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);
    assert!(bindings[0].cancellable);

    // The scanned sites feed straight into the tier classifier: a mod
    // blocked only by mixin reason codes promotes to the bounded JVM shim.
    let (tier, diagnostic, verdict) = classify_mod_with_mixins(
        &mixin_only_mod("safemod"),
        &report.sites,
        &set(&[]),
        &set(&[]),
        McVersion::V1_21_1,
    );
    assert_eq!(tier, ExecutionTier::BoundedJvmShim, "{diagnostic}");
    assert!(diagnostic.contains("EntityDamagePre"), "{diagnostic}");
    assert_eq!(verdict, Some(report.verdict.clone()));
}

/// Spec case 2: Intermediary names — `@Mixin(targets = "...")` dotted
/// string form plus `method = "method_5643"` lifts to the same hook.
#[test]
fn intermediary_targets_string_form_lifts() {
    let config = br#"{
        "package": "com.intermod.mixin",
        "mixins": ["EntityMixin"]
    }"#;
    // Single `s` string (not wrapped in an array) is a legal `targets`.
    let class = build_class(
        "com/intermod/mixin/EntityMixin",
        vec![mixin(vec![("targets", s("net.minecraft.class_1297"))])],
        vec![meth(
            "intermod$damage",
            vec![inject(vec![
                ("method", EV::Arr(vec![s("method_5643")])),
                ("at", EV::Arr(vec![at("HEAD")])),
            ])],
        )],
    );
    let jar = jar(&[
        ("intermod.mixins.json", config.as_slice()),
        ("com/intermod/mixin/EntityMixin.class", class.as_slice()),
    ]);

    let report = scan_jar_mixins(&jar).expect("intermediary JAR scans cleanly");
    assert_eq!(report.sites.len(), 1);
    assert_eq!(report.sites[0].target_class, "net.minecraft.class_1297");
    assert_eq!(report.sites[0].target_method, "method_5643");
    match &report.verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => {
            assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);
        }
        other => panic!("expected fully liftable verdict, got {other:?}"),
    }
}

/// Spec case 3: `@Redirect`, `@Overwrite`, `LocalCapture.CAPTURE_FAILHARD`,
/// an `@At("INVOKE")` point, an unknown mixin annotation, and a deep
/// physics target each fail closed — and together keep the whole JAR at
/// Tier 3.
#[test]
fn unliftable_mixins_fail_closed() {
    let target_le = cls("net/minecraft/world/entity/LivingEntity");
    let mixin_targets = mixin(vec![("value", EV::Arr(vec![target_le]))]);
    let methods = vec![
        meth(
            "bad$redirect",
            vec![ann(
                "Lorg/spongepowered/asm/mixin/injection/Redirect;",
                vec![
                    ("method", s("hurt")),
                    (
                        "at",
                        EV::Ann(Box::new(ann(
                            AT,
                            vec![
                                ("value", s("INVOKE")),
                                ("target", s("Lnet/minecraft/x;->y()V")),
                            ],
                            true,
                        ))),
                    ),
                ],
                true,
            )],
        ),
        meth(
            "bad$overwrite",
            vec![ann("Lorg/spongepowered/asm/mixin/Overwrite;", vec![], true)],
        ),
        meth(
            "bad$locals",
            vec![inject(vec![
                ("method", s("hurt")),
                ("at", at("HEAD")),
                (
                    "locals",
                    EV::Enum(
                        "Lorg/spongepowered/asm/mixin/injection/callback/LocalCapture;".into(),
                        "CAPTURE_FAILHARD".into(),
                    ),
                ),
            ])],
        ),
        meth(
            "bad$invokePoint",
            vec![inject(vec![
                ("method", s("hurt")),
                (
                    "at",
                    EV::Ann(Box::new(ann(
                        AT,
                        vec![
                            ("value", s("INVOKE")),
                            ("target", s("Lnet/minecraft/x;->y()V")),
                        ],
                        true,
                    ))),
                ),
            ])],
        ),
        meth(
            "bad$shadow",
            vec![ann(
                "Lorg/spongepowered/asm/mixin/Shadow;",
                vec![("method", s("hurt")), ("at", at("HEAD"))],
                true,
            )],
        ),
    ];
    // A @Mixin targeting Valkyrien Skies 2-style physics internals.
    let physics = build_class(
        "com/badmod/mixin/VSPhysicsMixin",
        vec![mixin(vec![(
            "targets",
            EV::Arr(vec![s("org.valkyrienskies.core.impl.shadow.VSPhysics")]),
        )])],
        vec![meth(
            "bad$physics",
            vec![inject(vec![("method", s("tick")), ("at", at("HEAD"))])],
        )],
    );
    let config = br#"{
        "package": "com.badmod.mixin",
        "mixins": ["BadMixin", "VSPhysicsMixin"]
    }"#;
    let jar = jar(&[
        ("bad.mixins.json", config.as_slice()),
        (
            "com/badmod/mixin/BadMixin.class",
            build_class("com/badmod/mixin/BadMixin", vec![mixin_targets], methods).as_slice(),
        ),
        ("com/badmod/mixin/VSPhysicsMixin.class", physics.as_slice()),
    ]);

    let report = scan_jar_mixins(&jar).expect("bad-mixin JAR still scans");
    let MixinLiftVerdict::RequiresFallback { lifted, unliftable } = &report.verdict else {
        panic!("expected RequiresFallback, got {:?}", report.verdict);
    };
    assert!(lifted.is_empty());
    let has_reason = |want: UnliftableReason| {
        unliftable.iter().any(|(_, r)| *r == want)
    };
    assert!(has_reason(UnliftableReason::UnsupportedAnnotation), "{unliftable:?}");
    assert!(has_reason(UnliftableReason::CapturesLocals), "{unliftable:?}");
    assert!(
        has_reason(UnliftableReason::UnsupportedInjectPoint),
        "{unliftable:?}"
    );
    assert!(has_reason(UnliftableReason::UnknownTarget), "{unliftable:?}");
    // @Shadow is a non-mutating declaration since Lane
    // MOD-TIER2-SINYTRA-FREE: it is recorded as a declaration, never a
    // site — so the `hurt`-method unliftable sites found here are the
    // @Redirect/@locals ones, which keep UnsupportedAnnotation covered.
    assert_eq!(
        report.declarations_skipped, 1,
        "the @Shadow member is a recorded declaration, not a blocker"
    );
    assert_eq!(report.declarations.len(), 1);
    assert_eq!(
        report.declarations[0].annotation,
        MixinAnnotationKind::Shadow
    );
    assert_eq!(report.declarations[0].member_name, "bad$shadow");
    let hurt_blocker = unliftable
        .iter()
        .find(|(site, _)| site.target_method == "hurt" && site.mixin_class.ends_with("BadMixin"))
        .expect("unliftable hurt site recorded");
    assert!(matches!(
        hurt_blocker.0.annotation,
        MixinAnnotationKind::Redirect | MixinAnnotationKind::Inject
    ));

    // The classifier keeps the mod in Tier 3.
    let (tier, diagnostic, _) = classify_mod_with_mixins(
        &mixin_only_mod("badmod"),
        &report.sites,
        &set(&[]),
        &set(&[]),
        McVersion::V1_21_1,
    );
    assert_eq!(tier, ExecutionTier::RequiresFallback);
    assert!(diagnostic.contains("unliftable"), "{diagnostic}");
}

/// Spec case 4: corrupt and truncated inputs fail closed without panic.
#[test]
fn corrupt_and_truncated_inputs_fail_closed() {
    // Not a class at all / empty input.
    assert!(matches!(
        parse_mixin_class(&[]),
        Err(ClassScanError::Truncated(_))
    ));
    assert_eq!(
        parse_mixin_class(b"\xDE\xAD\xBE\xEFxxxxxx"),
        Err(ClassScanError::BadMagic)
    );

    // Every strict prefix of a valid class is an error, never a panic.
    let valid = build_class(
        "com/trunc/M",
        vec![mixin(vec![("targets", s("net.minecraft.class_1297"))])],
        vec![meth("h", vec![inject(vec![("at", at("TAIL"))])])],
    );
    for cut in 1..valid.len() {
        assert!(
            parse_mixin_class(&valid[..cut]).is_err(),
            "prefix {cut} must fail closed"
        );
    }
    assert!(parse_mixin_class(&valid).is_ok());

    // Unknown constant-pool tag.
    let mut bad_tag = Vec::new();
    bad_tag.extend(0xCAFE_BABEu32.to_be_bytes());
    bad_tag.extend(0u16.to_be_bytes());
    bad_tag.extend(61u16.to_be_bytes());
    bad_tag.extend(2u16.to_be_bytes()); // cp_count = 2
    bad_tag.push(99); // bogus tag
    assert_eq!(
        parse_mixin_class(&bad_tag),
        Err(ClassScanError::UnsupportedConstantPoolTag(99))
    );

    // this_class pointing at a Utf8 slot (tag check) and past the pool
    // (index check), assembled by hand.
    for (this_index, expect_index_err) in [(3u16, false), (42u16, true)] {
        let mut b = Vec::new();
        b.extend(0xCAFE_BABEu32.to_be_bytes());
        b.extend(0u16.to_be_bytes());
        b.extend(61u16.to_be_bytes());
        b.extend(6u16.to_be_bytes()); // cp_count = 6
        for item in [
            CpItem::Utf8(b"T".to_vec()),
            CpItem::Class(1),
            CpItem::Utf8(b"java/lang/Object".to_vec()),
            CpItem::Class(3),
            CpItem::Int(0),
        ] {
            match item {
                CpItem::Utf8(bytes) => {
                    b.push(1);
                    b.extend((bytes.len() as u16).to_be_bytes());
                    b.extend(bytes);
                }
                CpItem::Class(i) => {
                    b.push(7);
                    b.extend(i.to_be_bytes());
                }
                CpItem::Int(v) => {
                    b.push(3);
                    b.extend(v.to_be_bytes());
                }
                _ => unreachable!(),
            }
        }
        b.extend(0x0021u16.to_be_bytes());
        b.extend(this_index.to_be_bytes());
        b.extend(4u16.to_be_bytes()); // super
        b.extend(0u16.to_be_bytes()); // interfaces
        b.extend(0u16.to_be_bytes()); // fields
        b.extend(0u16.to_be_bytes()); // methods
        b.extend(0u16.to_be_bytes()); // attributes
        let err = parse_mixin_class(&b).unwrap_err();
        if expect_index_err {
            assert!(matches!(err, ClassScanError::BadConstantPoolIndex(_)), "{err:?}");
        } else {
            assert!(matches!(err, ClassScanError::BadConstantPoolTag(_)), "{err:?}");
        }
    }

    // Archive-level corruption: no EOCD, truncated ZIP.
    assert!(matches!(
        scan_jar_mixins(b"not a jar"),
        Err(ClassScanError::Zip(_))
    ));
    let valid_jar = jar(&[("x.mixins.json", b"{}".as_slice())]);
    for cut in 1..valid_jar.len() {
        assert!(
            scan_jar_mixins(&valid_jar[..cut]).is_err(),
            "jar prefix {cut} must fail closed"
        );
    }
    assert!(scan_jar_mixins(&valid_jar).is_ok());
}

/// `client`-listed classes never apply on a dedicated server: they are
/// skipped (even when the class itself is unliftable or absent), and a
/// non-server scan still sees them.
#[test]
fn client_only_mixins_skipped_on_server_scan() {
    let config = br#"{
        "package": "com.sidemod.mixin",
        "mixins": ["SharedMixin"],
        "server": ["ServerMixin"],
        "client": ["ClientOnlyMixin"]
    }"#;
    fn le_mixin() -> Ann {
        mixin(vec![(
            "value",
            EV::Arr(vec![cls("net/minecraft/world/entity/LivingEntity")]),
        )])
    }
    fn good(internal: &str) -> Vec<u8> {
        build_class(
            internal,
            vec![le_mixin()],
            vec![meth(
                "onHurt",
                vec![inject(vec![("method", s("hurt")), ("at", at("HEAD"))])],
            )],
        )
    }
    // The client mixin class is deliberately unliftable (@Redirect) AND
    // its bytes are absent from the archive — a server scan must not
    // even look for them.
    let jar = jar(&[
        ("side.mixins.json", config.as_slice()),
        (
            "com/sidemod/mixin/SharedMixin.class",
            good("com/sidemod/mixin/SharedMixin").as_slice(),
        ),
        (
            "com/sidemod/mixin/ServerMixin.class",
            good("com/sidemod/mixin/ServerMixin").as_slice(),
        ),
    ]);

    let report = scan_jar_mixins(&jar).expect("server-side scan skips client mixins");
    assert_eq!(report.mixin_configs_found, 1);
    assert_eq!(report.classes_scanned, 2);
    assert_eq!(report.skipped_client_mixins, 1);
    assert_eq!(report.sites.len(), 2);
    assert!(matches!(
        report.verdict,
        MixinLiftVerdict::FullyLiftable(_)
    ));

    // A whole-archive scan (server_only = false) treats the missing
    // client mixin class as unscannable — fail closed.
    let err = scan_jar_mixins_with_options(&jar, JarScanOptions { server_only: false })
        .unwrap_err();
    assert!(
        matches!(err, ClassScanError::MissingMixinClass(ref n) if n.contains("ClientOnlyMixin")),
        "{err:?}"
    );
}

/// With no `*.mixins.json` at all the scanner falls back to parsing every
/// `.class` member defensively.
#[test]
fn jar_without_mixin_config_scans_all_classes() {
    let mixin_class = build_class(
        "com/bare/mixin/JoinMixin",
        vec![mixin(vec![(
            "value",
            EV::Arr(vec![cls("net/minecraft/server/players/PlayerList")]),
        )])],
        vec![meth(
            "onJoin",
            vec![inject(vec![
                ("method", s("placeNewPlayer")),
                ("at", at("HEAD")),
            ])],
        )],
    );
    let jar = jar(&[
        ("com/bare/mixin/JoinMixin.class", mixin_class.as_slice()),
        (
            "com/bare/Plain.class",
            build_class("com/bare/Plain", vec![], vec![meth("noop", vec![])]).as_slice(),
        ),
        ("assets/bare/icon.png", b"\x89PNG".as_slice()),
    ]);

    let report = scan_jar_mixins(&jar).expect("config-free JAR scans classes");
    assert_eq!(report.mixin_configs_found, 0);
    assert_eq!(report.classes_scanned, 2);
    assert_eq!(report.sites.len(), 1);
    match &report.verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => {
            assert_eq!(bindings[0].hook, StandardEventHook::PlayerJoin);
        }
        other => panic!("expected fully liftable verdict, got {other:?}"),
    }
}

/// Mixin annotations are honored in `RuntimeVisibleAnnotations` exactly
/// as in the `RuntimeInvisibleAnnotations` `javac` emits for CLASS
/// retention.
#[test]
fn visible_and_invisible_annotation_attributes_both_read() {
    let visible_inject = ann(
        INJECT,
        vec![("method", s("hurt")), ("at", at("HEAD"))],
        false,
    );
    let invisible_inject = ann(
        INJECT,
        vec![("method", s("tick")), ("at", at("TAIL"))],
        true,
    );
    let class = build_class(
        "com/vis/M",
        vec![ann(
            MIXIN,
            vec![(
                "targets",
                EV::Arr(vec![s("net.minecraft.server.level.ServerLevel")]),
            )],
            false,
        )],
        vec![
            meth("v", vec![visible_inject]),
            meth("i", vec![invisible_inject]),
        ],
    );
    let sites = parse_mixin_class(&class).expect("class parses");
    assert_eq!(sites.len(), 2);
    let methods: BTreeSet<&str> = sites.iter().map(|s| s.target_method.as_str()).collect();
    assert_eq!(methods, set(&["hurt", "tick"]).iter().map(String::as_str).collect());
}

/// One site is produced per `(target_class, method, at)` combination.
#[test]
fn target_method_at_cross_product() {
    let class = build_class(
        "com/x/M",
        vec![mixin(vec![
            (
                "value",
                EV::Arr(vec![
                    cls("net/minecraft/world/entity/LivingEntity"),
                    cls("net/minecraft/class_1309"),
                ]),
            ),
            ("targets", EV::Arr(vec![s("net.minecraft.class_1297")])),
        ])],
        vec![meth(
            "handler",
            vec![inject(vec![
                ("method", EV::Arr(vec![s("hurt"), s("method_5643")])),
                ("at", EV::Arr(vec![at("HEAD"), at("TAIL")])),
            ])],
        )],
    );
    let sites = parse_mixin_class(&class).expect("class parses");
    assert_eq!(sites.len(), 3 * 2 * 2, "{sites:?}");
    let combos: BTreeSet<(&str, &str, &str)> = sites
        .iter()
        .map(|s| {
            (
                s.target_class.as_str(),
                s.target_method.as_str(),
                s.at.as_ref().unwrap().as_str(),
            )
        })
        .collect();
    assert_eq!(combos.len(), 12);
    // Handler-name defaulting: an @Inject with no `method` element uses
    // the handler's own name, matching Mixin's rule.
    let defaulted = build_class(
        "com/x/N",
        vec![mixin(vec![("targets", s("net.minecraft.class_1297"))])],
        vec![meth("method_5643", vec![inject(vec![("at", at("HEAD"))])])],
    );
    let sites = parse_mixin_class(&defaulted).expect("class parses");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].target_method, "method_5643");
    match lift_via(&sites) {
        MixinLiftVerdict::FullyLiftable(bindings) => {
            assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);
        }
        other => panic!("expected lift, got {other:?}"),
    }
}

fn lift_via(sites: &[mod_adapter_core::mixin_lifter::MixinSite]) -> MixinLiftVerdict {
    mod_adapter_core::mixin_lifter::lift_mixin_sites(sites)
}

/// A config-registered class absent from the archive is unscannable:
/// fail closed rather than pretend the mixin set is complete.
#[test]
fn config_registered_class_missing_fails_closed() {
    let config = br#"{"package": "com.ghost.mixin", "mixins": ["Ghost"]}"#;
    let jar_bytes = jar(&[("ghost.mixins.json", config.as_slice())]);
    let err = scan_jar_mixins(&jar_bytes).unwrap_err();
    assert!(
        matches!(err, ClassScanError::MissingMixinClass(ref n) if n == "com/ghost/mixin/Ghost.class"),
        "{err:?}"
    );

    // A mixin config that is not JSON fails closed too.
    let bad = jar(&[("broken.mixins.json", b"not json".as_slice())]);
    assert!(matches!(
        scan_jar_mixins(&bad),
        Err(ClassScanError::Json(_))
    ));
}

/// A class carrying mixin *method* annotations but no `@Mixin` still
/// emits sites — with an empty target the lifter rejects.
#[test]
fn method_annotations_without_mixin_fail_closed() {
    let class = build_class(
        "com/odd/NotAMixin",
        vec![],
        vec![meth(
            "onHurt",
            vec![inject(vec![("method", s("hurt")), ("at", at("HEAD"))])],
        )],
    );
    let sites = parse_mixin_class(&class).expect("class parses");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].target_class, "");
    match lift_via(&sites) {
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
            assert_eq!(unliftable[0].1, UnliftableReason::UnknownTarget);
        }
        other => panic!("expected RequiresFallback, got {other:?}"),
    }

    // A plain class with no mixin annotations yields nothing.
    let plain = build_class("com/odd/Plain", vec![], vec![meth("m", vec![])]);
    assert_eq!(parse_mixin_class(&plain).unwrap(), vec![]);
}

/// `scan_jar_path` reads a JAR straight from disk.
#[test]
fn scan_jar_path_reads_from_disk() {
    let class = build_class(
        "com/path/mixin/P",
        vec![mixin(vec![(
            "value",
            EV::Arr(vec![cls("net/minecraft/server/level/ServerLevel")]),
        )])],
        vec![meth(
            "onTick",
            vec![inject(vec![("method", s("tick")), ("at", at("TAIL"))])],
        )],
    );
    let jar = jar(&[
        ("path.mixins.json", br#"{"package":"com.path.mixin","mixins":["P"]}"#.as_slice()),
        ("com/path/mixin/P.class", class.as_slice()),
    ]);
    let path = std::env::temp_dir().join(format!("mixin_jar_scan_{}.jar", std::process::id()));
    std::fs::write(&path, &jar).expect("write temp jar");
    let report = scan_jar_path(&path);
    let _ = std::fs::remove_file(&path);
    let report = report.expect("jar on disk scans");
    assert_eq!(report.sites.len(), 1);
    match &report.verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => {
            assert_eq!(bindings[0].hook, StandardEventHook::ServerTickPost);
        }
        other => panic!("expected fully liftable verdict, got {other:?}"),
    }
}

/// Exotic-but-legal modified UTF-8 in the constant pool decodes; broken
/// sequences are a hard error. Wide (dual-slot) pool entries are skipped
/// correctly throughout every fixture.
#[test]
fn modified_utf8_edge_cases() {
    // A CONSTANT_Utf8 containing a NUL encoded as 0xC0 0x80 and a
    // supplementary character encoded as a surrogate pair must decode.
    let mut cp = Cp::new();
    let mut weird = Vec::new();
    weird.extend(b"name");
    weird.extend([0xC0, 0x80]); // NUL in MUTF-8
    weird.extend([0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]); // U+1F600 surrogate pair
    cp.utf8_bytes(&weird);
    let class = {
        let mut cp = cp;
        cp.class("com/utf/M");
        cp.class("java/lang/Object");
        cp.utf8("SourceFile");
        cp.utf8("M.java");
        let mut out = Vec::new();
        out.extend(0xCAFE_BABEu32.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        out.extend(61u16.to_be_bytes());
        out.extend(cp.count().to_be_bytes());
        out.extend(cp.encode());
        out.extend(0x0021u16.to_be_bytes());
        out.extend(cp.class_idx("com/utf/M").to_be_bytes());
        out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
        out.extend([0u8; 8]); // interfaces/fields/methods/attrs
        out
    };
    assert_eq!(parse_mixin_class(&class).unwrap(), vec![]);

    // Unpaired surrogate and truncated lead bytes reject.
    for bad in [
        vec![0xEDu8, 0xA0, 0xBD],        // lone high surrogate
        vec![0xEDu8, 0xB8, 0x80],        // lone low surrogate
        vec![b'a', 0xC0],                // truncated 2-byte
        vec![b'a', 0xE0, 0x80],          // truncated 3-byte
        vec![b'a', 0xF0, 0x80, 0x80, 0x80], // 4-byte form never legal in MUTF-8
        vec![0x00],                      // bare NUL never legal
    ] {
        let mut cp = Cp::new();
        cp.utf8_bytes(&bad);
        cp.class("com/utf/Bad");
        cp.class("java/lang/Object");
        let mut out = Vec::new();
        out.extend(0xCAFE_BABEu32.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        out.extend(61u16.to_be_bytes());
        out.extend(cp.count().to_be_bytes());
        out.extend(cp.encode());
        out.extend(0x0021u16.to_be_bytes());
        out.extend(cp.class_idx("com/utf/Bad").to_be_bytes());
        out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
        out.extend([0u8; 8]);
        assert_eq!(
            parse_mixin_class(&out),
            Err(ClassScanError::InvalidUtf8),
            "bad mutf8 {bad:?} must fail closed"
        );
    }
}

/// Annotation nesting past the depth cap and an unknown `element_value`
/// tag fail closed.
#[test]
fn hostile_annotation_shapes_fail_closed() {
    // 20 nested `@` annotations inside `at`.
    fn deep(n: usize) -> EV {
        if n == 0 {
            s("HEAD")
        } else {
            EV::Ann(Box::new(ann(AT, vec![("value", deep(n - 1))], true)))
        }
    }
    let class = build_class(
        "com/deep/M",
        vec![mixin(vec![("targets", s("net.minecraft.class_1297"))])],
        vec![meth(
            "m",
            vec![inject(vec![("method", s("method_5643")), ("at", deep(20))])],
        )],
    );
    assert_eq!(
        parse_mixin_class(&class),
        Err(ClassScanError::AnnotationNestingTooDeep)
    );

    // An unknown element_value tag byte inside @Inject.
    let mut cp = Cp::new();
    cp.class("com/tag/M");
    cp.class("java/lang/Object");
    for s_ in ["RuntimeInvisibleAnnotations", "m", "()V", INJECT, "method", "hurt"] {
        cp.utf8(s_);
    }
    let mut payload = 1u16.to_be_bytes().to_vec(); // one annotation
    payload.extend(cp.utf8_idx(INJECT).to_be_bytes());
    payload.extend(1u16.to_be_bytes()); // one element
    payload.extend(cp.utf8_idx("method").to_be_bytes());
    payload.push(b'?'); // bogus element_value tag
    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(61u16.to_be_bytes());
    out.extend(cp.count().to_be_bytes());
    out.extend(cp.encode());
    out.extend(0x0021u16.to_be_bytes());
    out.extend(cp.class_idx("com/tag/M").to_be_bytes());
    out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces
    out.extend(0u16.to_be_bytes()); // fields
    out.extend(1u16.to_be_bytes()); // one method
    out.extend(0x0001u16.to_be_bytes());
    out.extend(cp.utf8_idx("m").to_be_bytes());
    out.extend(cp.utf8_idx("()V").to_be_bytes());
    out.extend(1u16.to_be_bytes()); // one attribute
    out.extend(cp.utf8_idx("RuntimeInvisibleAnnotations").to_be_bytes());
    out.extend((payload.len() as u32).to_be_bytes());
    out.extend(payload);
    out.extend(0u16.to_be_bytes()); // class attrs
    assert_eq!(
        parse_mixin_class(&out),
        Err(ClassScanError::Malformed("unknown element_value tag"))
    );
}
