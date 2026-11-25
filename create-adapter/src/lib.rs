// SPDX-License-Identifier: LGPL-3.0-or-later

//! Zero-JVM Create Mod core adapter backed by [`mod_offload`].
//!
//! Implements Phase C1-a of `specs/mod-compatibility/create-mod-strategy.md`:
//! - Deterministic NeoForge-order multi-property `BlockState` Cartesian expansion
//!   and canonical Save v2 string palette encoding/decoding.
//! - Zero-dependency data-driven JSON & stored-ZIP/JAR recipe/tag extractor.
//! - Region-isolated Create kinetic, belt transport, mechanical processing, and
//!   fluid factory adapter driving `mod_offload::region_scheduler::RegionScheduler`
//!   across arbitrary dynamic dimensions (`dim: i32`).
//! - [`orbital`]: clean-room altitude-trigger (`Y >= 560`-style) dimension
//!   transitions for 6-DOF ships — atomic ship + passenger transfers into
//!   microgravity or low-gravity planetary dimensions with per-dimension
//!   [`aeronautics::AtmospherePhysics`] and anti-cheat-safe passenger
//!   `TeleportIntent`s.

#![deny(unsafe_code)]

pub mod addon_profile;
pub mod aeronautics;
pub mod blockstates;
pub mod contraption_adapter;
pub mod create_compat;
pub mod data_loader;
pub mod factory;
pub mod factorio_sim;
pub mod neoforge_wire;
pub mod offload_bridge;
pub mod orbital;
pub mod rail_network;
pub mod schematic;

use std::collections::BTreeSet;

use crate::offload_bridge::{OffloadBridge, OffloadBudget, OffloadError, OffloadTickReport};
use crate::rail_network::{ChunkKey, RailNetwork, TransitEvent};

/// Combined live Create adapter engine.
///
/// [`OffloadBridge`] remains the owner of factory, contraption, orbital,
/// combat-motion, and companion-storage state. This facade composes that
/// bridge with the live [`RailNetwork`] so train simulation is consumed by
/// the same production engine surface instead of existing as a library-only
/// module. Each tick runs the offload bridge first and then advances trains
/// against the most recently supplied loaded rail chunk set.
///
/// The rail side is dormant-by-shape when no trains are registered: one
/// empty sorted-key snapshot iteration per tick, no chunk access, and no
/// allocation beyond the network's retained scratch buffer.
pub struct CreateAdapterEngine {
    offload: OffloadBridge,
    rail_network: RailNetwork,
    loaded_rail_chunks: BTreeSet<ChunkKey>,
}

/// Deterministic output of one [`CreateAdapterEngine::step_tick`].
#[derive(Clone, Debug)]
pub struct CreateAdapterTickReport {
    pub offload: OffloadTickReport,
    pub rail_events: Vec<TransitEvent>,
    /// Rail-network logical tick after the advance.
    pub rail_tick: u64,
}

impl CreateAdapterEngine {
    /// Empty production engine: empty offload bridge, empty rail network,
    /// and no loaded rail chunks.
    pub fn new() -> Self {
        Self {
            offload: OffloadBridge::new(),
            rail_network: RailNetwork::new(),
            loaded_rail_chunks: BTreeSet::new(),
        }
    }

    /// Engine around a preconfigured offload bridge.
    pub fn with_offload(offload: OffloadBridge) -> Self {
        Self {
            offload,
            ..Self::new()
        }
    }

    pub fn offload(&self) -> &OffloadBridge {
        &self.offload
    }

    /// Mutable offload access for registration/setup. Per-tick execution
    /// still goes through [`step_tick`](Self::step_tick) so rail/offload
    /// ordering stays fixed.
    pub fn offload_mut(&mut self) -> &mut OffloadBridge {
        &mut self.offload
    }

    pub fn rail_network(&self) -> &RailNetwork {
        &self.rail_network
    }

    /// Mutable rail access for topology, station, signal, and train setup.
    pub fn rail_network_mut(&mut self) -> &mut RailNetwork {
        &mut self.rail_network
    }

    /// Replace the loaded chunk-column set used for train render-mode
    /// transitions. Trains outside this set continue virtually in O(1).
    pub fn set_loaded_rail_chunks(&mut self, chunks: BTreeSet<ChunkKey>) {
        self.loaded_rail_chunks = chunks;
    }

    pub fn loaded_rail_chunks(&self) -> &BTreeSet<ChunkKey> {
        &self.loaded_rail_chunks
    }

    /// Advance the combined engine one server tick. The ordering contract is
    /// fixed: offload state first, rail network second, then one immutable
    /// report value pairing both outputs.
    pub fn step_tick(
        &mut self,
        tick: u64,
        budget: OffloadBudget,
    ) -> Result<CreateAdapterTickReport, OffloadError> {
        let offload = self.offload.step_tick(tick, budget)?;
        let rail_events =
            rail_network::tick_rail_network(&mut self.rail_network, &self.loaded_rail_chunks);
        Ok(CreateAdapterTickReport {
            offload,
            rail_events,
            rail_tick: self.rail_network.tick,
        })
    }
}

impl Default for CreateAdapterEngine {
    fn default() -> Self {
        Self::new()
    }
}
