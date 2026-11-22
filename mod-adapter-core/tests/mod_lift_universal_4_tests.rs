// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane MOD-LIFT-UNIVERSAL-4 regression tests: four generic, clean-room
//! mixin-lifter upgrades measured on real production JARs (create,
//! Farmer's Delight, waystones, balm).
//!
//! 1. `<prefix>$`-namespaced read-only `@Accessor` handlers
//!    (`create$getConcrete`, `my_mod$getHealth`) lift exactly like their
//!    bare counterparts, on a widened table of verified plain stored
//!    vanilla fields.
//! 2. Read-only `@Invoker` calls onto a verified pure query table lift
//!    natively (`BlockBehaviour#getShape`); constructors, mutating
//!    methods, and unknown targets stay strictly fail-closed.
//! 3. `net/minecraft/util/datafix/*` targets classify
//!    `legacy_datafix_skipped` — save-upgrade-only code, never a
//!    live-gameplay Tier-3 blocker.
//! 4. The canonical hook table grew universal 1.21.1 `HEAD`/`TAIL`/
//!    `RETURN` hooks (`FarmBlock#turnToDirt`, `Player#canEat`,
//!    `Item#useOn`, `Entity#tick`, `FenceBlock#connectsTo`,
//!    `FlowingFluid#canPassThrough`, ...) while `@At(INVOKE)` and
//!    `@ModifyArg` on gameplay classes stay strictly fail-closed.

use mod_adapter_core::mixin_lifter::{
    classify_mixin_target, is_legacy_datafix_target, lift_accessor_sites, lift_invoker_sites,
    lift_mixin_sites, AccessorRejection, InjectPoint, InvokerRejection, MixinAnnotationKind,
    MixinLiftVerdict, MixinSite, MixinTargetScope, StandardEventHook, UnliftableReason,
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

fn lifted_hooks(verdict: &MixinLiftVerdict) -> Vec<StandardEventHook> {
    let bindings = match verdict {
        MixinLiftVerdict::FullyLiftable(b) => b,
        MixinLiftVerdict::RequiresFallback { lifted, .. } => lifted,
    };
    bindings.iter().map(|b| b.hook).collect()
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
fn prefixed_readonly_accessor_and_safe_invoker_lift_while_mutating_fails_closed() {
    // A `<modid>$`-namespaced read-only getter on a verified plain stored
    // field lifts identically to the bare name: `my_mod$getHealth` reads
    // `ItemEntity#health` (`()I`).
    let accessor_plan = lift_accessor_sites(&[
        member_site(
            "net/minecraft/world/entity/item/ItemEntity",
            "health",
            MixinAnnotationKind::Accessor,
            "my_mod$getHealth",
            "()I",
        ),
        // Explicit `value` plus a namespaced handler still resolves the
        // declared field (`concrete` on `ConcretePowderBlock`).
        member_site(
            "net/minecraft/world/level/block/ConcretePowderBlock",
            "concrete",
            MixinAnnotationKind::Accessor,
            "my_mod$getConcrete",
            "()Lnet/minecraft/world/level/block/Block;",
        ),
    ]);
    assert!(accessor_plan.is_fully_lifted(), "{:?}", accessor_plan.rejected);
    let fields: Vec<&str> = accessor_plan
        .bindings
        .iter()
        .map(|b| b.field.as_str())
        .collect();
    assert_eq!(fields, ["ItemEntity#health", "ConcretePowderBlock#concrete"]);

    // A verified pure/read-only `@Invoker` query lifts to a native
    // method-call binding.
    let invoker_plan = lift_invoker_sites(&[member_site(
        "net/minecraft/world/level/block/state/BlockBehaviour",
        "getShape",
        MixinAnnotationKind::Invoker,
        "create$callGetShape",
        "(Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/world/level/BlockGetter;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/phys/shapes/CollisionContext;)Lnet/minecraft/world/phys/shapes/VoxelShape;",
    )]);
    assert!(invoker_plan.is_fully_lifted(), "{:?}", invoker_plan.rejected);
    assert_eq!(invoker_plan.bindings[0].method, "BlockBehaviour#getShape");
    assert_eq!(
        invoker_plan.tier(),
        mod_adapter_core::fallback::ExecutionTier::UniversalNative
    );

    // Fail closed: constructor invokers, mutating invokers, mutating
    // accessor setters, and unknown fields/methods.
    let rejected = lift_invoker_sites(&[
        member_site(
            "net/minecraft/world/entity/item/FallingBlockEntity",
            "<init>",
            MixinAnnotationKind::Invoker,
            "create$init",
            "()Lnet/minecraft/world/entity/item/FallingBlockEntity;",
        ),
        member_site(
            "net/minecraft/world/entity/LivingEntity",
            "spawnItemParticles",
            MixinAnnotationKind::Invoker,
            "create$invokeSpawnItemParticles",
            "(Lnet/minecraft/world/item/ItemStack;I)V",
        ),
        member_site(
            "net/minecraft/world/entity/LivingEntity",
            "method_99999",
            MixinAnnotationKind::Invoker,
            "create$callSomething",
            "()V",
        ),
    ]);
    assert!(rejected.bindings.is_empty());
    let reasons: Vec<InvokerRejection> =
        rejected.rejected.iter().map(|(_, r)| *r).collect();
    assert_eq!(
        reasons,
        [
            InvokerRejection::ConstructorTarget,
            InvokerRejection::MutatingTarget,
            InvokerRejection::UnknownMethod,
        ]
    );

    let accessor_rejected = lift_accessor_sites(&[
        member_site(
            "net/minecraft/world/inventory/Slot",
            "x",
            MixinAnnotationKind::Accessor,
            "my_mod$setX",
            "(I)V",
        ),
        member_site(
            "net/minecraft/world/inventory/Slot",
            "notAField",
            MixinAnnotationKind::Accessor,
            "my_mod$getNotAField",
            "()I",
        ),
    ]);
    assert!(accessor_rejected.bindings.is_empty());
    let accessor_reasons: Vec<AccessorRejection> = accessor_rejected
        .rejected
        .iter()
        .map(|(_, r)| *r)
        .collect();
    assert_eq!(
        accessor_reasons,
        [
            AccessorRejection::MutatingSetter,
            AccessorRejection::UnknownField,
        ]
    );
}

#[test]
fn legacy_datafix_mixins_classify_nonblocking_and_universal_1211_hooks_lift() {
    // Legacy save-upgrade targets classify non-blocking: V1460 and the
    // other DataFixerUpper schema/fix classes only run inside pre-1.21.1
    // world migrations — never live gameplay.
    for class in [
        "net/minecraft/util/datafix/schemas/V1460",
        "net/minecraft/util/datafix/schemas/V3818_3",
        "net/minecraft/util/datafix/fixes/ItemStackComponentizationFix",
        "net/minecraft/util/datafix/fixes/BlockPosFormatAndRenamesFix",
        // Yarn dialect of the same tree.
        "net/minecraft/datafixer/schema/Schema1460",
        // Dot form normalizes identically.
        "net.minecraft.util.datafix.schemas.V1460",
    ] {
        assert_eq!(
            classify_mixin_target(class),
            MixinTargetScope::LegacyDataFixOnly,
            "{class}"
        );
        assert!(is_legacy_datafix_target(class), "{class}");
    }
    // Anything else — including names merely containing "datafix" — is
    // fail-closed live gameplay.
    for class in [
        "net/minecraft/world/entity/Entity",
        "net/minecraft/util/datafixer/NotARealPath",
        "com/example/mymod/datafix/MyFix",
    ] {
        assert_eq!(classify_mixin_target(class), MixinTargetScope::LiveGameplay, "{class}");
    }

    // Universal 1.21.1 hooks: `HEAD`, `RETURN`, and `TAIL` all lift onto
    // the same single-phase hook per target.
    for (point, class, method, hook) in [
        (
            InjectPoint::Head,
            "net/minecraft/world/level/block/FarmBlock",
            "turnToDirt",
            StandardEventHook::FarmlandTrample,
        ),
        (
            InjectPoint::Return,
            "net/minecraft/world/entity/player/Player",
            "canEat",
            StandardEventHook::PlayerCanEatQuery,
        ),
        (
            InjectPoint::Tail,
            "net/minecraft/world/item/Item",
            "useOn",
            StandardEventHook::ItemUseOnBlock,
        ),
        (
            InjectPoint::Head,
            "net/minecraft/world/entity/Entity",
            "tick",
            StandardEventHook::EntityTick,
        ),
        (
            InjectPoint::Tail,
            "net/minecraft/world/entity/Entity",
            "tick",
            StandardEventHook::EntityTick,
        ),
        (
            InjectPoint::Head,
            "net/minecraft/world/level/block/FenceBlock",
            "connectsTo",
            StandardEventHook::BlockConnectQuery,
        ),
        (
            InjectPoint::Head,
            "net/minecraft/world/level/material/FlowingFluid",
            "canPassThrough",
            StandardEventHook::FluidPassQuery,
        ),
        // Intermediary spellings resolve to the same hooks.
        (
            InjectPoint::Head,
            "net/minecraft/class_2344",
            "method_10125",
            StandardEventHook::FarmlandTrample,
        ),
        (
            InjectPoint::Head,
            "net/minecraft/class_1297",
            "method_5773",
            StandardEventHook::EntityTick,
        ),
    ] {
        let verdict = lift_mixin_sites(&[inject_site(class, method, point.clone())]);
        assert!(
            matches!(verdict, MixinLiftVerdict::FullyLiftable(_)),
            "{class}#{method} @ {point:?}: {verdict:?}"
        );
        assert_eq!(lifted_hooks(&verdict), [hook], "{class}#{method}");
    }

    // The wider universal table.
    for (class, method, hook) in [
        (
            "net/minecraft/world/entity/Entity",
            "load",
            StandardEventHook::EntityDataSync,
        ),
        (
            "net/minecraft/world/level/block/CampfireBlock",
            "isSmokeSource",
            StandardEventHook::CampfireSmokeQuery,
        ),
        (
            "net/minecraft/world/level/block/CropBlock",
            "mayPlaceOn",
            StandardEventHook::PlantPlaceQuery,
        ),
        (
            "net/minecraft/server/ServerScoreboard",
            "addPlayerToTeam",
            StandardEventHook::ScoreboardTeamChange,
        ),
        (
            "net/minecraft/server/ServerScoreboard",
            "removePlayerFromTeam",
            StandardEventHook::ScoreboardTeamChange,
        ),
        (
            "net/minecraft/server/MinecraftServer",
            "reloadResources",
            StandardEventHook::ResourceReload,
        ),
        (
            "net/minecraft/server/ReloadableServerResources",
            "loadResources",
            StandardEventHook::ResourceReload,
        ),
        (
            "net/minecraft/world/level/saveddata/maps/MapItemSavedData",
            "checkBanners",
            StandardEventHook::MapDataSync,
        ),
        (
            "net/minecraft/world/level/levelgen/feature/Feature",
            "isGrassOrDirt",
            StandardEventHook::TreeSoilQuery,
        ),
        (
            "net/minecraft/world/level/levelgen/feature/trunkplacers/TrunkPlacer",
            "setDirtAt",
            StandardEventHook::TreeSoilQuery,
        ),
    ] {
        let verdict = lift_mixin_sites(&[inject_site(class, method, InjectPoint::Head)]);
        assert!(
            matches!(verdict, MixinLiftVerdict::FullyLiftable(_)),
            "{class}#{method}: {verdict:?}"
        );
        assert_eq!(lifted_hooks(&verdict), [hook], "{class}#{method}");
    }

    // `@At(INVOKE)` on a gameplay class stays strictly fail-closed, and a
    // legacy-datafix *target* still fails closed when it reaches the
    // lifter directly (classification is the scanner's job; the lifter
    // itself keeps `UnknownTarget` for datafix classes).
    let mut invoke = inject_site(
        "net/minecraft/world/entity/Entity",
        "tick",
        InjectPoint::Invoke("net/minecraft/world/entity/Entity;move".into()),
    );
    let invoke_verdict = lift_mixin_sites(&[invoke.clone()]);
    assert_eq!(
        unliftable_reasons(&invoke_verdict),
        [UnliftableReason::UnsupportedInjectPoint]
    );

    // `@ModifyArg` on a gameplay class stays strictly fail-closed.
    invoke.annotation = MixinAnnotationKind::ModifyArg;
    invoke.at = Some(InjectPoint::Head);
    let modify_arg = lift_mixin_sites(&[invoke]);
    assert_eq!(
        unliftable_reasons(&modify_arg),
        [UnliftableReason::UnsupportedAnnotation]
    );

    // A `@ModifyArg` aimed at a datafix class is no live-gameplay blocker
    // only via classification — straight through the lifter it is still
    // `UnsupportedAnnotation`, and even a plain `@Inject` there is
    // `UnknownTarget` (not a hook target).
    let datafix_modify = lift_mixin_sites(&[MixinSite {
        annotation: MixinAnnotationKind::ModifyArg,
        ..inject_site(
            "net/minecraft/util/datafix/schemas/V1460",
            "register",
            InjectPoint::Head,
        )
    }]);
    assert_eq!(
        unliftable_reasons(&datafix_modify),
        [UnliftableReason::UnsupportedAnnotation]
    );
    let datafix_inject = lift_mixin_sites(&[inject_site(
        "net/minecraft/util/datafix/schemas/V1460",
        "register",
        InjectPoint::Head,
    )]);
    assert_eq!(
        unliftable_reasons(&datafix_inject),
        [UnliftableReason::UnknownTarget]
    );
}
