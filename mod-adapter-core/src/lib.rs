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
//! - [`fallback`]: 4-tier mod execution classifier and safe fallback planner
//!   (`Strict`, `Quarantine` with Save v2 block-loss guard, `DelegateNeoForge`).

#![deny(unsafe_code)]

pub mod blockstates;
pub mod data_loader;
pub mod fallback;
pub mod neoforge_wire;
pub mod profile;
