// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room Mixin-to-Event static lifter.
//!
//! Most real modpack Mixin usage is a small set of boring, standard
//! `@Inject(at = @At("HEAD" | "TAIL" | "RETURN"))` hooks on vanilla lifecycle
//! methods — damage, block break/place, server/player ticks, chat, join/leave,
//! and block interaction. [`lift_mixin_sites`] maps exactly those sites onto
//! megu's [`StandardEventHook`]s so
//! [`crate::fallback::classify_mod_with_mixins`] can promote a mod whose *only*
//! unsupported surface is a fully liftable mixin set from Tier 3
//! (`RequiresFallback`) to Tier 2 (`BoundedJvmShim`) with exact event
//! bindings.
//!
//! The lifter is strictly fail-closed. Anything it cannot prove safe —
//! `@Overwrite` / `@Redirect` / `@ModifyVariable` / `@ModifyArg` /
//! `@ModifyConstant` / unknown annotations,
//! `locals` capture, `@At("INVOKE")` / `@At("FIELD")` or any non-trivial
//! injection point, a cancellable injection at a point where the vanilla
//! work is already done, and any target class or method outside the standard
//! table — is reported [`UnliftableReason`] and keeps the whole mod at
//! Tier 3. It never guesses. The non-mutating `@Shadow`/`@Unique`
//! member *declarations* are not injection sites at all and never reach the
//! lifter (see [`MixinMemberKind`]); a manually constructed site carrying
//! one still fails closed here.
//!
//! Two narrowly verified surfaces lift alongside `@Inject` sites instead of
//! blocking: read-only `@Accessor` getters (`get*`/`is*` shape on a
//! allowlisted plain stored vanilla field — [`lift_accessor_sites`]) and
//! read-only `@Invoker` calls (a verified pure query method —
//! [`lift_invoker_sites`]). Mutating accessors (`set*`), constructor or
//! mutating invokers (`<init>`, `spawnItemParticles`, `set*`), and any
//! member outside those tables stay strictly fail-closed. A
//! `cancellable = true` `TAIL`/`RETURN` site on a verified
//! value-returning target lifts as [`LiftedHookKind::ReturnValueOverride`]
//! (`CIR.setReturnValue`); on a void target it still fails closed as
//! `NonCancellablePostHook`. Finally, `net/minecraft/util/datafix/*`
//! targets execute only inside pre-1.21.1 save upgrades and
//! `net/minecraft/gametest/*` targets only inside the GameTest harness —
//! never live gameplay — so [`classify_mixin_target`] records them
//! `legacy_datafix_skipped` / `gametest_skipped` instead of letting them
//! veto a mod's live-gameplay surface, and a dedicated-server scan
//! records strictly client-visual-only targets (armor-trim sprite
//! paths, item eat/drink particles) as `client_cosmetic_skipped`.
//!
//! The target table below covers the 1.21.1 vanilla lifecycle in three naming
//! dialects, normalized by [`lift_mixin_sites`]:
//!
//! - Mojang official names in dot (`net.minecraft.world.entity.LivingEntity`)
//!   or slash (`net/minecraft/world/entity/LivingEntity`) form, per the
//!   official 1.21.1 `server.txt`/`client.txt` mappings (`hurt`, `tick`,
//!   `destroyBlock`, `place`/`placeBlock`, `handleChat`, `placeNewPlayer`,
//!   `remove`, `useWithoutItem`, `useItemOn`, `doTick`).
//! - Fabric Intermediary names (`net/minecraft/class_1309` + `method_5643`),
//!   per the `intermediary-1.21.1` / `yarn-1.21.1+build.3` mappings pinned by
//!   `tools/vanilla_data/gen_vanilla_data.sh`. Intermediary numbers are
//!   hierarchy-shared: an override inherits the parent's number (so
//!   `Entity`/`LivingEntity`/`Player`/`ServerPlayer` `tick()` all resolve to
//!   `method_5773`, `hurt` to `method_5643`), which is why broad targets like
//!   `Entity.tick` deliberately do *not* map to the player-scoped hooks.
//! - Yarn named aliases (`damage`, `tryBreakBlock`, `onPlayerConnect`,
//!   `onChatMessage`, `onUse`, `onUseWithItem`, `playerTick`) where they were
//!   verified in the same pinned mappings.
//!
//! Clean-room note: the table below is factual mapping data (class and method
//! names) only — no code, comments, or logic from Minecraft, Fabric, Yarn,
//! Intermediary, or any mod is copied, linked, bundled, or derived from here.

use std::fmt;

use crate::intermediary_map::TinyV2MappingTable;

/// The Mixin annotation attached to one injection site, as reported by the
/// caller's bytecode/config scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MixinAnnotationKind {
    Inject,
    Overwrite,
    Redirect,
    ModifyVariable,
    ModifyArg,
    ModifyConstant,
    Accessor,
    Invoker,
    /// `@Shadow` — declares a reference to an existing vanilla member.
    Shadow,
    /// `@Unique` — marks a mixin-private helper member.
    Unique,
    Unknown(String),
}

/// Whether one mixin member annotation injects/overwrites vanilla behavior
/// or merely *declares* a member. `@Shadow` only references an existing
/// vanilla field/method and `@Unique` only marks a mixin-private helper —
/// neither injects into, wraps, redirects, nor overwrites vanilla code, so
/// neither is an injection site (and neither is a Tier-3 blocker).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixinMemberKind {
    /// An injection/transformation the lifter must judge fail-closed.
    Injection,
    /// A non-mutating declaration (`@Shadow`, `@Unique`) — recorded in
    /// scan diagnostics, never lifted and never a blocker.
    Declaration,
}

impl MixinAnnotationKind {
    /// Stable operator-facing name (`@Inject`, `@Redirect`, ...).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Inject => "@Inject",
            Self::Overwrite => "@Overwrite",
            Self::Redirect => "@Redirect",
            Self::ModifyVariable => "@ModifyVariable",
            Self::ModifyArg => "@ModifyArg",
            Self::ModifyConstant => "@ModifyConstant",
            Self::Accessor => "@Accessor",
            Self::Invoker => "@Invoker",
            Self::Shadow => "@Shadow",
            Self::Unique => "@Unique",
            Self::Unknown(name) => name.as_str(),
        }
    }

    /// [`MixinMemberKind::Declaration`] for `@Shadow`/`@Unique`;
    /// [`MixinMemberKind::Injection`] for everything else.
    pub fn member_kind(&self) -> MixinMemberKind {
        match self {
            Self::Shadow | Self::Unique => MixinMemberKind::Declaration,
            _ => MixinMemberKind::Injection,
        }
    }
}

impl fmt::Display for MixinAnnotationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `@At` injection point of one site. `None`/`Other` capture anything the
/// scan could not reduce to a known point; both fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectPoint {
    Head,
    Tail,
    Return,
    Invoke(String),
    Field(String),
    Other(String),
}

impl InjectPoint {
    /// Stable operator-facing name (`HEAD`, `INVOKE`, ...).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Head => "HEAD",
            Self::Tail => "TAIL",
            Self::Return => "RETURN",
            Self::Invoke(detail) | Self::Field(detail) | Self::Other(detail) => detail.as_str(),
        }
    }
}

impl fmt::Display for InjectPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invoke(detail) => write!(f, "INVOKE({detail})"),
            Self::Field(detail) => write!(f, "FIELD({detail})"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// One Mixin injection site extracted from a mod JAR (bytecode scan or mixin
/// config), already reduced to the fields the lifter needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MixinSite {
    /// Internal name of the mixin class declaring the handler.
    pub mixin_class: String,
    /// Vanilla class the handler injects into; dot, slash, descriptor, or
    /// Intermediary form are all accepted and normalized by the lifter.
    pub target_class: String,
    /// Target method name in the same dialect as `target_class`.
    pub target_method: String,
    pub annotation: MixinAnnotationKind,
    /// `@At` point. `None` means the scan saw no usable `at=` and fails closed.
    pub at: Option<InjectPoint>,
    /// `cancellable = true` on the `@Inject`.
    pub cancellable: bool,
    /// Handler captures locals (`locals = ...` or `LocalCapture`).
    pub captures_locals: bool,
    /// Declared name of the mixin handler method itself (e.g. `getYRot`) —
    /// the name an `@Accessor`'s shape is judged by.
    pub handler_name: String,
    /// JVM descriptor of the handler method (`()F` for a read-only getter,
    /// `(F)V` for a mutating setter). Empty when the producer does not know
    /// it; the accessor lifter treats that as unverifiable.
    pub handler_descriptor: String,
}

/// The standard lifecycle events a lifted site binds to in the Tier 2
/// (`BoundedJvmShim`) JVM shim runtime. `*Pre` hooks fire before the vanilla
/// body (cancellable); `*Post` hooks fire after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StandardEventHook {
    EntityDamagePre,
    EntityDamagePost,
    BlockBreakPre,
    BlockBreakPost,
    BlockPlacePre,
    BlockPlacePost,
    ServerTickPre,
    ServerTickPost,
    PlayerTickPre,
    PlayerTickPost,
    ServerChat,
    PlayerJoin,
    PlayerLeave,
    BlockInteract,
    /// An entity used a held item in the air (NeoForge
    /// `RightClickItem`, Fabric `UseItemCallback`).
    ItemUse,
    /// A player attack on an entity, before damage resolves (Fabric
    /// `AttackEntityCallback`).
    EntityAttack,
    /// A living entity died (NeoForge `LivingDeathEvent`).
    EntityDeath,
    /// An entity was added to a level (NeoForge `EntityJoinLevelEvent`).
    EntityJoinLevel,
    /// The server command dispatcher is being built (NeoForge
    /// `RegisterCommandsEvent`, Fabric `CommandRegistrationCallback`).
    CommandRegister,
    /// A farmland block was trampled into dirt (`FarmBlock.turnToDirt`).
    FarmlandTrample,
    /// A player's ability to eat was queried (`Player.canEat`).
    PlayerCanEatQuery,
    /// An item was used on a block (`Item.useOn`, Fabric `useOnBlock`).
    ItemUseOnBlock,
    /// Any entity ticked (`Entity.tick` — every entity, not just players).
    EntityTick,
    /// An entity's persistent data was loaded or saved
    /// (`Entity.load` / `Entity.save`).
    EntityDataSync,
    /// A fence block's connection query ran (`FenceBlock.connectsTo` /
    /// `FenceBlock.isSameFence`).
    BlockConnectQuery,
    /// A campfire's smoke-source query ran (`CampfireBlock.isSmokeSource`).
    CampfireSmokeQuery,
    /// A crop/stem block's soil-suitability query ran
    /// (`CropBlock.mayPlaceOn` / `StemBlock.mayPlaceOn`).
    PlantPlaceQuery,
    /// A flowing fluid's pass-through query ran
    /// (`FlowingFluid.canPassThrough`).
    FluidPassQuery,
    /// A scoreboard team membership changed
    /// (`ServerScoreboard.addPlayerToTeam` / `removePlayerFromTeam`).
    ScoreboardTeamChange,
    /// Server datapack resources reloaded
    /// (`MinecraftServer.reloadResources` /
    /// `ReloadableServerResources.loadResources`).
    ResourceReload,
    /// Map data was loaded, saved, or banner-checked
    /// (`MapItemSavedData.load` / `save` / `checkBanners`).
    MapDataSync,
    /// A tree/soil suitability query ran (`Feature.isGrassOrDirt` /
    /// `TrunkPlacer.setDirtAt`).
    TreeSoilQuery,
    /// A loot bonus-count function rolled
    /// (`EnchantedCountIncreaseFunction.run`).
    LootBonusRoll,
    /// A structure pool element placed into the world
    /// (`SinglePoolElement.place`).
    StructurePiecePlace,
}

impl StandardEventHook {
    /// Stable operator-facing name (the variant name).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EntityDamagePre => "EntityDamagePre",
            Self::EntityDamagePost => "EntityDamagePost",
            Self::BlockBreakPre => "BlockBreakPre",
            Self::BlockBreakPost => "BlockBreakPost",
            Self::BlockPlacePre => "BlockPlacePre",
            Self::BlockPlacePost => "BlockPlacePost",
            Self::ServerTickPre => "ServerTickPre",
            Self::ServerTickPost => "ServerTickPost",
            Self::PlayerTickPre => "PlayerTickPre",
            Self::PlayerTickPost => "PlayerTickPost",
            Self::ServerChat => "ServerChat",
            Self::PlayerJoin => "PlayerJoin",
            Self::PlayerLeave => "PlayerLeave",
            Self::BlockInteract => "BlockInteract",
            Self::ItemUse => "ItemUse",
            Self::EntityAttack => "EntityAttack",
            Self::EntityDeath => "EntityDeath",
            Self::EntityJoinLevel => "EntityJoinLevel",
            Self::CommandRegister => "CommandRegister",
            Self::FarmlandTrample => "FarmlandTrample",
            Self::PlayerCanEatQuery => "PlayerCanEatQuery",
            Self::ItemUseOnBlock => "ItemUseOnBlock",
            Self::EntityTick => "EntityTick",
            Self::EntityDataSync => "EntityDataSync",
            Self::BlockConnectQuery => "BlockConnectQuery",
            Self::CampfireSmokeQuery => "CampfireSmokeQuery",
            Self::PlantPlaceQuery => "PlantPlaceQuery",
            Self::FluidPassQuery => "FluidPassQuery",
            Self::ScoreboardTeamChange => "ScoreboardTeamChange",
            Self::ResourceReload => "ResourceReload",
            Self::MapDataSync => "MapDataSync",
            Self::TreeSoilQuery => "TreeSoilQuery",
            Self::LootBonusRoll => "LootBonusRoll",
            Self::StructurePiecePlace => "StructurePiecePlace",
        }
    }
}

impl fmt::Display for StandardEventHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a lifted binding relates to the vanilla method's return value.
/// Plain observation sites are [`LiftedHookKind::Standard`]; a
/// `cancellable = true` `TAIL`/`RETURN` site on a verified
/// value-returning target is [`LiftedHookKind::ReturnValueOverride`],
/// where `CallbackInfoReturnable.setReturnValue` lowers onto a
/// return-value override — distinct from the fail-closed
/// [`UnliftableReason::NonCancellablePostHook`] rejection a post hook on
/// a void target still gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiftedHookKind {
    /// A plain pre/post observation binding.
    Standard,
    /// A post-phase return-value override (`CIR.setReturnValue`).
    ReturnValueOverride,
}

impl LiftedHookKind {
    /// Stable operator-facing name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::ReturnValueOverride => "ReturnValueOverride",
        }
    }
}

impl fmt::Display for LiftedHookKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One successfully lifted injection site bound to a standard event hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiftedBinding {
    pub site: MixinSite,
    pub hook: StandardEventHook,
    /// Mirrors the site's `cancellable` flag; `false` for non-cancellable
    /// post hooks and `true` for post hooks only when the verified target
    /// returns a value ([`LiftedHookKind::ReturnValueOverride`] — a
    /// cancellable post injection on a void target is
    /// [`UnliftableReason::NonCancellablePostHook`]).
    pub cancellable: bool,
    /// Whether the binding observes or overrides the vanilla return
    /// value. [`LiftedHookKind::ReturnValueOverride`] only on
    /// `cancellable` `TAIL`/`RETURN` sites against verified
    /// value-returning targets.
    pub kind: LiftedHookKind,
}

/// Why a single site cannot lift. Every variant keeps the mod at Tier 3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnliftableReason {
    /// Anything other than plain `@Inject` (`@Overwrite`, `@Redirect`,
    /// `@ModifyVariable`, `@ModifyArg`, `@ModifyConstant`, `@Accessor`,
    /// `@Invoker`, or an unrecognized annotation).
    UnsupportedAnnotation,
    /// Injection point outside `HEAD`/`TAIL`/`RETURN`, or no usable `at=`.
    UnsupportedInjectPoint,
    /// Handler captures local variables (`locals`/`LocalCapture`) — the shim
    /// event API cannot reproduce captured locals.
    CapturesLocals,
    /// Target class or method outside the verified standard-hook table
    /// (e.g. deep chunk/physics internals such as Valkyrien Skies 2 Mixins).
    UnknownTarget,
    /// `cancellable = true` at `TAIL`/`RETURN` (or any post-event binding):
    /// the vanilla work already happened, so the hook cannot be cancelled.
    NonCancellablePostHook,
}

impl UnliftableReason {
    /// Stable operator-facing name (the variant name).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnsupportedAnnotation => "UnsupportedAnnotation",
            Self::UnsupportedInjectPoint => "UnsupportedInjectPoint",
            Self::CapturesLocals => "CapturesLocals",
            Self::UnknownTarget => "UnknownTarget",
            Self::NonCancellablePostHook => "NonCancellablePostHook",
        }
    }
}

impl fmt::Display for UnliftableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Verdict for one mod's complete Mixin surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MixinLiftVerdict {
    /// Every site lifted; `[]` only when the caller passed an empty slice.
    FullyLiftable(Vec<LiftedBinding>),
    /// At least one site failed closed; the mod stays Tier 3. `lifted`
    /// records the bindings that *would* have applied for diagnostics.
    RequiresFallback {
        lifted: Vec<LiftedBinding>,
        unliftable: Vec<(MixinSite, UnliftableReason)>,
    },
}

/// One row of the standard-hook target table: a vanilla lifecycle method,
/// named in every accepted dialect, bound to a `HEAD` (pre) hook and a
/// `TAIL`/`RETURN` (post) hook. Single-phase events repeat the same hook.
struct StandardTarget {
    /// Accepted `target_class` values after [`normalize_class`] (`a/b/C` form).
    classes: &'static [&'static str],
    /// Accepted `target_method` values (Mojang, Intermediary, Yarn named).
    methods: &'static [&'static str],
    /// Hook bound for `@At("HEAD")`.
    pre: StandardEventHook,
    /// Hook bound for `@At("TAIL")` / `@At("RETURN")`.
    post: StandardEventHook,
}

/// The verified 1.21.1 vanilla lifecycle table. Method aliases per class were
/// cross-checked against the pinned Intermediary/Yarn mappings and Mojang
/// `server.txt`/`client.txt`; see the module docs for the exact pairs.
const STANDARD_TARGETS: &[StandardTarget] = &[
    // Entity damage: `Entity.hurt`/`LivingEntity.hurt` (intermediary
    // `Entity.damage` = class_1297.method_5643, inherited by LivingEntity).
    // `Player.hurt`/`ServerPlayer.hurt` intentionally stay unmapped: the
    // shim cannot narrow dispatch to one entity subclass.
    StandardTarget {
        classes: &[
            "net/minecraft/world/entity/Entity",
            "net/minecraft/world/entity/LivingEntity",
            "net/minecraft/class_1297",
            "net/minecraft/class_1309",
        ],
        methods: &["hurt", "damage", "method_5643"],
        pre: StandardEventHook::EntityDamagePre,
        post: StandardEventHook::EntityDamagePost,
    },
    // Block break: `ServerPlayerGameMode.destroyBlock` (intermediary
    // `tryBreakBlock` = class_3225.method_14266).
    StandardTarget {
        classes: &[
            "net/minecraft/server/level/ServerPlayerGameMode",
            "net/minecraft/class_3225",
        ],
        methods: &["destroyBlock", "tryBreakBlock", "method_14266"],
        pre: StandardEventHook::BlockBreakPre,
        post: StandardEventHook::BlockBreakPost,
    },
    // Block place: `BlockItem.place` / `BlockItem.placeBlock` (intermediary
    // `place` = class_1747.method_7712, `method_7708`).
    StandardTarget {
        classes: &[
            "net/minecraft/world/item/BlockItem",
            "net/minecraft/class_1747",
        ],
        methods: &["place", "placeBlock", "method_7712", "method_7708"],
        pre: StandardEventHook::BlockPlacePre,
        post: StandardEventHook::BlockPlacePost,
    },
    // Server level tick: `ServerLevel.tick` (class_3218.method_18765).
    StandardTarget {
        classes: &[
            "net/minecraft/server/level/ServerLevel",
            "net/minecraft/class_3218",
        ],
        methods: &["tick", "method_18765"],
        pre: StandardEventHook::ServerTickPre,
        post: StandardEventHook::ServerTickPost,
    },
    // Player tick: `Player.tick` and `ServerPlayer.tick`/`doTick`
    // (class_1657/class_3222.method_5773 via Entity.tick inheritance,
    // class_3222.method_14226 = yarn `playerTick` = Mojang `doTick`).
    // `Entity.tick`/`LivingEntity.tick` stay unmapped: they tick far more
    // than players, so lifting them would silently change semantics.
    StandardTarget {
        classes: &[
            "net/minecraft/world/entity/player/Player",
            "net/minecraft/server/level/ServerPlayer",
            "net/minecraft/class_1657",
            "net/minecraft/class_3222",
        ],
        methods: &[
            "tick",
            "doTick",
            "playerTick",
            "method_5773",
            "method_14226",
        ],
        pre: StandardEventHook::PlayerTickPre,
        post: StandardEventHook::PlayerTickPost,
    },
    // Server chat: `ServerGamePacketListenerImpl.handleChat` and its
    // `ServerGamePacketListener` interface declaration (intermediary
    // `onChatMessage` = method_12048 on class_3244 / class_2792).
    StandardTarget {
        classes: &[
            "net/minecraft/server/network/ServerGamePacketListenerImpl",
            "net/minecraft/network/protocol/game/ServerGamePacketListener",
            "net/minecraft/class_3244",
            "net/minecraft/class_2792",
        ],
        methods: &["handleChat", "onChatMessage", "method_12048"],
        pre: StandardEventHook::ServerChat,
        post: StandardEventHook::ServerChat,
    },
    // Player join: `PlayerList.placeNewPlayer` (intermediary
    // `onPlayerConnect` = class_3324.method_14570).
    StandardTarget {
        classes: &[
            "net/minecraft/server/players/PlayerList",
            "net/minecraft/class_3324",
        ],
        methods: &["placeNewPlayer", "onPlayerConnect", "method_14570"],
        pre: StandardEventHook::PlayerJoin,
        post: StandardEventHook::PlayerJoin,
    },
    // Player leave: `PlayerList.remove` (class_3324.method_14611).
    StandardTarget {
        classes: &[
            "net/minecraft/server/players/PlayerList",
            "net/minecraft/class_3324",
        ],
        methods: &["remove", "method_14611"],
        pre: StandardEventHook::PlayerLeave,
        post: StandardEventHook::PlayerLeave,
    },
    // Block interact: `BlockBehaviour.useWithoutItem`/`useItemOn` plus the
    // `BlockBehaviour$BlockStateBase` wrappers (class_4970.method_55766/
    // method_55765, class_4970$class_4971.method_55781/method_55780; yarn
    // `onUse`/`onUseWithItem`). Legacy `use`/`onUse` names are accepted on
    // the same classes for pre-split mappings.
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/block/state/BlockBehaviour",
            "net/minecraft/world/level/block/state/BlockBehaviour$BlockStateBase",
            "net/minecraft/class_4970",
            "net/minecraft/class_4970$class_4971",
        ],
        methods: &[
            "useWithoutItem",
            "useItemOn",
            "use",
            "onUse",
            "onUseWithItem",
            "method_55766",
            "method_55765",
            "method_55781",
            "method_55780",
        ],
        pre: StandardEventHook::BlockInteract,
        post: StandardEventHook::BlockInteract,
    },
    // -------------------------------------------------------------------
    // Universal 1.21.1 single-phase hooks: generic vanilla gameplay query
    // and lifecycle methods every loader ecosystem mixins into. Like
    // `ServerChat`, `HEAD` and `TAIL`/`RETURN` bind the same hook — the
    // event describes the call, and only a `HEAD` site may cancel it.
    // -------------------------------------------------------------------
    // Farmland trample: `FarmBlock.turnToDirt` (yarn `setToDirt` =
    // class_2344.method_10125). `fallOn` deliberately stays unmapped: it
    // covers every fall onto farmland, not just the trample decision.
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/block/FarmBlock",
            "net/minecraft/class_2344",
        ],
        methods: &[
            "turnToDirt",
            "setToDirt",
            "method_10125",
            "turnToDirt(Lnet/minecraft/world/entity/Entity;Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/world/level/Level;Lnet/minecraft/core/BlockPos;)V",
        ],
        pre: StandardEventHook::FarmlandTrample,
        post: StandardEventHook::FarmlandTrample,
    },
    // Eat check: `Player.canEat` (yarn `canConsume` = method_7332).
    StandardTarget {
        classes: &[
            "net/minecraft/world/entity/player/Player",
            "net/minecraft/class_1657",
        ],
        methods: &["canEat", "canConsume", "method_7332", "canEat(Z)Z"],
        pre: StandardEventHook::PlayerCanEatQuery,
        post: StandardEventHook::PlayerCanEatQuery,
    },
    // Item used on a block: `Item.useOn` (yarn `useOnBlock` =
    // method_7884). Distinct from `BlockInteract` (`Block.useItemOn`): this
    // is the item-side entry point every item inherits.
    StandardTarget {
        classes: &["net/minecraft/world/item/Item", "net/minecraft/class_1792"],
        methods: &[
            "useOn",
            "useOnBlock",
            "method_7884",
            "useOn(Lnet/minecraft/world/item/context/UseOnContext;)Lnet/minecraft/world/InteractionResult;",
        ],
        pre: StandardEventHook::ItemUseOnBlock,
        post: StandardEventHook::ItemUseOnBlock,
    },
    // Entity tick: `Entity.tick` (class_1297.method_5773). This is the
    // whole-entity tick — `LivingEntity.tick` stays unmapped (it runs for
    // a materially different population than the shim can narrow), while
    // `Player`/`ServerPlayer` `tick` keeps binding `PlayerTick*` from the
    // row above.
    StandardTarget {
        classes: &[
            "net/minecraft/world/entity/Entity",
            "net/minecraft/class_1297",
        ],
        methods: &["tick", "method_5773", "tick()V"],
        pre: StandardEventHook::EntityTick,
        post: StandardEventHook::EntityTick,
    },
    // Entity persistence: `Entity.load` / `Entity.save` (yarn `readNbt` /
    // `writeNbt` = method_5651 / method_5647).
    StandardTarget {
        classes: &[
            "net/minecraft/world/entity/Entity",
            "net/minecraft/class_1297",
        ],
        methods: &[
            "load",
            "save",
            "readNbt",
            "writeNbt",
            "method_5651",
            "method_5647",
            "load(Lnet/minecraft/nbt/CompoundTag;)V",
            "save(Lnet/minecraft/nbt/CompoundTag;)Z",
        ],
        pre: StandardEventHook::EntityDataSync,
        post: StandardEventHook::EntityDataSync,
    },
    // Fence connection query: `FenceBlock.connectsTo` / `isSameFence`
    // (yarn `canConnect` / `canConnectToFence` = method_10184 /
    // method_26375).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/block/FenceBlock",
            "net/minecraft/class_2354",
        ],
        methods: &[
            "connectsTo",
            "isSameFence",
            "canConnect",
            "canConnectToFence",
            "method_10184",
            "method_26375",
            "connectsTo(Lnet/minecraft/world/level/block/state/BlockState;ZLnet/minecraft/core/Direction;)Z",
        ],
        pre: StandardEventHook::BlockConnectQuery,
        post: StandardEventHook::BlockConnectQuery,
    },
    // Campfire smoke query: `CampfireBlock.isSmokeSource` (yarn
    // `isLitCampfire` = method_23896 — the hay-bale smoke-height check).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/block/CampfireBlock",
            "net/minecraft/class_3922",
        ],
        methods: &[
            "isSmokeSource",
            "isLitCampfire",
            "method_23896",
            "isSmokeSource(Lnet/minecraft/world/level/block/state/BlockState;)Z",
        ],
        pre: StandardEventHook::CampfireSmokeQuery,
        post: StandardEventHook::CampfireSmokeQuery,
    },
    // Plant soil query: `CropBlock.mayPlaceOn` / `StemBlock.mayPlaceOn`
    // (yarn `canPlantOnTop` = method_9695, declared on the shared
    // BushBlock/PlantBlock parent so both overrides share the number).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/block/CropBlock",
            "net/minecraft/world/level/block/StemBlock",
            "net/minecraft/class_2302",
            "net/minecraft/class_2513",
        ],
        methods: &[
            "mayPlaceOn",
            "canPlantOnTop",
            "method_9695",
            "mayPlaceOn(Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/world/level/BlockGetter;Lnet/minecraft/core/BlockPos;)Z",
        ],
        pre: StandardEventHook::PlantPlaceQuery,
        post: StandardEventHook::PlantPlaceQuery,
    },
    // Fluid spread query: `FlowingFluid.canPassThrough` (yarn
    // `canFlowThrough` = method_15746).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/material/FlowingFluid",
            "net/minecraft/class_3609",
        ],
        methods: &[
            "canPassThrough",
            "canFlowThrough",
            "method_15746",
            "canPassThrough(Lnet/minecraft/world/level/BlockGetter;Lnet/minecraft/world/level/material/Fluid;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/core/Direction;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/world/level/material/FluidState;)Z",
        ],
        pre: StandardEventHook::FluidPassQuery,
        post: StandardEventHook::FluidPassQuery,
    },
    // Scoreboard team change: `ServerScoreboard.addPlayerToTeam` /
    // `removePlayerFromTeam` (inherited from `Scoreboard`; yarn
    // `addScoreHolderToTeam` / `removeScoreHolderFromTeam` =
    // method_1172 / method_1157).
    StandardTarget {
        classes: &[
            "net/minecraft/server/ServerScoreboard",
            "net/minecraft/class_2995",
        ],
        methods: &[
            "addPlayerToTeam",
            "removePlayerFromTeam",
            "addScoreHolderToTeam",
            "removeScoreHolderFromTeam",
            "method_1172",
            "method_1157",
            "addPlayerToTeam(Ljava/lang/String;Lnet/minecraft/world/scores/PlayerTeam;)V",
            "removePlayerFromTeam(Ljava/lang/String;Lnet/minecraft/world/scores/PlayerTeam;)V",
        ],
        pre: StandardEventHook::ScoreboardTeamChange,
        post: StandardEventHook::ScoreboardTeamChange,
    },
    // Resource reload: `MinecraftServer.reloadResources` (method_29439;
    // `MinecraftServer` keeps its own name under intermediary) and
    // `ReloadableServerResources.loadResources` (yarn `DataPackContents`
    // `reload` = class_5350.method_29466).
    StandardTarget {
        classes: &[
            "net/minecraft/server/MinecraftServer",
            "net/minecraft/server/ReloadableServerResources",
            "net/minecraft/class_5350",
        ],
        methods: &[
            "reloadResources",
            "loadResources",
            "reload",
            "method_29439",
            "method_29466",
            "reloadResources(Ljava/util/Collection;)Ljava/util/concurrent/CompletableFuture;",
            "loadResources(Lnet/minecraft/server/packs/resources/ResourceManager;Lnet/minecraft/core/LayeredRegistryAccess;Lnet/minecraft/world/flag/FeatureFlagSet;Lnet/minecraft/commands/Commands$CommandSelection;ILjava/util/concurrent/Executor;Ljava/util/concurrent/Executor;)Ljava/util/concurrent/CompletableFuture;",
        ],
        pre: StandardEventHook::ResourceReload,
        post: StandardEventHook::ResourceReload,
    },
    // Map data sync: `MapItemSavedData.load` / `save` / `checkBanners`
    // (yarn `MapState` `fromNbt` = method_32371; `save` is inherited from
    // `SavedData`/`PersistentState` = method_17919).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/saveddata/maps/MapItemSavedData",
            "net/minecraft/class_22",
        ],
        methods: &[
            "load",
            "save",
            "checkBanners",
            "fromNbt",
            "method_32371",
            "method_17919",
            "load(Lnet/minecraft/nbt/CompoundTag;Lnet/minecraft/core/HolderLookup$Provider;)Lnet/minecraft/world/level/saveddata/maps/MapItemSavedData;",
            "save(Lnet/minecraft/nbt/CompoundTag;Lnet/minecraft/core/HolderLookup$Provider;)V",
            "checkBanners(Lnet/minecraft/world/level/BlockGetter;II)V",
        ],
        pre: StandardEventHook::MapDataSync,
        post: StandardEventHook::MapDataSync,
    },
    // Tree soil query: `Feature.isGrassOrDirt` (yarn `isSoil` =
    // method_23396 / method_27368 — both overloads) and
    // `TrunkPlacer.setDirtAt` (yarn `setToDirt` = method_27400).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/levelgen/feature/Feature",
            "net/minecraft/world/level/levelgen/feature/trunkplacers/TrunkPlacer",
            "net/minecraft/class_3031",
            "net/minecraft/class_5141",
        ],
        methods: &[
            "isGrassOrDirt",
            "isSoil",
            "setDirtAt",
            "setToDirt",
            "method_23396",
            "method_27368",
            "method_27400",
        ],
        pre: StandardEventHook::TreeSoilQuery,
        post: StandardEventHook::TreeSoilQuery,
    },
    // Loot bonus-count roll: `EnchantedCountIncreaseFunction.run` —
    // Mojmap name of yarn `EnchantedCountIncreaseLootFunction` (class_125),
    // whose `run`/`process` (method_522) applies the count bonus.
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/storage/loot/functions/EnchantedCountIncreaseFunction",
            "net/minecraft/loot/function/EnchantedCountIncreaseLootFunction",
            "net/minecraft/class_125",
        ],
        methods: &[
            "run",
            "process",
            "method_522",
            "run(Lnet/minecraft/world/item/ItemStack;Lnet/minecraft/world/level/storage/loot/LootContext;)Lnet/minecraft/world/item/ItemStack;",
        ],
        pre: StandardEventHook::LootBonusRoll,
        post: StandardEventHook::LootBonusRoll,
    },
    // Structure piece placement: `SinglePoolElement.place` (yarn
    // `StructurePoolElement.generate` = method_16626 on the shared parent;
    // intermediary numbers are hierarchy-shared so the override on
    // SinglePoolElement class_3781 carries the same number).
    StandardTarget {
        classes: &[
            "net/minecraft/world/level/levelgen/structure/pools/SinglePoolElement",
            "net/minecraft/structure/pool/SinglePoolElement",
            "net/minecraft/class_3781",
        ],
        methods: &[
            "place",
            "generate",
            "method_16626",
            "place(Lnet/minecraft/world/level/levelgen/structure/templatesystem/StructureTemplateManager;Lnet/minecraft/world/level/WorldGenLevel;Lnet/minecraft/world/level/StructureManager;Lnet/minecraft/world/level/chunk/ChunkGenerator;Lnet/minecraft/core/BlockPos;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/Rotation;Lnet/minecraft/world/level/levelgen/structure/BoundingBox;Lnet/minecraft/util/RandomSource;Lnet/minecraft/world/level/levelgen/structure/templatesystem/LiquidSettings;Z)Z",
        ],
        pre: StandardEventHook::StructurePiecePlace,
        post: StandardEventHook::StructurePiecePlace,
    },
];

/// Normalize a `target_class` to `a/b/C` internal form: strips an optional
/// `L...;` descriptor wrapper and maps `.` to `/` so Mojang dot names, slash
/// names, descriptors, and Intermediary names all compare equal.
fn normalize_class(name: &str) -> String {
    let mut normalized = name.trim();
    if let Some(inner) = normalized
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
    {
        normalized = inner;
    }
    normalized.replace('.', "/")
}

/// Where a resolved injection point lands relative to the vanilla body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pre,
    Post,
}

/// Look up `(target_class, target_method)` in [`STANDARD_TARGETS`].
fn lookup_target(target_class: &str, target_method: &str) -> Option<&'static StandardTarget> {
    let class = normalize_class(target_class);
    let method = target_method.trim();
    STANDARD_TARGETS
        .iter()
        .find(|t| t.classes.contains(&class.as_str()) && t.methods.contains(&method))
}

/// The canonical hooks whose verified vanilla targets *return a value*:
/// `Item.useOn` (`InteractionResult`), `Player.canEat`,
/// `FenceBlock.connectsTo`, `CampfireBlock.isSmokeSource`,
/// `CropBlock`/`StemBlock.mayPlaceOn`, `FlowingFluid.canPassThrough`
/// (`Z`), and `MinecraftServer.reloadResources` /
/// `ReloadableServerResources.loadResources` (`CompletableFuture`). A
/// `cancellable = true` `TAIL`/`RETURN` site on one of these is a
/// standard `CallbackInfoReturnable.setReturnValue` override and lifts
/// as [`LiftedHookKind::ReturnValueOverride`]; a cancellable post hook
/// on anything else (every `V` target) still fails closed as
/// [`UnliftableReason::NonCancellablePostHook`].
const RETURN_VALUE_HOOKS: &[StandardEventHook] = &[
    StandardEventHook::ItemUseOnBlock,
    StandardEventHook::PlayerCanEatQuery,
    StandardEventHook::BlockConnectQuery,
    StandardEventHook::CampfireSmokeQuery,
    StandardEventHook::PlantPlaceQuery,
    StandardEventHook::FluidPassQuery,
    StandardEventHook::ResourceReload,
];

/// Lift one site to a standard event binding, or fail closed with the exact
/// reason. Check order: annotation → locals capture → injection point →
/// target table → post-hook cancellability. The `(target_class,
/// target_method)` pair is first normalized through `mappings`
/// (Intermediary → canonical `named` resolution, including the built-in
/// hook anchors), so a Fabric Intermediary name and its NeoForge Mojmap
/// equivalent judge against the *same* `STANDARD_TARGETS` row.
fn lift_site(
    site: &MixinSite,
    mappings: &TinyV2MappingTable,
) -> Result<LiftedBinding, UnliftableReason> {
    if site.annotation != MixinAnnotationKind::Inject {
        return Err(UnliftableReason::UnsupportedAnnotation);
    }
    if site.captures_locals {
        return Err(UnliftableReason::CapturesLocals);
    }
    let phase = match site.at {
        Some(InjectPoint::Head) => Phase::Pre,
        Some(InjectPoint::Tail) | Some(InjectPoint::Return) => Phase::Post,
        _ => return Err(UnliftableReason::UnsupportedInjectPoint),
    };
    let (canonical_class, canonical_method) =
        mappings.canonical_target(&site.target_class, &site.target_method);
    let target =
        lookup_target(&canonical_class, canonical_method).ok_or(UnliftableReason::UnknownTarget)?;
    let hook = match phase {
        Phase::Pre => target.pre,
        Phase::Post => target.post,
    };
    // A `TAIL`/`RETURN` handler runs after the vanilla work, so cancelling
    // it could never take the action back — unless the verified target
    // returns a value, where `setReturnValue` is a standard override the
    // hook can reproduce. Every other post cancellation still fails closed.
    let mut kind = LiftedHookKind::Standard;
    if site.cancellable && phase == Phase::Post {
        if RETURN_VALUE_HOOKS.contains(&hook) {
            kind = LiftedHookKind::ReturnValueOverride;
        } else {
            return Err(UnliftableReason::NonCancellablePostHook);
        }
    }
    Ok(LiftedBinding {
        site: site.clone(),
        hook,
        cancellable: site.cancellable,
        kind,
    })
}

/// Lift a mod's complete extracted Mixin surface to standard event hooks.
///
/// Returns [`MixinLiftVerdict::FullyLiftable`] only when *every* site maps
/// cleanly; an empty `sites` slice yields `FullyLiftable(vec![])`. A single
/// unliftable site flips the verdict to
/// [`MixinLiftVerdict::RequiresFallback`], preserving both the lifted
/// bindings and every rejected site with its reason for diagnostics.
///
/// Target names resolve through the built-in canonical Intermediary→Mojmap
/// hook anchors (`TinyV2MappingTable::default()`), so Fabric Intermediary
/// sites lift to the exact same hooks as NeoForge Mojmap sites even when no
/// `.tiny` document was supplied.
pub fn lift_mixin_sites(sites: &[MixinSite]) -> MixinLiftVerdict {
    lift_mixin_sites_with_mappings(sites, &TinyV2MappingTable::default())
}

/// [`lift_mixin_sites`] with an explicit Intermediary→canonical mapping
/// table merged in (e.g. a `*.tiny` document found inside the mod JAR).
/// The table only *renames* lookup keys — the fail-closed
/// `STANDARD_TARGETS` table still decides every hook, so mappings can
/// never promote an otherwise unmapped target.
pub fn lift_mixin_sites_with_mappings(
    sites: &[MixinSite],
    mappings: &TinyV2MappingTable,
) -> MixinLiftVerdict {
    let mut lifted = Vec::with_capacity(sites.len());
    let mut unliftable = Vec::new();
    for site in sites {
        match lift_site(site, mappings) {
            Ok(binding) => lifted.push(binding),
            Err(reason) => unliftable.push((site.clone(), reason)),
        }
    }
    if unliftable.is_empty() {
        MixinLiftVerdict::FullyLiftable(lifted)
    } else {
        MixinLiftVerdict::RequiresFallback { lifted, unliftable }
    }
}

fn describe_site(site: &MixinSite) -> String {
    let at = site
        .at
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<none>".to_string());
    format!(
        "{} -> {}#{} {} @At({})",
        site.mixin_class, site.target_class, site.target_method, site.annotation, at
    )
}

// ---------------------------------------------------------------------------
// Real NeoForge 1.21.1 / Fabric API 1.21.1 event descriptors
// ---------------------------------------------------------------------------

/// One row of the real-loader event table: normalized internal names of a
/// NeoForge 1.21.1 event or Fabric API 1.21.1 callback bound to the
/// [`StandardEventHook`] it lowers onto.
struct EventDescriptorRow {
    classes: &'static [&'static str],
    hook: StandardEventHook,
}

/// Real NeoForge 1.21.1 `net.neoforged.neoforge.event.*` events and Fabric
/// API 1.21.1 `net.fabricmc.fabric.api.event.*` callbacks, mapped to the
/// standard hook a subscription lowers onto. Legacy `net.minecraftforge`
/// damage events are kept for parity with the bounded VM's host-call
/// allowlist. Names are normalized internal names (slash form) — the
/// dotted, `L...;` descriptor, and full method-descriptor spellings all
/// resolve through [`event_descriptor_hook`].
///
/// Clean-room note: like [`STANDARD_TARGETS`], this is factual mapping data
/// (public API class names) only.
const EVENT_DESCRIPTOR_HOOKS: &[EventDescriptorRow] = &[
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/level/BlockEvent$BreakEvent",
            "net/fabricmc/fabric/api/event/player/PlayerBlockBreakEvents$Before",
        ],
        hook: StandardEventHook::BlockBreakPre,
    },
    EventDescriptorRow {
        classes: &["net/fabricmc/fabric/api/event/player/PlayerBlockBreakEvents$After"],
        hook: StandardEventHook::BlockBreakPost,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/level/BlockEvent$EntityPlaceEvent",
            "net/neoforged/neoforge/event/level/BlockEvent$EntityMultiPlaceEvent",
        ],
        hook: StandardEventHook::BlockPlacePre,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/entity/player/PlayerInteractEvent$RightClickBlock",
            "net/fabricmc/fabric/api/event/player/UseBlockCallback",
        ],
        hook: StandardEventHook::BlockInteract,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/entity/player/PlayerInteractEvent$RightClickItem",
            "net/fabricmc/fabric/api/event/player/UseItemCallback",
        ],
        hook: StandardEventHook::ItemUse,
    },
    EventDescriptorRow {
        classes: &["net/fabricmc/fabric/api/event/player/AttackEntityCallback"],
        hook: StandardEventHook::EntityAttack,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/entity/living/LivingDamageEvent",
            "net/neoforged/neoforge/event/entity/living/LivingDamageEvent$Pre",
            "net/minecraftforge/event/entity/living/LivingDamageEvent",
            "net/minecraftforge/event/entity/living/LivingHurtEvent",
        ],
        hook: StandardEventHook::EntityDamagePre,
    },
    EventDescriptorRow {
        classes: &["net/neoforged/neoforge/event/entity/living/LivingDamageEvent$Post"],
        hook: StandardEventHook::EntityDamagePost,
    },
    EventDescriptorRow {
        classes: &["net/neoforged/neoforge/event/entity/living/LivingDeathEvent"],
        hook: StandardEventHook::EntityDeath,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/tick/ServerTickEvent$Pre",
            "net/fabricmc/fabric/api/event/lifecycle/ServerTickEvents$StartTick",
        ],
        hook: StandardEventHook::ServerTickPre,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/tick/ServerTickEvent$Post",
            "net/fabricmc/fabric/api/event/lifecycle/ServerTickEvents$EndTick",
        ],
        hook: StandardEventHook::ServerTickPost,
    },
    EventDescriptorRow {
        classes: &["net/neoforged/neoforge/event/entity/EntityJoinLevelEvent"],
        hook: StandardEventHook::EntityJoinLevel,
    },
    EventDescriptorRow {
        classes: &[
            "net/neoforged/neoforge/event/RegisterCommandsEvent",
            "net/fabricmc/fabric/api/command/v2/CommandRegistrationCallback",
        ],
        hook: StandardEventHook::CommandRegister,
    },
];

/// Normalize one event/callback spelling to an internal name: bare
/// internal (`net/neoforged/.../BreakEvent`), dotted
/// (`net.neoforged...BreakEvent`), class descriptor (`Lnet/.../X;`), or a
/// full method descriptor whose first reference parameter is the event
/// (`(Lnet/.../BreakEvent;)V` → `net/.../BreakEvent`). Primitive or
/// parameterless forms yield `None` — fail-closed.
fn normalize_event_descriptor(descriptor: &str) -> Option<String> {
    let descriptor = descriptor.trim();
    if let Some(rest) = descriptor.strip_prefix('(') {
        // Method descriptor: the hookable form is a single leading
        // reference parameter — `(Lpkg/Class;...)`.
        let rest = rest.strip_prefix('L')?;
        let end = rest.find(';')?;
        return Some(rest[..end].to_string());
    }
    if let Some(inner) = descriptor
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
    {
        return Some(inner.to_string());
    }
    if descriptor.is_empty() {
        return None;
    }
    Some(descriptor.replace('.', "/"))
}

/// Lower a real NeoForge 1.21.1 or Fabric API 1.21.1 event/callback JVM
/// descriptor to its [`StandardEventHook`]. Accepts a bare internal name,
/// a `L...;` descriptor, a dotted name, or a full method descriptor (the
/// first `L...;` parameter is taken as the event). Anything outside the
/// table returns `None` — fail-closed, never a guessed hook.
pub fn event_descriptor_hook(descriptor: &str) -> Option<StandardEventHook> {
    let internal = normalize_event_descriptor(descriptor)?;
    EVENT_DESCRIPTOR_HOOKS
        .iter()
        .find(|row| row.classes.contains(&internal.as_str()))
        .map(|row| row.hook)
}

// ---------------------------------------------------------------------------
// Safe read-only `@Accessor` lifting
// ---------------------------------------------------------------------------

/// Why one `@Accessor` site cannot lift natively — the structured reason
/// the shim plan and the adaptation report carry so the rejection is
/// never a silent drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessorRejection {
    /// A `set*`-named method or a `(T)V` signature — mutating field writes
    /// never lift onto a native read-only plan.
    MutatingSetter,
    /// The site is not an `@Accessor` method at all.
    NotAnAccessor,
    /// The handler shape does not prove a read-only getter — missing
    /// `get*`/`is*` prefix, a `()V`/parametered signature, or a missing
    /// descriptor.
    OpaqueSignature,
    /// `(target class, field)` is outside the verified safe read-only
    /// table — unknown private fields stay unliftable.
    UnknownField,
}

impl AccessorRejection {
    /// Stable operator-facing reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MutatingSetter => {
                "mutating @Accessor setter — field writes never lift natively"
            }
            Self::NotAnAccessor => "not an @Accessor site",
            Self::OpaqueSignature => {
                "handler is not a provably read-only getter (expected a `get*`/`is*` name and a `()T` signature)"
            }
            Self::UnknownField => {
                "field is outside the verified safe read-only table"
            }
        }
    }
}

impl fmt::Display for AccessorRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The value kind a lifted read-only field access produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessorValueKind {
    Int,
    Float,
    /// An opaque object reference (`block`, `gameMode`, ...).
    Ref,
}

/// One read-only `@Accessor` getter proven to read a known-safe vanilla
/// field natively — no JVM needed for the field read itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiftedAccessorBinding {
    /// The accessor site this binding covers.
    pub site: MixinSite,
    /// Canonical `Class#field` display (`Entity#yRot`).
    pub field: String,
    /// Kind of value the read produces.
    pub value_kind: AccessorValueKind,
    /// Execution tier the binding satisfies — a pure native field read.
    pub tier: crate::fallback::ExecutionTier,
}

/// The deterministic plan [`lift_accessor_sites`] produces: every lifted
/// read-only getter binding plus every rejected site with its structured
/// reason. Callers fold [`Self::tier`] into the mod's execution plan —
/// fully lifted means `UniversalNative` for the accessor surface;
/// anything rejected forces `RequiresFallback` for it.
#[derive(Clone, Debug, Default)]
pub struct LiftedAccessorPlan {
    /// Proven read-only getter bindings, in site order.
    pub bindings: Vec<LiftedAccessorBinding>,
    /// Sites that stay unliftable, each with its structured reason.
    pub rejected: Vec<(MixinSite, AccessorRejection)>,
}

impl LiftedAccessorPlan {
    /// `true` when every presented `@Accessor` site lifted.
    pub fn is_fully_lifted(&self) -> bool {
        self.rejected.is_empty()
    }

    /// The execution tier the accessor surface satisfies: a fully lifted
    /// non-empty plan needs no JVM (`UniversalNative`); anything rejected
    /// forces the accessor surface to `RequiresFallback`.
    pub fn tier(&self) -> crate::fallback::ExecutionTier {
        if self.rejected.is_empty() && !self.bindings.is_empty() {
            crate::fallback::ExecutionTier::UniversalNative
        } else {
            crate::fallback::ExecutionTier::RequiresFallback
        }
    }
}

/// One verified safe read-only vanilla field an `@Accessor` getter may
/// lift onto — a row of (normalized class names, accepted field names,
/// canonical `Class#field`, value kind). Names are factual mapping data
/// only.
struct SafeAccessorField {
    classes: &'static [&'static str],
    fields: &'static [&'static str],
    canonical: &'static str,
    kind: AccessorValueKind,
}

/// The verified safe read-only field table. Each row names a vanilla
/// field that is a plain stored value (never a derived or mutating view)
/// safe for a native read proxy: entity pose/flags, held-item count,
/// block-state owner, health, and the player game-mode. Mojang and Fabric
/// Intermediary class names are accepted; the field name itself is the
/// Mojang name mods actually write in `@Accessor("yRot")`.
const SAFE_ACCESSOR_FIELDS: &[SafeAccessorField] = &[
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/entity/Entity",
            "net/minecraft/class_1297",
        ],
        fields: &["yRot"],
        canonical: "Entity#yRot",
        kind: AccessorValueKind::Float,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/entity/Entity",
            "net/minecraft/class_1297",
        ],
        fields: &["xRot"],
        canonical: "Entity#xRot",
        kind: AccessorValueKind::Float,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/entity/Entity",
            "net/minecraft/class_1297",
        ],
        fields: &["onGround"],
        canonical: "Entity#onGround",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/entity/LivingEntity",
            "net/minecraft/class_1309",
        ],
        fields: &["health"],
        canonical: "LivingEntity#health",
        kind: AccessorValueKind::Float,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/level/block/state/BlockState",
            "net/minecraft/world/level/block/state/BlockBehaviour$BlockStateBase",
            "net/minecraft/class_2680",
        ],
        fields: &["block"],
        canonical: "BlockState#block",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/item/ItemStack",
            "net/minecraft/class_1799",
        ],
        fields: &["count"],
        canonical: "ItemStack#count",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/server/level/ServerPlayer",
            "net/minecraft/class_3222",
        ],
        fields: &["gameMode"],
        canonical: "ServerPlayer#gameMode",
        kind: AccessorValueKind::Ref,
    },
    // -------------------------------------------------------------------
    // Additional plain stored fields real production mods read through
    // read-only accessors. Every row is a stored field — never a derived
    // or mutating view — so the native read proxy is exact. `set*`
    // handlers on any of these still fail closed as `MutatingSetter`.
    // -------------------------------------------------------------------
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/level/block/ConcretePowderBlock",
            "net/minecraft/class_2292",
        ],
        fields: &["concrete"],
        canonical: "ConcretePowderBlock#concrete",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/inventory/Slot",
            "net/minecraft/class_1735",
        ],
        fields: &["x"],
        canonical: "Slot#x",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/inventory/Slot",
            "net/minecraft/class_1735",
        ],
        fields: &["y"],
        canonical: "Slot#y",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/entity/vehicle/MinecartFurnace",
            "net/minecraft/class_1696",
        ],
        fields: &["fuel"],
        canonical: "MinecartFurnace#fuel",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/entity/item/ItemEntity",
            "net/minecraft/class_1542",
        ],
        fields: &["health"],
        canonical: "ItemEntity#health",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/effect/MobEffectInstance",
            "net/minecraft/class_1293",
        ],
        fields: &["hiddenEffect"],
        canonical: "MobEffectInstance#hiddenEffect",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/nbt/NbtAccounter",
            "net/minecraft/class_2505",
        ],
        fields: &["usage"],
        canonical: "NbtAccounter#usage",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/item/alchemy/PotionBrewing",
            "net/minecraft/class_1845",
        ],
        fields: &["potionMixes"],
        canonical: "PotionBrewing#potionMixes",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/item/alchemy/PotionBrewing",
            "net/minecraft/class_1845",
        ],
        fields: &["containerMixes"],
        canonical: "PotionBrewing#containerMixes",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/core/MappedRegistry",
            "net/minecraft/class_2370",
        ],
        fields: &["toId"],
        canonical: "MappedRegistry#toId",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/core/MappedRegistry",
            "net/minecraft/class_2370",
        ],
        fields: &["byValue"],
        canonical: "MappedRegistry#byValue",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/server/level/ServerLevel",
            "net/minecraft/class_3218",
        ],
        fields: &["entityTickList"],
        canonical: "ServerLevel#entityTickList",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/server/level/ChunkMap",
            "net/minecraft/class_3898",
        ],
        fields: &["entityMap"],
        canonical: "ChunkMap#entityMap",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/server/level/ChunkMap$TrackedEntity",
            "net/minecraft/class_3898$class_3208",
        ],
        fields: &["seenBy"],
        canonical: "ChunkMap$TrackedEntity#seenBy",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/core/dispenser/ProjectileDispenseBehavior",
            "net/minecraft/class_2965",
        ],
        fields: &["projectileItem"],
        canonical: "ProjectileDispenseBehavior#projectileItem",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/core/dispenser/ProjectileDispenseBehavior",
            "net/minecraft/class_2965",
        ],
        fields: &["dispenseConfig"],
        canonical: "ProjectileDispenseBehavior#dispenseConfig",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/item/crafting/ShapedRecipe",
            "net/minecraft/class_1869",
        ],
        fields: &["pattern"],
        canonical: "ShapedRecipe#pattern",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/gametest/framework/GameTestHelper",
            "net/minecraft/class_4516",
        ],
        fields: &["testInfo"],
        canonical: "GameTestHelper#testInfo",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/gametest/framework/GameTestHelper",
            "net/minecraft/class_4516",
        ],
        fields: &["finalCheckAdded"],
        canonical: "GameTestHelper#finalCheckAdded",
        kind: AccessorValueKind::Int,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/data/models/ItemModelGenerators",
            "net/minecraft/class_4915",
        ],
        fields: &["GENERATED_TRIM_MODELS"],
        canonical: "ItemModelGenerators#GENERATED_TRIM_MODELS",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/level/levelgen/structure/pools/SinglePoolElement",
            "net/minecraft/class_3781",
        ],
        fields: &["template"],
        canonical: "SinglePoolElement#template",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/level/levelgen/structure/pools/StructureTemplatePool",
            "net/minecraft/class_3785",
        ],
        fields: &["rawTemplates"],
        canonical: "StructureTemplatePool#rawTemplates",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/level/levelgen/structure/pools/StructureTemplatePool",
            "net/minecraft/class_3785",
        ],
        fields: &["templates"],
        canonical: "StructureTemplatePool#templates",
        kind: AccessorValueKind::Ref,
    },
    // -------------------------------------------------------------------
    // Diagnostic / state-owner / NeoForge registry plain stored fields.
    // `SystemReport` (yarn `net/minecraft/util/SystemDetails`, class_6396)
    // stores crash-report constants; `StateHolder` (yarn `net/minecraft/
    // state/State`, class_2688) stores the block/item `owner`; the two
    // `net/neoforged/` rows are NeoForge's fluid-interaction registry and
    // item-handler backing list. `set*` handlers on any of these still
    // fail closed as `MutatingSetter`.
    // -------------------------------------------------------------------
    SafeAccessorField {
        classes: &["net/minecraft/SystemReport", "net/minecraft/class_6396"],
        fields: &["OPERATING_SYSTEM"],
        canonical: "SystemReport#OPERATING_SYSTEM",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &["net/minecraft/SystemReport", "net/minecraft/class_6396"],
        fields: &["JAVA_VERSION"],
        canonical: "SystemReport#JAVA_VERSION",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &["net/minecraft/SystemReport", "net/minecraft/class_6396"],
        fields: &["entries"],
        canonical: "SystemReport#entries",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &[
            "net/minecraft/world/level/block/state/StateHolder",
            "net/minecraft/class_2688",
        ],
        fields: &["owner"],
        canonical: "StateHolder#owner",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &["net/neoforged/neoforge/fluids/FluidInteractionRegistry"],
        fields: &["INTERACTIONS"],
        canonical: "FluidInteractionRegistry#INTERACTIONS",
        kind: AccessorValueKind::Ref,
    },
    SafeAccessorField {
        classes: &["net/neoforged/neoforge/items/ItemStackHandler"],
        fields: &["stacks"],
        canonical: "ItemStackHandler#stacks",
        kind: AccessorValueKind::Ref,
    },
];

/// Mixin's member-name prefix convention: real mods namespace their
/// generated accessor/invoker handlers as `<modid>$<name>`
/// (`create$getConcrete()`, `my_mod$isActive()`). The `<ident>$` portion
/// is a uniqueness prefix for the mixin — not part of the referenced
/// member name — so it is stripped before the handler shape is judged.
/// Only a well-formed leading identifier is stripped; anything else
/// (`$getX`, `9foo$getX`, a bare name) is returned unchanged and judged
/// on its own merits — fail closed.
fn strip_mixin_member_prefix(name: &str) -> &str {
    let Some((prefix, rest)) = name.split_once('$') else {
        return name;
    };
    let valid = !prefix.is_empty()
        && prefix
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !rest.is_empty();
    if valid {
        rest
    } else {
        name
    }
}

/// Mixin's own field-name inference for an `@Accessor` without a `value`
/// element: strip the `get`/`is`/`set` prefix and lowercase the first
/// remaining character (`getYRot` → `yRot`, `isOnGround` → `onGround`).
/// Real mods also name constant-style fields with an all-uppercase
/// remainder (`getOPERATING_SYSTEM` → `OPERATING_SYSTEM` on
/// `SystemReport`), so the verbatim remainder is a second candidate;
/// both are still judged against the fail-closed allowlist table, never
/// a guessed field.
fn infer_accessor_field_names(handler_name: &str) -> Vec<String> {
    let Some(stripped) = handler_name
        .strip_prefix("get")
        .or_else(|| handler_name.strip_prefix("is"))
        .or_else(|| handler_name.strip_prefix("set"))
    else {
        return Vec::new();
    };
    let mut chars = stripped.chars();
    let Some(first) = chars.next() else {
        return Vec::new();
    };
    let mut names = vec![first.to_lowercase().chain(chars).collect::<String>()];
    if names[0] != stripped {
        names.push(stripped.to_string());
    }
    names
}

/// Classify one `@Accessor` site: a `get*`/`is*` handler with a `()T`
/// (non-void) signature on a table field lifts to a native read binding;
/// `set*`/`(T)V` shapes, opaque signatures, and unknown fields fail
/// closed with their [`AccessorRejection`].
fn lift_accessor_site(site: &MixinSite) -> Result<LiftedAccessorBinding, AccessorRejection> {
    if site.annotation != MixinAnnotationKind::Accessor {
        return Err(AccessorRejection::NotAnAccessor);
    }
    // Real mods name generated accessors `<modid>$get*` — strip the
    // namespace prefix so `create$getConcrete` is judged by its `get` +
    // `()T` getter shape exactly like `getConcrete`.
    let name = strip_mixin_member_prefix(site.handler_name.as_str());
    let descriptor = site.handler_descriptor.as_str();
    // `set` + uppercase is the Mixin setter convention (`setYRot`); a
    // lowercase continuation (`settings`) is just a name.
    if name
        .strip_prefix("set")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
    {
        return Err(AccessorRejection::MutatingSetter);
    }
    // `()T` with a non-void T is the only provably read-only shape;
    // the shortest legal form is 3 bytes (`()F`, `()I`).
    let getter_signature =
        descriptor.starts_with("()") && descriptor.len() > 2 && !descriptor.ends_with('V');
    if descriptor.ends_with("V") && !descriptor.starts_with("()") {
        // Any parametered method returning void is a write, named or not.
        return Err(AccessorRejection::MutatingSetter);
    }
    let getter_name = name.starts_with("get") || name.starts_with("is");
    if !getter_name || !getter_signature {
        return Err(AccessorRejection::OpaqueSignature);
    }
    // The field is the annotation `value` when present (the scanner then
    // reports it as `target_method`, distinct from the handler name),
    // otherwise Mixin's own name-based inference applies.
    let fields = if site.target_method != site.handler_name {
        vec![site.target_method.clone()]
    } else {
        let candidates = infer_accessor_field_names(name);
        if candidates.is_empty() {
            return Err(AccessorRejection::OpaqueSignature);
        }
        candidates
    };
    let class = normalize_class(&site.target_class);
    let row = SAFE_ACCESSOR_FIELDS
        .iter()
        .find(|r| {
            r.classes.contains(&class.as_str())
                && fields.iter().any(|f| r.fields.contains(&f.as_str()))
        })
        .ok_or(AccessorRejection::UnknownField)?;
    Ok(LiftedAccessorBinding {
        site: site.clone(),
        field: row.canonical.to_string(),
        value_kind: row.kind,
        tier: crate::fallback::ExecutionTier::UniversalNative,
    })
}

/// Lift every `@Accessor` site in `sites` into a deterministic
/// [`LiftedAccessorPlan`]. Non-`@Accessor` sites are ignored here — they
/// remain [`lift_mixin_sites`]' job; the two surfaces compose in the
/// adaptation pipeline.
pub fn lift_accessor_sites(sites: &[MixinSite]) -> LiftedAccessorPlan {
    let mut plan = LiftedAccessorPlan::default();
    for site in sites {
        if site.annotation != MixinAnnotationKind::Accessor {
            continue;
        }
        match lift_accessor_site(site) {
            Ok(binding) => plan.bindings.push(binding),
            Err(reason) => plan.rejected.push((site.clone(), reason)),
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// Safe read-only `@Invoker` lifting
// ---------------------------------------------------------------------------

/// Why one `@Invoker` site cannot lift natively — the structured reason
/// the shim plan and the adaptation report carry so the rejection is
/// never a silent drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvokerRejection {
    /// The site is not an `@Invoker` method at all.
    NotAnInvoker,
    /// `<init>`/`<clinit>` — constructors never lift natively; they
    /// allocate and initialize vanilla objects, never a pure query.
    ConstructorTarget,
    /// A known mutating vanilla method — state changes never lift onto a
    /// native read-only plan.
    MutatingTarget,
    /// No `value` element and the handler name gives no usable target
    /// method name (even after `<prefix>$` and verb-prefix stripping).
    OpaqueSignature,
    /// `(target class, method)` is outside the verified pure/read-only
    /// query table — unknown methods stay unliftable.
    UnknownMethod,
}

impl InvokerRejection {
    /// Stable operator-facing reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotAnInvoker => "not an @Invoker site",
            Self::ConstructorTarget => {
                "@Invoker targets a constructor (<init>/<clinit>) — constructors never lift natively"
            }
            Self::MutatingTarget => {
                "@Invoker targets a mutating vanilla method — state changes never lift natively"
            }
            Self::OpaqueSignature => {
                "handler gives no usable @Invoker target method name"
            }
            Self::UnknownMethod => {
                "method is outside the verified pure/read-only query table"
            }
        }
    }
}

impl fmt::Display for InvokerRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `@Invoker` site proven to call a verified pure/read-only vanilla
/// query method — the call itself lowers onto a native host query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiftedInvokerBinding {
    /// The invoker site this binding covers.
    pub site: MixinSite,
    /// Canonical `Class#method` display (`BlockBehaviour#getShape`).
    pub method: String,
    /// Execution tier the binding satisfies — a pure native method call.
    pub tier: crate::fallback::ExecutionTier,
}

/// The deterministic plan [`lift_invoker_sites`] produces, mirroring
/// [`LiftedAccessorPlan`]: lifted read-only call bindings plus every
/// rejected site with its structured reason.
#[derive(Clone, Debug, Default)]
pub struct LiftedInvokerPlan {
    /// Proven read-only query-call bindings, in site order.
    pub bindings: Vec<LiftedInvokerBinding>,
    /// Sites that stay unliftable, each with its structured reason.
    pub rejected: Vec<(MixinSite, InvokerRejection)>,
}

impl LiftedInvokerPlan {
    /// `true` when every presented `@Invoker` site lifted.
    pub fn is_fully_lifted(&self) -> bool {
        self.rejected.is_empty()
    }

    /// The execution tier the invoker surface satisfies: a fully lifted
    /// non-empty plan needs no JVM (`UniversalNative`); anything rejected
    /// forces `RequiresFallback`.
    pub fn tier(&self) -> crate::fallback::ExecutionTier {
        if self.rejected.is_empty() && !self.bindings.is_empty() {
            crate::fallback::ExecutionTier::UniversalNative
        } else {
            crate::fallback::ExecutionTier::RequiresFallback
        }
    }
}

/// One verified pure/read-only vanilla query method an `@Invoker` may
/// lift onto — (normalized class names, accepted method names, canonical
/// `Class#method` display). Names are factual mapping data only.
struct SafeInvokerMethod {
    classes: &'static [&'static str],
    methods: &'static [&'static str],
    canonical: &'static str,
}

/// The verified pure/read-only query table. Every row names a vanilla
/// method that only reads state and returns a value — it allocates no
/// object graph, mutates no world/entity state, and schedules no work.
/// Mojang names, Fabric Intermediary (`class_NNNN`/`method_NNNN`), and
/// Yarn named aliases are all accepted for the same canonical method.
/// Anything mutating (`spawnItemParticles`), a constructor (`<init>`),
/// or not listed here fails closed.
const SAFE_INVOKER_METHODS: &[SafeInvokerMethod] = &[
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/level/block/state/BlockBehaviour",
            "net/minecraft/world/level/block/state/BlockBehaviour$BlockStateBase",
            "net/minecraft/class_4970",
            "net/minecraft/class_4970$class_4971",
        ],
        methods: &["getShape", "getOutlineShape", "method_9530"],
        canonical: "BlockBehaviour#getShape",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/level/block/CropBlock",
            "net/minecraft/class_2302",
        ],
        methods: &["getAgeProperty", "method_9824"],
        canonical: "CropBlock#getAgeProperty",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/entity/decoration/ItemFrame",
            "net/minecraft/class_1533",
        ],
        methods: &[
            "getFrameItemStack",
            "getFramedItemStack",
            "getHeldItemStack",
            "method_6940",
        ],
        canonical: "ItemFrame#getFrameItemStack",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/item/alchemy/PotionBrewing",
            "net/minecraft/class_1845",
        ],
        methods: &["isContainer", "isItemRecipeIngredient", "method_8079"],
        canonical: "PotionBrewing#isContainer",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/item/context/UseOnContext",
            "net/minecraft/class_1838",
        ],
        methods: &["getHitResult", "method_30344"],
        canonical: "UseOnContext#getHitResult",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/data/loot/BlockLootSubProvider",
            "net/minecraft/class_7788",
        ],
        methods: &["hasSilkTouch", "createSilkTouchCondition", "method_60390"],
        canonical: "BlockLootSubProvider#hasSilkTouch",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/level/block/DispenserBlock",
            "net/minecraft/class_2315",
        ],
        methods: &["getDispenseMethod", "getBehaviorForItem", "method_10011"],
        canonical: "DispenserBlock#getDispenseMethod",
    },
    SafeInvokerMethod {
        classes: &[
            "net/minecraft/world/level/material/FlowingFluid",
            "net/minecraft/class_3609",
        ],
        methods: &["getNewLiquid", "getUpdatedState", "method_15727"],
        canonical: "FlowingFluid#getNewLiquid",
    },
];

/// Vanilla methods a mod may write an `@Invoker` for that are *known*
/// mutators or lifecycle/allocating calls — named here only so the
/// rejection diagnostic can say `MutatingTarget` instead of the generic
/// `UnknownMethod`. The fail-closed outcome is identical either way; an
/// unlisted mutator simply reports `UnknownMethod`.
const KNOWN_MUTATING_INVOKER_NAMES: &[&str] = &[
    "spawnItemParticles",
    "discard",
    "die",
    "kill",
    "hurt",
    "heal",
    "moveTo",
    "setPos",
    "teleport",
    "teleportTo",
    "setDeltaMovement",
    "push",
    "setRot",
    "setYRot",
    "setXRot",
    "setOnGround",
    "setAirSupply",
    "setTicksFrozen",
    "setSecondsOnFire",
    "igniteForSeconds",
    "extinguishFire",
    "setInvisible",
    "setSilent",
    "setNoGravity",
    "setInvulnerable",
    "setGlowingTag",
    "setCustomName",
    "startRiding",
    "stopRiding",
    "dismount",
    "ejectPassengers",
    "setBaby",
    "setAggressive",
    "setTarget",
    "tame",
    "setSitting",
    "setItemSlot",
    "setItemInHand",
    "removeAllEffects",
    "addEffect",
    "setFuel",
    "setPlacedBy",
    "setBlock",
    "spawnAnim",
    "broadcastDamageEvent",
];

/// `true` for method names that are mutating by construction: the `set*`
/// convention every field writer follows, plus [`KNOWN_MUTATING_INVOKER_NAMES`].
fn is_known_mutating_invoker_name(method: &str) -> bool {
    if method
        .strip_prefix("set")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
    {
        return true;
    }
    KNOWN_MUTATING_INVOKER_NAMES.contains(&method)
}

/// Mixin's own `@Invoker` target-name inference when the annotation has
/// no `value`: strip the `<prefix>$` namespace, then the leading invoker
/// verb (`call`, `invoke`, `new`, `create`, `spawn`, `make` followed by
/// an uppercase letter) and lowercase the first remaining character —
/// `callGetShape` → `getShape`. A handler that is already a plain name
/// (`getShape`) is its own target name.
fn infer_invoker_target_name(handler_name: &str) -> Option<String> {
    let name = strip_mixin_member_prefix(handler_name);
    for verb in ["call", "invoke", "new", "create", "spawn", "make"] {
        if let Some(rest) = name.strip_prefix(verb) {
            let mut chars = rest.chars();
            if let Some(first) = chars.next() {
                if first.is_uppercase() {
                    return Some(first.to_lowercase().chain(chars).collect());
                }
            }
        }
    }
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Classify one `@Invoker` site: the resolved target method (explicit
/// `value`, else the handler's inferred name) is checked against the
/// verified pure/read-only table after `<init>`/mutating fail-closed
/// gates. Constructor, mutating, opaque, and unknown targets all fail
/// closed with their [`InvokerRejection`].
fn lift_invoker_site(
    site: &MixinSite,
    mappings: &TinyV2MappingTable,
) -> Result<LiftedInvokerBinding, InvokerRejection> {
    if site.annotation != MixinAnnotationKind::Invoker {
        return Err(InvokerRejection::NotAnInvoker);
    }
    // The `@Invoker` `value` names the target method (the scanner then
    // reports it as `target_method`); without one, Mixin infers the
    // target name from the handler's own name.
    let target_method = if site.target_method != site.handler_name {
        site.target_method.clone()
    } else {
        match infer_invoker_target_name(site.handler_name.as_str()) {
            Some(name) => name,
            None => return Err(InvokerRejection::OpaqueSignature),
        }
    };
    if target_method == "<init>" || target_method == "<clinit>" {
        return Err(InvokerRejection::ConstructorTarget);
    }
    if is_known_mutating_invoker_name(&target_method) {
        return Err(InvokerRejection::MutatingTarget);
    }
    let (canonical_class, canonical_method) =
        mappings.canonical_target(&site.target_class, &target_method);
    let class = normalize_class(canonical_class.as_ref());
    let row = SAFE_INVOKER_METHODS
        .iter()
        .find(|r| {
            r.classes.contains(&class.as_str()) && r.methods.contains(&canonical_method)
        })
        .ok_or(InvokerRejection::UnknownMethod)?;
    Ok(LiftedInvokerBinding {
        site: site.clone(),
        method: row.canonical.to_string(),
        tier: crate::fallback::ExecutionTier::UniversalNative,
    })
}

/// Lift every `@Invoker` site in `sites` into a deterministic
/// [`LiftedInvokerPlan`] using the built-in canonical hook anchors.
/// Non-`@Invoker` sites are ignored — they remain
/// [`lift_mixin_sites`]/[`lift_accessor_sites`]' job.
pub fn lift_invoker_sites(sites: &[MixinSite]) -> LiftedInvokerPlan {
    lift_invoker_sites_with_mappings(sites, &TinyV2MappingTable::default())
}

/// [`lift_invoker_sites`] with an explicit Intermediary→canonical mapping
/// table merged in (e.g. a `*.tiny` document found inside the mod JAR).
/// The table only *renames* lookup keys — the fail-closed
/// `SAFE_INVOKER_METHODS` table still decides every outcome.
pub fn lift_invoker_sites_with_mappings(
    sites: &[MixinSite],
    mappings: &TinyV2MappingTable,
) -> LiftedInvokerPlan {
    let mut plan = LiftedInvokerPlan::default();
    for site in sites {
        if site.annotation != MixinAnnotationKind::Invoker {
            continue;
        }
        match lift_invoker_site(site, mappings) {
            Ok(binding) => plan.bindings.push(binding),
            Err(reason) => plan.rejected.push((site.clone(), reason)),
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// Legacy datafix-only mixin targets
// ---------------------------------------------------------------------------

/// Where a mixin `target_class` executes relative to live 1.21.1
/// gameplay, for lift gating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixinTargetScope {
    /// A normal live-gameplay vanilla class — standard lift rules apply.
    LiveGameplay,
    /// A `net/minecraft/util/datafix/*` (yarn `net/minecraft/datafixer/*`)
    /// class. These run exclusively inside DataFixerUpper migrations when
    /// a pre-1.21.1 saved world is upgraded — never during live 1.21.1
    /// gameplay — so a mixin on one is recorded as
    /// `legacy_datafix_skipped`, not a Tier-3 blocker.
    LegacyDataFixOnly,
    /// A `net/minecraft/gametest/*` (yarn `net/minecraft/test/*`) class —
    /// the GameTest harness that only executes when a test run is
    /// explicitly invoked, never during live gameplay. A mixin on one is
    /// recorded as `gametest_skipped`, not a Tier-3 blocker.
    GameTestOnly,
}

/// Class-name prefixes (normalized internal form) whose members only run
/// during legacy save upgrades: the Mojmap `net/minecraft/util/datafix/`
/// tree and the identical yarn `net/minecraft/datafixer/` tree.
const LEGACY_DATAFIX_PREFIXES: &[&str] = &[
    "net/minecraft/util/datafix/",
    "net/minecraft/datafixer/",
];

/// Class-name prefixes (normalized internal form) whose members only run
/// inside the GameTest harness: the Mojmap `net/minecraft/gametest/`
/// tree and the identical yarn `net/minecraft/test/` tree.
const GAME_TEST_PREFIXES: &[&str] = &["net/minecraft/gametest/", "net/minecraft/test/"];

/// `true` when `target_class` names a legacy datafix-only class in any
/// accepted spelling (dot, slash, `L...;`, or canonical yarn name).
pub fn is_legacy_datafix_target(target_class: &str) -> bool {
    let class = normalize_class(target_class);
    LEGACY_DATAFIX_PREFIXES
        .iter()
        .any(|prefix| class.starts_with(prefix))
}

/// `true` when `target_class` names a GameTest-harness-only class in any
/// accepted spelling (dot, slash, `L...;`, or canonical yarn name).
pub fn is_gametest_target(target_class: &str) -> bool {
    let class = normalize_class(target_class);
    GAME_TEST_PREFIXES
        .iter()
        .any(|prefix| class.starts_with(prefix))
}

/// Classify a mixin `target_class` as [`MixinTargetScope::LiveGameplay`],
/// [`MixinTargetScope::LegacyDataFixOnly`], or
/// [`MixinTargetScope::GameTestOnly`]. Fail-closed by construction:
/// anything outside the verified prefixes — including unrecognized
/// names — is `LiveGameplay`.
pub fn classify_mixin_target(target_class: &str) -> MixinTargetScope {
    if is_legacy_datafix_target(target_class) {
        MixinTargetScope::LegacyDataFixOnly
    } else if is_gametest_target(target_class) {
        MixinTargetScope::GameTestOnly
    } else {
        MixinTargetScope::LiveGameplay
    }
}

// ---------------------------------------------------------------------------
// Pure client-cosmetic mixin targets
// ---------------------------------------------------------------------------

/// One strictly client-visual-only vanilla method a dedicated server
/// scan may skip with telemetry: (normalized class names, accepted
/// method names). Names are factual mapping data only.
struct ClientCosmeticTarget {
    classes: &'static [&'static str],
    methods: &'static [&'static str],
}

/// The verified client-visual-only target table. Every row names a
/// vanilla method whose only effect is client rendering — armor-trim
/// sprite path resolution and item eat/drink particle/sound effects —
/// so a mixin on one can never alter server gameplay. Anything outside
/// the table stays live gameplay (fail closed).
const CLIENT_COSMETIC_TARGETS: &[ClientCosmeticTarget] = &[
    // Armor trim sprite path resolution — only consulted by the client
    // renderer when drawing trimmed armor models (yarn `ArmorTrim`
    // `method_57411`/`method_57413`, the two texture-path methods).
    ClientCosmeticTarget {
        classes: &[
            "net/minecraft/world/item/armortrim/ArmorTrim",
            "net/minecraft/item/trim/ArmorTrim",
            "net/minecraft/class_8053",
        ],
        methods: &[
            "innerTexture",
            "outerTexture",
            "method_57411",
            "method_57413",
        ],
    },
    // Item eating/drinking particles and sounds — the entity produces
    // them only on the client (yarn `shouldSpawnConsumptionEffects`
    // method_22382, `spawnConsumptionEffects` method_6098,
    // `spawnItemParticles` method_6037).
    ClientCosmeticTarget {
        classes: &[
            "net/minecraft/world/entity/LivingEntity",
            "net/minecraft/class_1309",
        ],
        methods: &[
            "shouldTriggerItemUseEffects",
            "triggerItemUseEffects",
            "spawnItemParticles",
            "shouldSpawnConsumptionEffects",
            "spawnConsumptionEffects",
            "method_22382",
            "method_6098",
            "method_6037",
            "shouldTriggerItemUseEffects()Z",
            "triggerItemUseEffects(Lnet/minecraft/world/item/ItemStack;I)V",
            "spawnItemParticles(Lnet/minecraft/world/item/ItemStack;I)V",
        ],
    },
];

/// `true` when `(target_class, target_method)` names a verified
/// client-visual-only vanilla method in any accepted spelling — a
/// dedicated server never runs it, so a server-side mixin scan records
/// the site as `client_cosmetic_skipped` instead of a Tier-3 blocker.
/// Fail-closed: anything outside the table is live gameplay.
pub fn is_client_cosmetic_target(target_class: &str, target_method: &str) -> bool {
    let class = normalize_class(target_class);
    let method = target_method.trim();
    CLIENT_COSMETIC_TARGETS
        .iter()
        .any(|t| t.classes.contains(&class.as_str()) && t.methods.contains(&method))
}

impl MixinLiftVerdict {
    /// One-line operator/diagnostic summary, e.g.
    /// `fully liftable: 2 site(s) bound (test.Mixin -> net/minecraft/.../LivingEntity#hurt @Inject @At(HEAD) => EntityDamagePre)` or
    /// `lifted 1 site(s); 1 unliftable: bad.Mixin -> ... (UnsupportedAnnotation)`.
    pub fn format_lift_summary(&self) -> String {
        match self {
            MixinLiftVerdict::FullyLiftable(bindings) => {
                if bindings.is_empty() {
                    return "fully liftable: 0 sites".to_string();
                }
                let bound: Vec<String> = bindings
                    .iter()
                    .map(|b| format!("{} => {}", describe_site(&b.site), b.hook))
                    .collect();
                format!(
                    "fully liftable: {} site(s) bound ({})",
                    bindings.len(),
                    bound.join(", ")
                )
            }
            MixinLiftVerdict::RequiresFallback { lifted, unliftable } => {
                let rejected: Vec<String> = unliftable
                    .iter()
                    .map(|(site, reason)| format!("{} ({})", describe_site(site), reason))
                    .collect();
                format!(
                    "lifted {} site(s); {} unliftable: {}",
                    lifted.len(),
                    unliftable.len(),
                    rejected.join(", ")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(target_class: &str, target_method: &str, at: InjectPoint) -> MixinSite {
        MixinSite {
            mixin_class: "com.example.mixin.StandardMixin".into(),
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

    fn only_binding(verdict: &MixinLiftVerdict) -> &LiftedBinding {
        match verdict {
            MixinLiftVerdict::FullyLiftable(bindings) if bindings.len() == 1 => &bindings[0],
            other => panic!("expected exactly one lifted binding, got {other:?}"),
        }
    }

    fn only_unliftable(verdict: &MixinLiftVerdict) -> &UnliftableReason {
        match verdict {
            MixinLiftVerdict::RequiresFallback { unliftable, .. } if unliftable.len() == 1 => {
                &unliftable[0].1
            }
            other => panic!("expected exactly one unliftable site, got {other:?}"),
        }
    }

    #[test]
    fn empty_sites_is_fully_liftable() {
        assert_eq!(
            lift_mixin_sites(&[]),
            MixinLiftVerdict::FullyLiftable(vec![])
        );
    }

    /// Every standard hook lifts from both Mojang dot and slash class names.
    #[test]
    fn all_standard_hooks_lift_mojang_dot_and_slash_names() {
        let cases: &[(&str, &str, InjectPoint, StandardEventHook)] = &[
            (
                "net.minecraft.world.entity.LivingEntity",
                "hurt",
                InjectPoint::Head,
                StandardEventHook::EntityDamagePre,
            ),
            (
                "net.minecraft.world.entity.LivingEntity",
                "hurt",
                InjectPoint::Tail,
                StandardEventHook::EntityDamagePost,
            ),
            (
                "net.minecraft.server.level.ServerPlayerGameMode",
                "destroyBlock",
                InjectPoint::Head,
                StandardEventHook::BlockBreakPre,
            ),
            (
                "net.minecraft.server.level.ServerPlayerGameMode",
                "destroyBlock",
                InjectPoint::Return,
                StandardEventHook::BlockBreakPost,
            ),
            (
                "net.minecraft.world.item.BlockItem",
                "place",
                InjectPoint::Head,
                StandardEventHook::BlockPlacePre,
            ),
            (
                "net.minecraft.world.item.BlockItem",
                "place",
                InjectPoint::Tail,
                StandardEventHook::BlockPlacePost,
            ),
            (
                "net.minecraft.server.level.ServerLevel",
                "tick",
                InjectPoint::Head,
                StandardEventHook::ServerTickPre,
            ),
            (
                "net.minecraft.server.level.ServerLevel",
                "tick",
                InjectPoint::Tail,
                StandardEventHook::ServerTickPost,
            ),
            (
                "net.minecraft.world.entity.player.Player",
                "tick",
                InjectPoint::Head,
                StandardEventHook::PlayerTickPre,
            ),
            (
                "net.minecraft.world.entity.player.Player",
                "tick",
                InjectPoint::Tail,
                StandardEventHook::PlayerTickPost,
            ),
            (
                "net.minecraft.server.network.ServerGamePacketListenerImpl",
                "handleChat",
                InjectPoint::Head,
                StandardEventHook::ServerChat,
            ),
            (
                "net.minecraft.server.players.PlayerList",
                "placeNewPlayer",
                InjectPoint::Head,
                StandardEventHook::PlayerJoin,
            ),
            (
                "net.minecraft.server.players.PlayerList",
                "remove",
                InjectPoint::Head,
                StandardEventHook::PlayerLeave,
            ),
            (
                "net.minecraft.world.level.block.state.BlockBehaviour",
                "useWithoutItem",
                InjectPoint::Head,
                StandardEventHook::BlockInteract,
            ),
            (
                "net.minecraft.world.level.block.state.BlockBehaviour",
                "useItemOn",
                InjectPoint::Head,
                StandardEventHook::BlockInteract,
            ),
        ];
        assert_eq!(
            cases
                .iter()
                .map(|(_, _, _, h)| *h)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            14,
            "test table must exercise all 14 StandardEventHook variants"
        );
        for (class, method, at, want) in cases {
            for name in [class.replace('.', "/"), (*class).to_string()] {
                let verdict = lift_mixin_sites(&[site(&name, method, at.clone())]);
                assert_eq!(
                    only_binding(&verdict).hook,
                    *want,
                    "{name}#{method} @ {at:?}"
                );
            }
        }
    }

    /// Intermediary (and Yarn-named) aliases verified against the pinned
    /// 1.21.1 mappings resolve to the same hooks.
    #[test]
    fn intermediary_and_yarn_aliases_lift() {
        let cases: &[(&str, &str, InjectPoint, StandardEventHook)] = &[
            (
                "net/minecraft/class_1309",
                "method_5643",
                InjectPoint::Head,
                StandardEventHook::EntityDamagePre,
            ),
            (
                "net/minecraft/class_1297",
                "method_5643",
                InjectPoint::Tail,
                StandardEventHook::EntityDamagePost,
            ),
            (
                "net/minecraft/class_3225",
                "method_14266",
                InjectPoint::Head,
                StandardEventHook::BlockBreakPre,
            ),
            (
                "net/minecraft/class_3225",
                "tryBreakBlock",
                InjectPoint::Tail,
                StandardEventHook::BlockBreakPost,
            ),
            (
                "net/minecraft/class_1747",
                "method_7712",
                InjectPoint::Head,
                StandardEventHook::BlockPlacePre,
            ),
            (
                "net/minecraft/class_1747",
                "method_7708",
                InjectPoint::Tail,
                StandardEventHook::BlockPlacePost,
            ),
            (
                "net/minecraft/class_3218",
                "method_18765",
                InjectPoint::Head,
                StandardEventHook::ServerTickPre,
            ),
            (
                "net/minecraft/class_1657",
                "method_5773",
                InjectPoint::Head,
                StandardEventHook::PlayerTickPre,
            ),
            (
                "net/minecraft/class_3222",
                "method_14226",
                InjectPoint::Tail,
                StandardEventHook::PlayerTickPost,
            ),
            (
                "net/minecraft/class_3244",
                "method_12048",
                InjectPoint::Head,
                StandardEventHook::ServerChat,
            ),
            (
                "net/minecraft/class_2792",
                "onChatMessage",
                InjectPoint::Head,
                StandardEventHook::ServerChat,
            ),
            (
                "net/minecraft/class_3324",
                "method_14570",
                InjectPoint::Head,
                StandardEventHook::PlayerJoin,
            ),
            (
                "net/minecraft/class_3324",
                "method_14611",
                InjectPoint::Return,
                StandardEventHook::PlayerLeave,
            ),
            (
                "net/minecraft/class_4970",
                "method_55766",
                InjectPoint::Head,
                StandardEventHook::BlockInteract,
            ),
            (
                "net/minecraft/class_4970",
                "method_55765",
                InjectPoint::Head,
                StandardEventHook::BlockInteract,
            ),
            (
                "net/minecraft/class_4970$class_4971",
                "method_55781",
                InjectPoint::Head,
                StandardEventHook::BlockInteract,
            ),
            (
                "net/minecraft/class_4970$class_4971",
                "method_55780",
                InjectPoint::Head,
                StandardEventHook::BlockInteract,
            ),
        ];
        for (class, method, at, want) in cases {
            let verdict = lift_mixin_sites(&[site(class, method, at.clone())]);
            assert_eq!(only_binding(&verdict).hook, *want, "{class}#{method}");
        }
    }

    /// Descriptor-form class names (`L...;`) normalize like slash names.
    #[test]
    fn descriptor_class_names_normalize() {
        let verdict = lift_mixin_sites(&[site(
            "Lnet/minecraft/server/players/PlayerList;",
            "remove",
            InjectPoint::Head,
        )]);
        assert_eq!(only_binding(&verdict).hook, StandardEventHook::PlayerLeave);
    }

    #[test]
    fn non_inject_annotations_all_fail_closed() {
        for annotation in [
            MixinAnnotationKind::Overwrite,
            MixinAnnotationKind::Redirect,
            MixinAnnotationKind::ModifyVariable,
            MixinAnnotationKind::ModifyArg,
            MixinAnnotationKind::ModifyConstant,
            MixinAnnotationKind::Accessor,
            MixinAnnotationKind::Invoker,
            MixinAnnotationKind::Unknown("@Survive".into()),
        ] {
            let mut s = site(
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
                InjectPoint::Head,
            );
            s.annotation = annotation.clone();
            assert_eq!(
                only_unliftable(&lift_mixin_sites(&[s])),
                &UnliftableReason::UnsupportedAnnotation,
                "{annotation:?}"
            );
        }
    }

    #[test]
    fn local_capture_fails_closed() {
        let mut s = site(
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
            InjectPoint::Head,
        );
        s.captures_locals = true;
        assert_eq!(
            only_unliftable(&lift_mixin_sites(&[s])),
            &UnliftableReason::CapturesLocals
        );
    }

    #[test]
    fn non_standard_inject_points_fail_closed() {
        for at in [
            Some(InjectPoint::Invoke("net/minecraft/x/f".into())),
            Some(InjectPoint::Field("field_1234".into())),
            Some(InjectPoint::Other("INVOKE_DYNAMIC".into())),
            Some(InjectPoint::Other("TAIL;INVOKE".into())),
            None,
        ] {
            let mut s = site(
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
                InjectPoint::Head,
            );
            s.at = at.clone();
            assert_eq!(
                only_unliftable(&lift_mixin_sites(&[s])),
                &UnliftableReason::UnsupportedInjectPoint,
                "{at:?}"
            );
        }
    }

    #[test]
    fn cancellable_post_hooks_fail_closed() {
        // TAIL/RETURN run after the vanilla work: they cannot be cancelled.
        for at in [InjectPoint::Tail, InjectPoint::Return] {
            for (class, method) in [
                ("net/minecraft/world/entity/LivingEntity", "hurt"),
                ("net/minecraft/server/level/ServerLevel", "tick"),
                ("net/minecraft/server/players/PlayerList", "placeNewPlayer"),
            ] {
                let mut s = site(class, method, at.clone());
                s.cancellable = true;
                assert_eq!(
                    only_unliftable(&lift_mixin_sites(&[s])),
                    &UnliftableReason::NonCancellablePostHook,
                    "{class}#{method} @ {at:?}"
                );
            }
        }
        // ...but cancellable HEAD/single-phase-pre hooks are legitimate.
        for (class, method, want) in [
            (
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
                StandardEventHook::EntityDamagePre,
            ),
            (
                "net/minecraft/server/level/ServerPlayerGameMode",
                "destroyBlock",
                StandardEventHook::BlockBreakPre,
            ),
            (
                "net/minecraft/server/network/ServerGamePacketListenerImpl",
                "handleChat",
                StandardEventHook::ServerChat,
            ),
        ] {
            let mut s = site(class, method, InjectPoint::Head);
            s.cancellable = true;
            let binding = only_binding(&lift_mixin_sites(&[s])).clone();
            assert_eq!(binding.hook, want);
            assert!(binding.cancellable);
        }
    }

    #[test]
    fn unknown_or_out_of_scope_targets_fail_closed() {
        // Valkyrien Skies 2-shaped deep physics/chunk internals, unknown
        // intermediary classes, and real vanilla methods outside the table.
        for (class, method) in [
            ("net/minecraft/world/level/chunk/LevelChunk", "method_12205"),
            ("org/valkyrienskies/core/impl/shadow/VSPhysics", "tick"),
            ("net/minecraft/class_12345", "method_99999"),
            ("net/minecraft/server/level/ServerLevel", "addPlayer"),
            ("net/minecraft/world/entity/LivingEntity", "method_6007"),
            // Broad vanilla methods deliberately outside the hook scope:
            // LivingEntity.tick is neither `Entity.tick` (all entities →
            // EntityTick) nor `Player.tick` (PlayerTick*) — the shim can
            // narrow neither population, so it must never lift.
            ("net/minecraft/world/entity/LivingEntity", "tick"),
            ("net/minecraft/class_1309", "method_5773"),
            // Player.hurt / ServerPlayer.hurt stay unmapped (narrow dispatch).
            ("net/minecraft/class_1657", "method_5643"),
            ("net/minecraft/server/level/ServerPlayer", "hurt"),
        ] {
            assert_eq!(
                only_unliftable(&lift_mixin_sites(&[site(class, method, InjectPoint::Head)])),
                &UnliftableReason::UnknownTarget,
                "{class}#{method}"
            );
        }
    }

    #[test]
    fn one_bad_site_fails_the_whole_mod() {
        let mut bad = site(
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
            InjectPoint::Return,
        );
        bad.annotation = MixinAnnotationKind::Redirect;
        let good = site(
            "net/minecraft/server/level/ServerPlayerGameMode",
            "destroyBlock",
            InjectPoint::Head,
        );
        match lift_mixin_sites(&[good.clone(), bad]) {
            MixinLiftVerdict::RequiresFallback { lifted, unliftable } => {
                assert_eq!(lifted.len(), 1);
                assert_eq!(lifted[0].hook, StandardEventHook::BlockBreakPre);
                assert_eq!(unliftable.len(), 1);
                assert_eq!(unliftable[0].1, UnliftableReason::UnsupportedAnnotation);
            }
            other => panic!("expected RequiresFallback, got {other:?}"),
        }
    }

    #[test]
    fn verdict_summary_names_hooks_and_rejections() {
        let lifted = lift_mixin_sites(&[
            site(
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
                InjectPoint::Head,
            ),
            site(
                "net/minecraft/server/level/ServerLevel",
                "tick",
                InjectPoint::Tail,
            ),
        ]);
        let summary = lifted.format_lift_summary();
        assert!(
            summary.starts_with("fully liftable: 2 site(s) bound"),
            "{summary}"
        );
        assert!(summary.contains("EntityDamagePre"), "{summary}");
        assert!(summary.contains("ServerTickPost"), "{summary}");

        let mut bad = site("net/minecraft/x/Unknown", "y", InjectPoint::Head);
        bad.annotation = MixinAnnotationKind::ModifyConstant;
        let mixed = lift_mixin_sites(&[bad]);
        let summary = mixed.format_lift_summary();
        assert!(summary.contains("0 site(s)"), "{summary}");
        assert!(summary.contains("1 unliftable"), "{summary}");
        assert!(summary.contains("net/minecraft/x/Unknown#y"), "{summary}");
        assert!(summary.contains("UnsupportedAnnotation"), "{summary}");

        assert_eq!(
            lift_mixin_sites(&[]).format_lift_summary(),
            "fully liftable: 0 sites"
        );
    }
}
