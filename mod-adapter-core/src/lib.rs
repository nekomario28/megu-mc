// SPDX-License-Identifier: LGPL-3.0-or-later

//! Universal all-mod optimization and compatibility core backed by [`mod_offload`].
//!
//! Provides the shared Layer 1 foundation used by both generic mods (via
//! declarative [`profile::ModOffloadProfile`] manifests) and flagship
//! special-case adapters such as `create-adapter`:
//! - [`blockstates`]: Deterministic NeoForge-order multi-property `BlockState`
//!   Cartesian expansion and Save v2 canonical `"namespace:block[k=v]"` codec.
//! - [`data_loader`]: Zero-JVM JSON & stored-ZIP/JAR loader for recipes, tags,
//!   and declarative mod offload profiles across any namespace.
//! - [`dep_resolver`]: missing mod-dependency UX engine — Modrinth v2
//!   auto-fetch query building and verified artifact selection behind
//!   [`mod_sync::ConsentToken`]-gated, content-addressed installs, plus a
//!   restart-free hot-reload classifier for newly dropped JARs and an
//!   EN/JA [`dep_resolver::DepRecoveryPlan`] operator summary.
//! - [`neoforge_wire`]: Protocol 767 VarInt/string codecs and NeoForge 1.21.1
//!   Configuration `frozen_registry*` synchronization frames.
//! - [`profile`]: Generic declarative offload runtime bridging any mod's
//!   cables/pipes/tanks/processors onto `mod_offload::RegionScheduler`.
//! - [`dimension_mod`]: Universal dimension-adding mod support — zero-JVM
//!   `dimension_type`/`dimension`/`worldgen`/ore-tag discovery, declarative
//!   mod portals (vertical frames, fluid pools, rocket ascent, interact
//!   blocks), per-dimension environment profiles, and >64-section light
//!   masks for tall custom dimensions.
//! - [`ecosystem_compat`]: top-100 generic ecosystem primitives — map-mod
//!   fair-play mode (Xaero control codes + level-id plugin payloads client
//!   side, occluded-entity radar suppression server side, identical on
//!   dedicated servers and friend hosts), a unified Curios/Trinkets
//!   [`ecosystem_compat::AccessoryInventory`] with vanilla three-stage
//!   attribute aggregation and `ACC1` persistence, and the dual-loader
//!   (`fabric.mod.json` + `META-INF/neoforge.mods.toml`) JAR scanner feeding
//!   [`ecosystem_compat::plan_dual_loader_mods_dir`]'s unified Fabric +
//!   NeoForge mod table.
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
//!   directly into [`mixin_lifter::lift_mixin_sites`]. Non-mutating
//!   `@Shadow`/`@Unique` members are recorded as declarations in scan
//!   telemetry — never sites, never blockers.
//! - [`intermediary_map`]: clean-room Tiny v2 (`*.tiny`) runtime mapping
//!   loader and canonical Intermediary→Mojmap name resolver —
//!   [`intermediary_map::parse_tiny_v2`] produces the
//!   [`intermediary_map::TinyV2MappingTable`] that
//!   [`mixin_lifter::lift_mixin_sites_with_mappings`] normalizes Fabric
//!   Intermediary targets through, so they bind the exact same
//!   [`mixin_lifter::StandardEventHook`] as NeoForge Mojmap targets; a
//!   built-in anchor table covers the standard hook classes/methods when
//!   no `.tiny` document is supplied.
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
//! - [`colossal_and_forge`]: generic colossal multi-part creature and
//!   multi-block forge primitives — a five-stage
//!   [`colossal_and_forge::GrowthStageCurve`] with smooth `0.35..=2.2`
//!   hitbox scaling and a `flight_unlock_stage` gate, yaw/pitch-rotated
//!   multi-part AABBs with per-part damage multipliers and ray hit
//!   testing, a stage-gated mounted-flight controller with stamina and
//!   safe-landing detection, bounded elemental breath cones (<= 48
//!   blocks reach, <= 256 block transforms per tick) driving declarative
//!   terrain-transform tables and lightning chain-arcs, and a generic
//!   `validate_structure` multi-block forge with breath-charged heat and
//!   lossless alloy smelting — all data-driven, zero mod-specific code.
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
//! - [`mine_cut_all`]: strictly opt-in MineAll/CutAll planner — deterministic
//!   26-neighbor 3D BFS over ore veins and log/tree trunks with data-driven
//!   block/item tags, claim protection, exact exhaustion, and a durability
//!   budget that preserves the tool's last point by default.
//! - [`bytecode_property_extractor`]: pure-`std` zero-JVM static abstract
//!   interpreter for mod `.class` files and in-memory JARs — constant-folds
//!   block/item/food property-builder chains (including same-class constant
//!   light lambdas) under explicit class/method/instruction/archive budgets.
//! - [`bytecode_subset_vm`]: pure-`std` bounded `.class` bytecode subset
//!   evaluator — zero-JVM execution of safe mod event/logic methods with
//!   a fail-closed static verifier (opcode allowlist, instruction-boundary
//!   branch checks, operand-stack dataflow proof, and an allowlist-only
//!   [`bytecode_subset_vm::HostCallKind`] bridge that rejects reflection,
//!   `Runtime`/`ProcessBuilder`, JNDI, IO, and networking before a single
//!   instruction runs) plus a strict fuel cap so no mod loop can hang a
//!   server tick.
//! - [`mod_sync`]: hardened server→client mod sync protocol — the `MSY1`
//!   `CustomPacketPayload` body on channel `megu:mod_sync_v1`, an
//!   HTTPS-only trusted-CDN allowlist with SSRF/IP-literal/credentials
//!   rejection, a strict `.jar` basename jail, zero-dependency pure-`std`
//!   FIPS 180-4 SHA-256 with constant-time digest comparison,
//!   [`mod_sync::ConsentToken`]-gated atomic installs that refuse symlink
//!   escapes, and [`fallback::BUILTIN_SUPERSEDED_PERF_MODS`] auto-skip.
//! - [`custom_media`]: generic content-addressed custom painting/photo
//!   media registry — fail-closed PNG/JPEG header inspection with hard
//!   dimension/size bounds, per-player quotas, SHA-256 dedup, canvas
//!   placement tracking, and chunked `MDIA` wire framing with verified
//!   out-of-order reassembly for immersive-painting/camera mods.
//! - [`loader_extensions`]: generic NeoForge/Fabric 1.21.1 loader
//!   extensions — `quilt.mod.json` parsing, bounded recursive
//!   `META-INF/jarjar/` Jar-in-Jar inspection, `accesstransformer.cfg`
//!   directives with JVM-bridge classification, the
//!   `fabric:registry/sync/direct` wire codec, `tags/fluid` loading,
//!   NeoForge `data_maps` + `neoforge/biome_modifier` documents, and the
//!   ten `c:` conventional tag families with `forge:`/`neoforge:` alias
//!   resolution over a Vanilla 1.21.1 baseline.
//! - [`unified_bundle`]: shrink-to-generic `megu-mod-bundle/1`
//!   declarative mod bundle — one pure-JSON document declaring a mod's
//!   portals, progression barriers, biome regions, creature specs, and
//!   custom payload channels, installed atomically into
//!   [`unified_bundle::UnifiedModRuntime`] with zero lines of mod-specific
//!   Rust, plus [`unified_bundle::export_builtin_dimension_bundles`]
//!   proving the built-in dimension profiles are pure data.
//! - [`auto_adapter`]: built-in zero-JVM automatic mod adapter engine —
//!   one [`auto_adapter::auto_adapt_jar`]/[`auto_adapter::auto_adapt_mods_dir`]
//!   call chains superseded-perf-mod skip, loader-manifest metadata,
//!   generic data/culinary/dimension discovery, `.class` Mixin bytecode
//!   lifting, and `megu-mod-bundle/1` synthesis into a four-tier
//!   [`auto_adapter::AdaptationTier`] verdict with exact human-readable
//!   blockers, plus [`auto_adapter::scaffold_native_bundle`] emitting a
//!   validated starter bundle for native mod authors.
//! - [`mod_tick_runtime`]: generic modded block-entity tick runtime on
//!   `megu_core::perf_kit` — O(1) dirty-fingerprint idle skips,
//!   exact-tick wake scheduling, per-tick-budget burst slicing, and
//!   last-write-wins coalesced outbound deltas for any machine family.
//! - [`magic_primitives`]: clean-room magic-mod primitives engine —
//!   NaN-safe [`magic_primitives::ManaPool`]/[`magic_primitives::MagicAttributes`]
//!   with clamped reductions and per-school power, tick-driven instant /
//!   cast-time / channeled casting with damage/move interrupts, per-spell
//!   cooldowns + GCD, a composable [`magic_primitives::SpellDefinition`]
//!   delivery graph (self/touch/projectile/beam/AoE/chain/orbit/summon)
//!   under fail-closed caps, data-driven
//!   [`magic_primitives::CustomStatusRegistry`] DoT/HoT/absorption/CC
//!   statuses, multiblock [`magic_primitives::RitualRecipe`] altars with a
//!   bounded cycle-safe [`magic_primitives::ManaNetwork`], and the `MAG1`
//!   `megu:magic_v1` sync codec.
//! - [`unified_tags`]: cross-mod `XMOD1-CORE` unified tag foundation —
//!   one merged item/block/fluid [`unified_tags::UnifiedTagIndex`] across
//!   every loaded JAR (bounded Jar-in-Jar inner JARs included), datapacks,
//!   and the Vanilla 1.21.1 `c:` baseline, with deterministic nested-tag
//!   expansion, depth-16 fail-closed cycle guards, `"replace"`/optional
//!   entry handling, and bidirectional `#c:*` ↔ `#forge:*`/`#neoforge:*`
//!   common-tag aliasing, plus flattening interop with
//!   [`culinary_adapter::TagIndex`] and the `auto_adapter` tag table.
//! - [`capability_bus`]: cross-mod block-capability bus — a pure-Rust
//!   NeoForge `BlockCapabilities`/Fabric Transfer-API model
//!   ([`capability_bus::ItemTransferHandler`],
//!   [`capability_bus::FluidTransferHandler`],
//!   [`capability_bus::EnergyTransferHandler`]) over dimension-generic
//!   [`capability_bus::BlockCoord`]s, lossless component-carrying
//!   [`capability_bus::CapabilityItemStack`]s, atomic simulate-then-commit
//!   `transfer_*_between` helpers that can never duplicate or vanish
//!   cargo, saturating [`capability_bus::EnergyUnit`] /
//!   [`capability_bus::EnergyConversionRates`] /
//!   [`capability_bus::EnergyAmount`] FE/RF/EU/SU conversion, and
//!   ready-made [`capability_bus::SimpleSlotInventory`] /
//!   [`capability_bus::SimpleFluidTank`] /
//!   [`capability_bus::SimpleEnergyBuffer`] /
//!   [`capability_bus::CookingPotItemAdapter`] endpoints.
//! - [`recipe_conflict_unify`]: always-on recipe conflict index —
//!   Vanilla's silent same-input collisions become a deterministic,
//!   vanilla-first `< i/N >` selector over
//!   [`recipe_conflict_unify::InputSignature`] keys — plus the strictly
//!   opt-in [`recipe_conflict_unify::MaterialUnifier`] that maps every
//!   `#c:<family>/<material>` (`#forge:*` spellings included) tag member
//!   onto one namespace-priority-ranked canonical item, with a
//!   per-item exclusion list and a disabled-by-default passthrough.

#![deny(unsafe_code)]

pub mod auto_adapter;
pub mod blockstates;
pub mod bytecode_property_extractor;
pub mod bytecode_subset_vm;
pub mod capability_bus;
pub mod class_mixin_scanner;
pub mod colossal_and_forge;
pub mod creature_primitives;
pub mod culinary_adapter;
pub mod custom_media;
pub mod data_loader;
pub mod dep_resolver;
pub mod dimension_mod;
pub mod ecosystem_compat;
pub mod fallback;
pub mod hard_mod_primitives;
pub mod intermediary_map;
pub mod live_registry_bridge;
pub mod loader_extensions;
pub mod loot_modifier_engine;
pub mod magic_primitives;
pub mod mine_cut_all;
pub mod mixin_lifter;
pub mod mod_isolation;
pub mod mod_router;
pub mod mod_sync;
pub mod mod_tick_runtime;
pub mod modpack;
pub mod neoforge_wire;
pub mod profile;
pub mod provenance_diff;
pub mod recipe_conflict_unify;
pub mod season_primitives;
pub mod spine_bridge;
pub mod unified_bundle;
pub mod unified_tags;
pub mod vanilla_qol_and_holidays;
pub mod version_compat;
pub mod world_extensions;

pub use colossal_and_forge::{
    ElementKind, ElementalBreathSpec, GrowthStageCurve, GrowthStageSpec, MultiBlockForgeRuntime,
    MultiBlockForgeSpec, MultiPartHitbox, PartKind,
};
pub use dimension_mod::{
    DimEntryDescriptor, DimTypeDescriptor, DimensionEnvProfile, DimensionModClassification,
    DimensionModData, DimensionModProfile, GeneratorKind, PortalRegistry, PortalRule,
    PortalTransition, PortalTrigger, TeleportIntent,
};
pub use recipe_conflict_unify::{
    InputSignature, MaterialUnifier, MaterialUnifyConfig, RecipeCandidate, RecipeConflictIndex,
    ResolvedRecipeChoice, MATERIAL_TAG_FAMILIES,
};
