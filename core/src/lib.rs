// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc-spine-core`: the foundational primitives every other mc-spine crate
//! (`proto`, `state-store`, `server-files`, `worldgen`, `sim`, `client-*`)
//! shares, in the same role `horizon-core` fills upstream.
//!
//! This crate is a standalone, zero-dependency library so the simulation
//! contract is pinned in exactly one place and stays buildable on pure `std`.
//! Layers:
//!
//! * [`version`] — the canonical two-target version registry:
//!   [`version::TargetVersion`] (`1.21.1`/protocol 767 and `26.3`/protocol 777)
//!   with the [`version::VersionMeta`] constants (data version, pack formats,
//!   Java major) downstream code keys on instead of scattering literals.
//! * [`id`] — [`id::ResourceLocation`], vanilla's `namespace:path` identifier
//!   with the full character-set validation and fail-closed parsing.
//! * [`random`] — Java-exact random sources: [`random::LegacyRandom`] (the
//!   48-bit `java.util.Random` LCG behind `LegacyRandomSource`),
//!   [`random::Xoroshiro128pp`] (`Xoroshiro128PlusPlusRandomSource`) and
//!   [`random::WorldgenRandom`] (the `BitRandomSource` draw contract worldgen
//!   sees), plus the positional-fork/`RandomSplitter` machinery.
//! * [`math`] — the [`math::Mth`] primitives simulation code calls every tick:
//!   the 65 536-entry `sin`/`cos` table, `floor_div`/`floor_mod` grid
//!   arithmetic, `clamp`, the `lerp` family, `smoothstep`, `wrap_degrees` and
//!   `get_seed`.
//! * [`crash_report`] — [`crash_report::CrashContext`]: deterministic
//!   `MEGU-E-XXXXXXXX` fingerprints and privacy-redacted bilingual blame
//!   reports for operator-facing crash triage.
//! * [`perf_kit`] — universal zero-overhead performance primitives: a
//!   [`perf_kit::DirtyFingerprintGate`] for O(1) idle skipping,
//!   [`perf_kit::BoundedScratchVec`] reusable scratch storage with bounded
//!   retention, a [`perf_kit::TickBudgetQueue`] for smooth per-tick burst
//!   slicing, an [`perf_kit::EventWakeWheel`] exact-tick wake scheduler, a
//!   [`perf_kit::CoalescedDeltaBatch`] last-write-wins delta collapser, and
//!   an [`perf_kit::AdaptiveLoadGovernor`] hysteresis budget scale — all
//!   generic so any vanilla or modded subsystem can plug in.

#![deny(unsafe_code)]

pub mod crash_report;
pub mod id;
pub mod math;
pub mod perf_kit;
pub mod random;
pub mod version;
