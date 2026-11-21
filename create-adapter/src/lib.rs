// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Zero-JVM Create Mod core adapter backed by [`mod_offload`].
//!
//! Implements Phase C1-a of `specs/mod-compatibility/create-mod-strategy.md`:
//! - Deterministic NeoForge-order multi-property `BlockState` Cartesian expansion
//!   and canonical Save v2 string palette encoding/decoding.
//! - Zero-dependency data-driven JSON & stored-ZIP/JAR recipe/tag extractor.
//! - Region-isolated Create kinetic, belt transport, mechanical processing, and
//!   fluid factory adapter driving `mod_offload::region_scheduler::RegionScheduler`
//!   across arbitrary dynamic dimensions (`dim: i32`).

#![deny(unsafe_code)]

pub mod blockstates;
pub mod contraption_adapter;
pub mod data_loader;
pub mod factory;
pub mod neoforge_wire;
