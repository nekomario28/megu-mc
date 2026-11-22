// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room Mixin-to-Event static lifter.
//!
//! Most real modpack Mixin usage is a small set of boring, standard
//! `@Inject(at = @At("HEAD" | "TAIL" | "RETURN"))` hooks on vanilla lifecycle
//! methods — damage, block break/place, server/player ticks, chat, join/leave,
//! and block interaction. [`lift_mixin_sites`] maps exactly those sites onto
//! mc-spine's [`StandardEventHook`]s so
//! [`crate::fallback::classify_mod_with_mixins`] can promote a mod whose *only*
//! unsupported surface is a fully liftable mixin set from Tier 3
//! (`RequiresFallback`) to Tier 2 (`BoundedJvmShim`) with exact event
//! bindings.
//!
//! The lifter is strictly fail-closed. Anything it cannot prove safe —
//! `@Overwrite` / `@Redirect` / `@ModifyVariable` / `@ModifyArg` /
//! `@ModifyConstant` / `@Accessor` / `@Invoker` / unknown annotations,
//! `locals` capture, `@At("INVOKE")` / `@At("FIELD")` or any non-trivial
//! injection point, a cancellable injection at a point where the vanilla
//! work is already done, and any target class or method outside the standard
//! table — is reported [`UnliftableReason`] and keeps the whole mod at
//! Tier 3. It never guesses. The non-mutating `@Shadow`/`@Unique`
//! member *declarations* are not injection sites at all and never reach the
//! lifter (see [`MixinMemberKind`]); a manually constructed site carrying
//! one still fails closed here.
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
        }
    }
}

impl fmt::Display for StandardEventHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One successfully lifted injection site bound to a standard event hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiftedBinding {
    pub site: MixinSite,
    pub hook: StandardEventHook,
    /// Mirrors the site's `cancellable` flag; always `false` for post hooks
    /// (a cancellable post injection is [`UnliftableReason::NonCancellablePostHook`]).
    pub cancellable: bool,
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
    // it could never take the action back; only pre-phase sites may cancel.
    if site.cancellable && phase == Phase::Post {
        return Err(UnliftableReason::NonCancellablePostHook);
    }
    Ok(LiftedBinding {
        site: site.clone(),
        hook,
        cancellable: site.cancellable,
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
];

/// Mixin's own field-name inference for an `@Accessor` without a `value`
/// element: strip the `get`/`is`/`set` prefix and lowercase the first
/// remaining character (`getYRot` → `yRot`, `isOnGround` → `onGround`).
fn infer_accessor_field_name(handler_name: &str) -> Option<String> {
    let stripped = handler_name
        .strip_prefix("get")
        .or_else(|| handler_name.strip_prefix("is"))
        .or_else(|| handler_name.strip_prefix("set"))?;
    let mut chars = stripped.chars();
    let first = chars.next()?;
    Some(first.to_lowercase().chain(chars).collect())
}

/// Classify one `@Accessor` site: a `get*`/`is*` handler with a `()T`
/// (non-void) signature on a table field lifts to a native read binding;
/// `set*`/`(T)V` shapes, opaque signatures, and unknown fields fail
/// closed with their [`AccessorRejection`].
fn lift_accessor_site(site: &MixinSite) -> Result<LiftedAccessorBinding, AccessorRejection> {
    if site.annotation != MixinAnnotationKind::Accessor {
        return Err(AccessorRejection::NotAnAccessor);
    }
    let name = site.handler_name.as_str();
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
    let field = if site.target_method != site.handler_name {
        site.target_method.clone()
    } else {
        match infer_accessor_field_name(name) {
            Some(field) => field,
            None => return Err(AccessorRejection::OpaqueSignature),
        }
    };
    let class = normalize_class(&site.target_class);
    let row = SAFE_ACCESSOR_FIELDS
        .iter()
        .find(|r| r.classes.contains(&class.as_str()) && r.fields.contains(&field.as_str()))
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
            // Entity.tick / LivingEntity.tick cover all entities, not just
            // players, so they must never lift to PlayerTick*.
            ("net/minecraft/class_1297", "method_5773"),
            ("net/minecraft/world/entity/Entity", "tick"),
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
