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
//! * [`fault_isolation`] — [`fault_isolation::FaultIsolationRouter`]: a
//!   per-subsystem runtime fault-isolation circuit breaker
//!   (Closed / Throttled / OpenIsolated / HalfOpenProbe) that strides
//!   sustained tick-budget overruns, isolates runaway mods, chunk
//!   regions, contraptions, and builtin subsystems with deterministic
//!   `MEGU-E-XXXXXXXX` blame fingerprints, probes recovery with
//!   exponential backoff, and bounds tracked state so scope floods can
//!   never evict a live isolation or exhaust memory.
//! * [`perf_kit`] — universal zero-overhead performance primitives: a
//!   [`perf_kit::DirtyFingerprintGate`] for O(1) idle skipping,
//!   [`perf_kit::BoundedScratchVec`] reusable scratch storage with bounded
//!   retention, a [`perf_kit::TickBudgetQueue`] for smooth per-tick burst
//!   slicing, an [`perf_kit::EventWakeWheel`] exact-tick wake scheduler, a
//!   [`perf_kit::CoalescedDeltaBatch`] last-write-wins delta collapser, and
//!   an [`perf_kit::AdaptiveLoadGovernor`] hysteresis budget scale — all
//!   generic so any vanilla or modded subsystem can plug in.
//! * [`server_perf_governors`] — deterministic single-node server
//!   optimization governors surpassing Storia/Pumpkin-style tradeoffs
//!   without breaking vanilla parity: a [`server_perf_governors::VelocityViewGovernor`]
//!   anisotropic forward-cone chunk-send planner, a
//!   [`server_perf_governors::CrowdTickGuard`] prime-stride idle-AI guard
//!   that never skips physics/damage, and a
//!   [`server_perf_governors::CompressedChunkPacketCache`] epoch-tagged
//!   zero-copy shared packet cache with bounded LRU eviction.
//! * [`sys_metrics`] — a fail-safe system & performance telemetry
//!   snapshotter for the opt-in Advanced MiniHUD and server TUI: pure
//!   `/proc` + `/sys/class/drm` parsers for RAM/CPU/GPU with `None`
//!   fallback off Linux, a [`sys_metrics::FrameTimingStats`] rolling
//!   FPS/1%-low/MSPT ring buffer, and the
//!   [`sys_metrics::MiniHudConfig`] + [`sys_metrics::format_mini_hud_lines`]
//!   selectable-overlay formatter. No clocks, no panics, no external
//!   crates.
//! * [`romaji_ime`] — a clean-room, opt-in LunaChat-style romaji → kana
//!   chat converter: [`romaji_ime::romaji_to_hiragana`] covers
//!   Hepburn/Kunrei rows, youon, sokuon, small kana and `ん` rules, and
//!   [`romaji_ime::format_romaji_chat`] adds the smart bypass pipeline
//!   (commands, escape prefix, existing Japanese, URLs, mentions,
//!   numerics, all-caps acronyms) with a vanilla-safe 256-char bound.
//!   [`romaji_ime::RomajiChatMode::Off`] is the strict vanilla default.

#![deny(unsafe_code)]

pub mod crash_report;
pub mod dye_and_items;
pub mod fault_isolation;
pub mod hw_adaptive_engine;
pub mod id;
pub mod i18n;
pub mod math;
pub mod math_opt_foundation;
pub mod perf_kit;
pub mod random;
pub mod romaji_ime;
pub mod server_perf_governors;
pub mod sys_metrics;
pub mod version;
