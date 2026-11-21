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
//!   optimization mods (skipped before any Tier 3 check).
//! - [`version_compat`]: multi-version Minecraft compatibility engine —
//!   dependency-free Maven interval (NeoForge/Forge) and SemVer (Fabric)
//!   version-range parsing, the `pack_format`/`DataVersion` lookup table, and
//!   target-version-aware compat verdicts consumed by
//!   [`fallback::plan_mod_startup_for_version`].

#![deny(unsafe_code)]

pub mod blockstates;
pub mod data_loader;
pub mod dimension_mod;
pub mod fallback;
pub mod neoforge_wire;
pub mod profile;
pub mod version_compat;

pub use dimension_mod::{
    DimEntryDescriptor, DimTypeDescriptor, DimensionEnvProfile, DimensionModClassification,
    DimensionModData, DimensionModProfile, GeneratorKind, PortalRegistry, PortalRule,
    PortalTransition, PortalTrigger, TeleportIntent,
};
