// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane MOD-LIFT-UNIVERSAL-5 regression tests: five generic, clean-room
//! mixin-lifter upgrades measured on real production JARs (create,
//! Farmer's Delight, waystones, balm, appleskin).
//!
//! 1. `net/minecraft/gametest/*` (yarn `net/minecraft/test/*`) targets
//!    classify `GameTestOnly` — GameTest harness code, recorded
//!    `gametest_skipped`, never a live-gameplay Tier-3 blocker.
//! 2. The verified read-only `@Accessor` table grew the plain stored
//!    `SystemReport`/`StateHolder`/`FluidInteractionRegistry`/
//!    `ItemStackHandler` fields, with constant-style remainder
//!    inference (`getJAVA_VERSION` → `JAVA_VERSION`) — `set*` handlers
//!    still fail closed as `MutatingSetter`.
//! 3. Strictly client-visual-only vanilla methods (`ArmorTrim`
//!    `innerTexture`/`outerTexture`, `LivingEntity` item-use
//!    particles) skip with `client_cosmetic_skipped` telemetry on a
//!    dedicated-server scan only.
//! 4. `cancellable = true` at `TAIL`/`RETURN` on a verified
//!    value-returning target lifts as `ReturnValueOverride`
//!    (`Item#useOn`, `MinecraftServer#reloadResources`, ...); on a void
//!    target it still fails closed `NonCancellablePostHook`.
//! 5. `EnchantedCountIncreaseFunction#run` lifts `LootBonusRoll` and
//!    `SinglePoolElement#place` lifts `StructurePiecePlace`.
//!
//! All while mid-method `@At(INVOKE)`, mutating accessors, constructor
//! invokers, `@Mutable`, and unknown targets stay strictly fail-closed.

use mod_adapter_core::class_mixin_scanner::{scan_jar_mixins_with_options, JarScanOptions};
use mod_adapter_core::mixin_lifter::{
    classify_mixin_target, is_client_cosmetic_target, lift_accessor_sites,
    lift_invoker_sites, lift_mixin_sites, AccessorRejection, InjectPoint, InvokerRejection,
    LiftedBinding, LiftedHookKind, MixinAnnotationKind, MixinLiftVerdict, MixinSite,
    MixinTargetScope, StandardEventHook, UnliftableReason,
};

fn inject_site(target_class: &str, target_method: &str, at: InjectPoint) -> MixinSite {
    MixinSite {
        mixin_class: "com/example/mymod/mixin/StandardMixin".into(),
        target_class: target_class.into(),
        target_method: target_method.into(),
        annotation: MixinAnnotationKind::Inject,
        at: Some(at),
        cancellable: false,
        captures_locals: false,
        handler_name: "handler".into(),
        handler_descriptor: "()V".into(),
    }
}

fn member_site(
    target_class: &str,
    target_method: &str,
    annotation: MixinAnnotationKind,
    handler_name: &str,
    handler_descriptor: &str,
) -> MixinSite {
    MixinSite {
        mixin_class: "com/example/mymod/mixin/MemberAccessMixin".into(),
        target_class: target_class.into(),
        target_method: target_method.into(),
        annotation,
        at: None,
        cancellable: false,
        captures_locals: false,
        handler_name: handler_name.into(),
        handler_descriptor: handler_descriptor.into(),
    }
}

fn only_binding(verdict: &MixinLiftVerdict) -> &LiftedBinding {
    match verdict {
        MixinLiftVerdict::FullyLiftable(bindings) if bindings.len() == 1 => &bindings[0],
        other => panic!("expected exactly one lifted binding, got {other:?}"),
    }
}

fn unliftable_reasons(verdict: &MixinLiftVerdict) -> Vec<UnliftableReason> {
    match verdict {
        MixinLiftVerdict::FullyLiftable(_) => Vec::new(),
        MixinLiftVerdict::RequiresFallback { unliftable, .. } => {
            unliftable.iter().map(|(_, r)| r.clone()).collect()
        }
    }
}

#[test]
fn gametest_client_cosmetic_readonly_accessors_and_return_value_override_lift() {
    // 1. GameTest harness targets classify non-blocking — both Mojmap
    //    and yarn dialects, dot or slash form.
    for class in [
        "net/minecraft/gametest/framework/GameTestHelper",
        "net/minecraft/gametest/framework/GameTestSequence",
        "net/minecraft/test/GameTestHelper",
        "net.minecraft.gametest.framework.GameTestHelper",
        "Lnet/minecraft/gametest/framework/GameTestHelper;",
    ] {
        assert_eq!(
            classify_mixin_target(class),
            MixinTargetScope::GameTestOnly,
            "{class}"
        );
    }
    for class in [
        "net/minecraft/world/entity/Entity",
        "net/minecraft/util/datafix/schemas/V1460",
        "com/example/mymod/gametest/MyHarness",
    ] {
        assert_ne!(
            classify_mixin_target(class),
            MixinTargetScope::GameTestOnly,
            "{class}"
        );
    }
    assert_eq!(
        classify_mixin_target("net/minecraft/util/datafix/schemas/V1460"),
        MixinTargetScope::LegacyDataFixOnly
    );

    // 3. Verified client-cosmetic (class, method) pairs — and nothing
    //    else (fail-closed by construction).
    for (class, method) in [
        ("net/minecraft/world/item/armortrim/ArmorTrim", "innerTexture"),
        ("net/minecraft/world/item/armortrim/ArmorTrim", "outerTexture"),
        ("net/minecraft/class_8053", "method_57411"),
        (
            "net/minecraft/world/entity/LivingEntity",
            "shouldTriggerItemUseEffects",
        ),
        (
            "net/minecraft/world/entity/LivingEntity",
            "triggerItemUseEffects",
        ),
        (
            "net/minecraft/world/entity/LivingEntity",
            "triggerItemUseEffects(Lnet/minecraft/world/item/ItemStack;I)V",
        ),
        (
            "net/minecraft/world/entity/LivingEntity",
            "spawnItemParticles",
        ),
        ("net/minecraft/class_1309", "method_6037"),
    ] {
        assert!(is_client_cosmetic_target(class, method), "{class}#{method}");
    }
    for (class, method) in [
        ("net/minecraft/world/entity/LivingEntity", "hurt"),
        ("net/minecraft/world/entity/LivingEntity", "travel"),
        ("net/minecraft/world/entity/Entity", "spawnItemParticles"),
        ("net/minecraft/world/item/armortrim/ArmorTrim", "withTooltip"),
        ("com/example/mymod/ArmorTrim", "innerTexture"),
    ] {
        assert!(!is_client_cosmetic_target(class, method), "{class}#{method}");
    }

    // 2. The widened read-only accessor table — including
    //    constant-style verbatim inference (`getJAVA_VERSION` →
    //    `JAVA_VERSION`) — lifts every getter, and nothing else.
    let accessor_plan = lift_accessor_sites(&[
        member_site(
            "net/minecraft/SystemReport",
            "getOPERATING_SYSTEM",
            MixinAnnotationKind::Accessor,
            "getOPERATING_SYSTEM",
            "()Ljava/lang/String;",
        ),
        member_site(
            "net/minecraft/SystemReport",
            "getJAVA_VERSION",
            MixinAnnotationKind::Accessor,
            "getJAVA_VERSION",
            "()Ljava/lang/String;",
        ),
        member_site(
            "net/minecraft/SystemReport",
            "getEntries",
            MixinAnnotationKind::Accessor,
            "getEntries",
            "()Ljava/util/List;",
        ),
        member_site(
            "net/minecraft/world/level/block/state/StateHolder",
            "getOwner",
            MixinAnnotationKind::Accessor,
            "getOwner",
            "()Ljava/lang/Object;",
        ),
        member_site(
            "net/neoforged/neoforge/fluids/FluidInteractionRegistry",
            "getINTERACTIONS",
            MixinAnnotationKind::Accessor,
            "getINTERACTIONS",
            "()Ljava/util/List;",
        ),
        member_site(
            "net/neoforged/neoforge/items/ItemStackHandler",
            "getStacks",
            MixinAnnotationKind::Accessor,
            "getStacks",
            "()Lnet/neoforged/neoforge/items/IItemHandlerModifiable;",
        ),
    ]);
    assert!(
        accessor_plan.is_fully_lifted(),
        "{:?}",
        accessor_plan.rejected
    );
    let fields: Vec<&str> = accessor_plan
        .bindings
        .iter()
        .map(|b| b.field.as_str())
        .collect();
    assert_eq!(
        fields,
        [
            "SystemReport#OPERATING_SYSTEM",
            "SystemReport#JAVA_VERSION",
            "SystemReport#entries",
            "StateHolder#owner",
            "FluidInteractionRegistry#INTERACTIONS",
            "ItemStackHandler#stacks",
        ]
    );

    // 4. `cancellable = true` at `TAIL`/`RETURN` on a verified
    //    value-returning target lifts as `ReturnValueOverride`.
    for (point, class, method, hook) in [
        (
            InjectPoint::Tail,
            "net/minecraft/world/item/Item",
            "useOn",
            StandardEventHook::ItemUseOnBlock,
        ),
        (
            InjectPoint::Return,
            "net/minecraft/server/MinecraftServer",
            "reloadResources",
            StandardEventHook::ResourceReload,
        ),
        (
            InjectPoint::Tail,
            "net/minecraft/server/ReloadableServerResources",
            "loadResources",
            StandardEventHook::ResourceReload,
        ),
        (
            InjectPoint::Return,
            "net/minecraft/world/entity/player/Player",
            "canEat",
            StandardEventHook::PlayerCanEatQuery,
        ),
    ] {
        let mut site = inject_site(class, method, point.clone());
        site.cancellable = true;
        let verdict = lift_mixin_sites(&[site]);
        let binding = only_binding(&verdict);
        assert_eq!(binding.hook, hook, "{class}#{method} @ {point:?}");
        assert_eq!(binding.kind, LiftedHookKind::ReturnValueOverride);
        assert!(binding.cancellable);
    }

    // A non-cancellable post site on the same value-returning target is
    // a plain observation binding.
    let verdict = lift_mixin_sites(&[inject_site(
        "net/minecraft/world/item/Item",
        "useOn",
        InjectPoint::Tail,
    )]);
    let plain = only_binding(&verdict);
    assert_eq!(plain.kind, LiftedHookKind::Standard);
    assert!(!plain.cancellable);

    // 5. The two new universal hooks.
    let verdict = lift_mixin_sites(&[inject_site(
        "net/minecraft/world/level/storage/loot/functions/EnchantedCountIncreaseFunction",
        "run",
        InjectPoint::Tail,
    )]);
    let loot = only_binding(&verdict);
    assert_eq!(loot.hook, StandardEventHook::LootBonusRoll);
    assert_eq!(loot.kind, LiftedHookKind::Standard);
    let verdict = lift_mixin_sites(&[inject_site(
        "net/minecraft/world/level/storage/loot/functions/EnchantedCountIncreaseFunction",
        "run(Lnet/minecraft/world/item/ItemStack;Lnet/minecraft/world/level/storage/loot/LootContext;)Lnet/minecraft/world/item/ItemStack;",
        InjectPoint::Return,
    )]);
    assert_eq!(only_binding(&verdict).hook, StandardEventHook::LootBonusRoll);
    let verdict = lift_mixin_sites(&[inject_site(
        "net/minecraft/world/level/levelgen/structure/pools/SinglePoolElement",
        "place(Lnet/minecraft/world/level/levelgen/structure/templatesystem/StructureTemplateManager;Lnet/minecraft/world/level/WorldGenLevel;Lnet/minecraft/world/level/StructureManager;Lnet/minecraft/world/level/chunk/ChunkGenerator;Lnet/minecraft/core/BlockPos;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/Rotation;Lnet/minecraft/world/level/levelgen/structure/BoundingBox;Lnet/minecraft/util/RandomSource;Lnet/minecraft/world/level/levelgen/structure/templatesystem/LiquidSettings;Z)Z",
        InjectPoint::Head,
    )]);
    assert_eq!(
        only_binding(&verdict).hook,
        StandardEventHook::StructurePiecePlace
    );

    // Fail-closed pins: a cancellable post hook on a *void* target stays
    // `NonCancellablePostHook`; mutating accessors, constructor
    // invokers, and mid-method `@At(INVOKE)` stay Tier 3.
    let mut void_post = inject_site(
        "net/minecraft/world/entity/Entity",
        "tick",
        InjectPoint::Tail,
    );
    void_post.cancellable = true;
    assert_eq!(
        unliftable_reasons(&lift_mixin_sites(&[void_post])),
        [UnliftableReason::NonCancellablePostHook]
    );
    let mut void_post_head_equiv = inject_site(
        "net/minecraft/server/level/ServerLevel",
        "tick",
        InjectPoint::Return,
    );
    void_post_head_equiv.cancellable = true;
    assert_eq!(
        unliftable_reasons(&lift_mixin_sites(&[void_post_head_equiv])),
        [UnliftableReason::NonCancellablePostHook]
    );

    let fuel_setter = lift_accessor_sites(&[member_site(
        "net/minecraft/world/entity/vehicle/MinecartFurnace",
        "fuel",
        MixinAnnotationKind::Accessor,
        "setFuel",
        "(I)V",
    )]);
    assert_eq!(
        fuel_setter.rejected.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
        [AccessorRejection::MutatingSetter]
    );

    let ctor_invoker = lift_invoker_sites(&[member_site(
        "net/minecraft/world/entity/item/FallingBlockEntity",
        "<init>",
        MixinAnnotationKind::Invoker,
        "create$init",
        "()Lnet/minecraft/world/entity/item/FallingBlockEntity;",
    )]);
    assert_eq!(
        ctor_invoker
            .rejected
            .iter()
            .map(|(_, r)| *r)
            .collect::<Vec<_>>(),
        [InvokerRejection::ConstructorTarget]
    );

    let invoke_travel = lift_mixin_sites(&[inject_site(
        "net/minecraft/world/entity/LivingEntity",
        "travel",
        InjectPoint::Invoke(
            "net/minecraft/world/entity/LivingEntity;move(Lnet/minecraft/world/entity/MoverType;Lnet/minecraft/world/phys/Vec3;)V"
                .into(),
        ),
    )]);
    assert_eq!(
        unliftable_reasons(&invoke_travel),
        [UnliftableReason::UnsupportedInjectPoint]
    );
}

// ---------------------------------------------------------------------------
// Minimal JVMS class/JAR fixture (self-contained, no external deps)
// ---------------------------------------------------------------------------

/// One annotation `element_value` for the fixture builder.
enum EV {
    /// `s` string constant.
    S(String),
    /// `Z` boolean (stored as a `CONSTANT_Integer`).
    Z(bool),
    /// `c` class literal (internal name).
    Cls(String),
    /// `@` nested annotation.
    Ann(Box<Ann>),
    /// `[` array.
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

fn cls(internal: &str) -> EV {
    EV::Cls(internal.to_string())
}

fn z(v: bool) -> EV {
    EV::Z(v)
}

fn nested(desc: &str, elems: Vec<(&str, EV)>) -> EV {
    EV::Ann(Box::new(ann(desc, elems)))
}

fn arr(items: Vec<EV>) -> EV {
    EV::Arr(items)
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

struct Cp {
    items: Vec<Vec<u8>>,
    utf8: std::collections::HashMap<Vec<u8>, u16>,
    class: std::collections::HashMap<Vec<u8>, u16>,
    ints: std::collections::HashMap<i32, u16>,
}

impl Cp {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            utf8: std::collections::HashMap::new(),
            class: std::collections::HashMap::new(),
            ints: std::collections::HashMap::new(),
        }
    }

    fn utf8(&mut self, s: &str) -> u16 {
        if let Some(&i) = self.utf8.get(s.as_bytes()) {
            return i;
        }
        let index = (self.items.len() + 1) as u16;
        let mut item = vec![1u8];
        item.extend((s.len() as u16).to_be_bytes());
        item.extend(s.as_bytes());
        self.items.push(item);
        self.utf8.insert(s.as_bytes().to_vec(), index);
        index
    }

    fn class(&mut self, internal: &str) -> u16 {
        if let Some(&i) = self.class.get(internal.as_bytes()) {
            return i;
        }
        let name_index = self.utf8(internal);
        let index = (self.items.len() + 1) as u16;
        let mut item = vec![7u8];
        item.extend(name_index.to_be_bytes());
        self.items.push(item);
        self.class.insert(internal.as_bytes().to_vec(), index);
        index
    }

    fn int(&mut self, v: i32) -> u16 {
        if let Some(&i) = self.ints.get(&v) {
            return i;
        }
        let index = (self.items.len() + 1) as u16;
        let mut item = vec![3u8];
        item.extend(v.to_be_bytes());
        self.items.push(item);
        self.ints.insert(v, index);
        index
    }
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

fn anns_attr(cp: &mut Cp, anns: &[Ann]) -> (u16, Vec<u8>) {
    let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
    for a in anns {
        write_ann(&mut payload, cp, a);
    }
    (cp.utf8("RuntimeInvisibleAnnotations"), payload)
}

/// Minimal `.class`: public class `internal`, no fields, each method a
/// plain stub carrying its annotations in the
/// `RuntimeInvisibleAnnotations` attribute (Mixin's real CLASS
/// retention).
fn build_class(internal: &str, class_anns: &[Ann], methods: &[Meth]) -> Vec<u8> {
    let mut cp = Cp::new();
    cp.class(internal);
    cp.class("java/lang/Object");

    let mut method_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
    for m in methods {
        cp.utf8(&m.name);
        cp.utf8(&m.descriptor);
        if !m.anns.is_empty() {
            method_attrs.push(anns_attr(&mut cp, &m.anns));
        } else {
            method_attrs.push((0, Vec::new()));
        }
    }
    let class_attr = if class_anns.is_empty() {
        None
    } else {
        Some(anns_attr(&mut cp, class_anns))
    };

    let mut out = Vec::new();
    out.extend(0xCAFE_BABEu32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // minor
    out.extend(61u16.to_be_bytes()); // major (Java 17)
    out.extend(((cp.items.len() + 1) as u16).to_be_bytes());
    for item in &cp.items {
        out.extend(item);
    }
    out.extend(0x0021u16.to_be_bytes()); // public + super
    out.extend(cp.class[internal.as_bytes()].to_be_bytes());
    out.extend(cp.class[b"java/lang/Object".as_slice()].to_be_bytes());
    out.extend(0u16.to_be_bytes()); // interfaces_count
    out.extend(0u16.to_be_bytes()); // fields_count
    out.extend((methods.len() as u16).to_be_bytes());
    for (m, (name_index, payload)) in methods.iter().zip(&method_attrs) {
        out.extend(0x0001u16.to_be_bytes()); // public
        out.extend(cp.utf8[m.name.as_bytes()].to_be_bytes());
        out.extend(cp.utf8[m.descriptor.as_bytes()].to_be_bytes());
        if *name_index == 0 {
            out.extend(0u16.to_be_bytes()); // no attributes
        } else {
            out.extend(1u16.to_be_bytes());
            out.extend(name_index.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
    }
    match class_attr {
        Some((name_index, payload)) => {
            out.extend(1u16.to_be_bytes());
            out.extend(name_index.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
        None => out.extend(0u16.to_be_bytes()),
    }
    out
}

use megu_core::hash::crc32_ieee as crc32;

/// Pack `entries` (name → bytes) into a real ZIP archive with stored
/// (uncompressed) members — all multi-byte fields little-endian per
/// APPNOTE.
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
        out.extend(0u16.to_le_bytes()); // stored
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
        central.extend(0u16.to_le_bytes()); // stored
        central.extend(0u16.to_le_bytes()); // mod time
        central.extend(0u16.to_le_bytes()); // mod date
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes()); // extra len
        central.extend(0u16.to_le_bytes()); // comment len
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_le_bytes()); // internal attrs
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

fn mixin_ann(target: &str) -> Ann {
    ann("Lorg/spongepowered/asm/mixin/Mixin;", vec![("value", cls(target))])
}

fn inject_ann(method: &str, at: &str, cancellable: bool) -> Ann {
    ann(
        "Lorg/spongepowered/asm/mixin/injection/Inject;",
        vec![
            ("method", arr(vec![s(method)])),
            (
                "at",
                nested(
                    "Lorg/spongepowered/asm/mixin/injection/At;",
                    vec![("value", s(at))],
                ),
            ),
            ("cancellable", z(cancellable)),
        ],
    )
}

#[test]
fn scanner_partitions_gametest_and_client_cosmetic_sites() {
    let config = br#"{
        "package": "com/example/test/mixin",
        "mixins": ["GameTestHelperMixin", "ArmorTrimMixin"]
    }"#;
    let gametest_mixin = build_class(
        "com/example/test/mixin/GameTestHelperMixin",
        &[mixin_ann("net/minecraft/gametest/framework/GameTestHelper")],
        &[meth("onRunTest", "()V", vec![inject_ann("runTest", "HEAD", false)])],
    );
    let cosmetic_mixin = build_class(
        "com/example/test/mixin/ArmorTrimMixin",
        &[mixin_ann("net/minecraft/world/item/armortrim/ArmorTrim")],
        &[meth(
            "onInnerTexture",
            "()V",
            vec![inject_ann("innerTexture", "HEAD", false)],
        )],
    );
    let jar_bytes = jar(&[
        ("test.mixins.json", config.as_slice()),
        (
            "com/example/test/mixin/GameTestHelperMixin.class",
            gametest_mixin.as_slice(),
        ),
        (
            "com/example/test/mixin/ArmorTrimMixin.class",
            cosmetic_mixin.as_slice(),
        ),
    ]);

    // Dedicated-server scan: both non-live targets are partitioned out
    // with telemetry and do not veto the verdict.
    let report = scan_jar_mixins_with_options(
        &jar_bytes,
        JarScanOptions { server_only: true },
    )
    .unwrap();
    assert_eq!(report.gametest_skipped, 1);
    assert_eq!(report.gametest_sites.len(), 1);
    assert_eq!(
        report.gametest_sites[0].target_class,
        "net/minecraft/gametest/framework/GameTestHelper"
    );
    assert_eq!(report.client_cosmetic_skipped, 1);
    assert_eq!(report.client_cosmetic_sites.len(), 1);
    assert_eq!(
        report.client_cosmetic_sites[0].target_class,
        "net/minecraft/world/item/armortrim/ArmorTrim"
    );
    assert!(report.sites.is_empty());
    assert_eq!(report.verdict, MixinLiftVerdict::FullyLiftable(vec![]));

    // A client-including scan still partitions GameTest (never live on
    // either side) but judges the cosmetic site as an ordinary live
    // site — fail-closed `UnknownTarget`, exactly like before.
    let report = scan_jar_mixins_with_options(
        &jar_bytes,
        JarScanOptions { server_only: false },
    )
    .unwrap();
    assert_eq!(report.gametest_skipped, 1);
    assert_eq!(report.client_cosmetic_skipped, 0);
    assert_eq!(report.sites.len(), 1);
    assert_eq!(
        unliftable_reasons(&report.verdict),
        [UnliftableReason::UnknownTarget]
    );
}
