// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal all-mod optimization and compatibility core backed by [`mod_offload`].
//!
//! Provides the shared Layer 1 foundation used by both generic mods (via
//! declarative [`profile::ModOffloadProfile`] manifests) and flagship
//! special-case adapters such as `create-adapter`:
//! - [`blockstates`]: Deterministic NeoForge-order multi-property `BlockState`
//!   Cartesian expansion and Save v2 canonical `"namespace:block[k=v]"` codec.
//! - [`data_loader`]: Zero-JVM JSON & stored-ZIP/JAR loader for recipes, tags,
//!   and declarative mod offload profiles across any namespace.
//! - [`neoforge_wire`]: Protocol 767 VarInt/string codecs and NeoForge 1.21.1
//!   Configuration `frozen_registry*` synchronization frames.
//! - [`profile`]: Generic declarative offload runtime bridging any mod's
//!   cables/pipes/tanks/processors onto `mod_offload::RegionScheduler`.
//! - [`dimension_mod`]: Universal dimension-adding mod support — zero-JVM
//!   `dimension_type`/`dimension`/`worldgen`/ore-tag discovery, declarative
//!   mod portals (vertical frames, fluid pools, rocket ascent, interact
//!   blocks), per-dimension environment profiles, and >64-section light
//!   masks for tall custom dimensions.
//! - [`fallback`]: 4-tier mod execution classifier and safe fallback planner
//!   (`Strict`, `Quarantine` with Save v2 block-loss guard, `DelegateNeoForge`),
//!   plus [`fallback::BUILTIN_SUPERSEDED_PERF_MODS`] superseding of redundant
//!   optimization mods (skipped before any Tier 3 check) and
//!   [`fallback::classify_mod_with_mixins`], which promotes a mod whose only
//!   Tier 3 blockers are fully liftable `@Inject` sites to `BoundedJvmShim`.
//! - [`mixin_lifter`]: clean-room Mixin-to-Event static lifter — maps standard
//!   `@Inject` sites on vanilla lifecycle methods (Mojang dot/slash,
//!   Intermediary, and Yarn names) onto [`mixin_lifter::StandardEventHook`]
//!   bindings, failing closed on any `@Overwrite`/`@Redirect`/locals-capture/
//!   non-`HEAD`/`TAIL`/`RETURN` site or unknown target.
//! - [`class_mixin_scanner`]: pure-`std` JVM `.class` bytecode parser and
//!   JAR Mixin scanner — extracts `@Mixin`/`@Inject` (and every other
//!   `org.spongepowered.asm.mixin` annotation) from real mod JAR bytecode
//!   with zero JVM invocation, feeding [`mixin_lifter::MixinSite`]s
//!   directly into [`mixin_lifter::lift_mixin_sites`].
//! - [`culinary_adapter`]: clean-room, fully generic Farmer's Delight
//!   culinary engine — auto-discovers any `*:cooking` / `*:fermenting` /
//!   `*:cutting` recipe, item/block tag, `*:add_item` / `*:add_table` loot
//!   modifier, and food spec inside **any** culinary mod or addon JAR
//!   (zero hardcoded namespaces), resolves nested tags, executes
//!   deterministic chance rolls, and drives a sleeping-timer cooking pot
//!   that consumes zero CPU ticks while unheated, unmatched, or full.
//! - [`version_compat`]: multi-version Minecraft compatibility engine —
//!   dependency-free Maven interval (NeoForge/Forge) and SemVer (Fabric)
//!   version-range parsing, the `pack_format`/`DataVersion` lookup table, and
//!   target-version-aware compat verdicts consumed by
//!   [`fallback::plan_mod_startup_for_version`].
//! - [`modpack`]: Modrinth `.mrpack` (`modrinth.index.json`) and CurseForge
//!   (`manifest.json`) modpack pre-flight analyzer — parses both manifest
//!   formats fail-closed (including path-traversal rejection), checks the
//!   pack's Minecraft version against the server target, and reports in
//!   plain language which files are auto-skipped as
//!   [`fallback::BUILTIN_SUPERSEDED_PERF_MODS`], which are client-only,
//!   which run natively, and which still need a bytecode inspection.
//! - [`creature_primitives`]: generic zero-mod-name creature primitives —
//!   a declarative [`creature_primitives::CreatureSpec`]/[`creature_primitives::CreatureState`]
//!   tame/breed/command-mode state machine (`Follow`/`Sit`/`Wander`, healing,
//!   love mode, baby growth, owner-follow teleports), five-type
//!   [`creature_primitives::MovementProfile`] locomotion (`Land`,
//!   `Amphibious`, `Aquatic`, `GlidingFlight`, `WallClimb`), and a
//!   dirty-tracked [`creature_primitives::DynamicEntityMetadata`] table with
//!   the exact Protocol 767 `0x58` `SetEntityData` wire codec.
//! - [`world_extensions`]: generic world & mod extension primitives —
//!   declarative [`world_extensions::ProgressionBarrierEngine`] biome gates
//!   (Twilight-Forest-style chained boss progression, RPG packs), multi-part
//!   [`world_extensions::BossEncounterState`] hitboxes, the bounded `CGD1`
//!   chunk-generator / `EAD1` entity-AI delegate wire contracts, built-in
//!   AppleSkin + server-side inventory-sorter QoL support with a proven
//!   item-conservation invariant, and a
//!   [`world_extensions::BiomeRegionBlender`] TerraBlender-style weighted
//!   Voronoi multi-region biome composer.
//! - [`mod_sync`]: hardened server→client mod sync protocol — the `MSY1`
//!   `CustomPacketPayload` body on channel `mc-spine:mod_sync_v1`, an
//!   HTTPS-only trusted-CDN allowlist with SSRF/IP-literal/credentials
//!   rejection, a strict `.jar` basename jail, zero-dependency pure-`std`
//!   FIPS 180-4 SHA-256 with constant-time digest comparison,
//!   [`mod_sync::ConsentToken`]-gated atomic installs that refuse symlink
//!   escapes, and [`fallback::BUILTIN_SUPERSEDED_PERF_MODS`] auto-skip.

#![deny(unsafe_code)]

pub mod blockstates;
pub mod class_mixin_scanner;
pub mod creature_primitives;
pub mod culinary_adapter;
pub mod data_loader;
pub mod dimension_mod;
pub mod fallback;
pub mod mixin_lifter;
pub mod modpack;
pub mod mod_sync;
pub mod neoforge_wire;
pub mod profile;
pub mod version_compat;
pub mod world_extensions;

pub use dimension_mod::{
    DimEntryDescriptor, DimTypeDescriptor, DimensionEnvProfile, DimensionModClassification,
    DimensionModData, DimensionModProfile, GeneratorKind, PortalRegistry, PortalRule,
    PortalTransition, PortalTrigger, TeleportIntent,
};
