// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-core`: the foundational primitives every other megu crate
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
//! * [`tier25_guard_batch`] — the Tier-2.5 zero-JNI-idle, `O(1)`-per-tick
//!   bridge between native hot paths and bounded JVM loader hosts:
//!   [`tier25_guard_batch::GuardPredicate`] entry guards (lava/water/
//!   ground/sneak/effect/type/dimension/held-item/slot leaves plus
//!   depth- and node-bounded `And`/`Or`/`Not`) evaluated in pure Rust
//!   before any JNI crossing, and
//!   [`tier25_guard_batch::TickBatchCoalescer`], which queues a tick's
//!   surviving events per `NeoForge`/`Fabric` lane with coalesce-key
//!   dedup and drains them as ONE `TB25` big-endian CRC32-checked flat
//!   frame — 0 JNI calls on idle ticks, 1 on any busy tick.
//! * [`zip_archive`] — lazy, zero-dependency, zero-disk-extraction ZIP/JAR
//!   indexing: [`zip_archive::LazyZipIndex`] reads only the EOCD tail +
//!   Central Directory (ZIP64 included), seeks per entry on demand, resolves
//!   one level of `META-INF/jarjar`/`META-INF/jars` Jar-in-Jar in place, and
//!   inflates with the pure-`std` [`zip_archive::inflate_raw_rfc1951`]
//!   RFC 1951 decompressor under strict fail-closed caps.
//! * [`nbt_wire`] — the canonical, zero-allocation NBT wire skipper shared by
//!   every NBT reader in the tree: [`nbt_wire::skip_network_nbt`] for the
//!   anonymous MC 1.20.2+ network shape (no root name),
//!   [`nbt_wire::skip_named_nbt`] for save-file/older wire roots, and
//!   [`nbt_wire::skip_nbt_payload`] for callers that already consumed the tag
//!   byte. [`nbt_wire::NbtWireLimits`] bounds depth, aggregate tags, list
//!   length and array bytes, rejects negative lengths, `TAG_List(TAG_End,
//!   n > 0)` and every declared length larger than the remaining input, and
//!   uses a fixed frame stack so hostile nesting cannot overflow the process
//!   stack.
//! * [`redstone_eda`] — deterministic bucket-queue propagation for compiled
//!   redstone wire graphs, with per-component topology cache invalidation.
//! * [`diff_engine`] — universal differential-computation primitives so
//!   chunks, GPU buffers, entity state, inventories, and snapshots only
//!   compute, store, and transmit the exact minimal delta `ΔS`: the
//!   [`diff_engine::XorRleDiff`] CRC32-checked `XDF1` skip/literal XOR
//!   codec (zero-alloc `Ok(None)` on identical buffers), the
//!   [`diff_engine::DirtySlotTracker`] shadow-copy span coalescer for flat
//!   GPU/inventory arrays, the [`diff_engine::SubtileDiffMask`] 64-bit
//!   `4³` sub-tile locality mask for `16³` voxel sections, and the
//!   [`diff_engine::FieldDiffBitset`] zero-alloc 64-field dirty tracker.
//! * [`crowd_physics`] — zero-dependency 3D micro-cell indexing, bounded
//!   crowd push/sync/sleep policies, floor-support caching, and per-cell
//!   render/pose budgets for dense entity groups.
//! * [`math_opt_engine`] — four paper-backed, allocation-free mathematical
//!   engines: a [`math_opt_engine::DiscountedUcbAutotuner`] Discounted-UCB
//!   non-stationary bandit (Garivier & Moulines 2011) that detects kernel
//!   regime switches automatically, a
//!   [`math_opt_engine::P2QuantileEstimator`]/`WelfordStats` streaming
//!   P50/P95/P99 tracker (Jain & Chlamtac `P²`, 1985) in `O(1)` memory, a
//!   [`math_opt_engine::RooflineModel`] analytic roofline (Williams,
//!   Waterman & Patterson 2009) plus anti-windup PID frame-budget
//!   controller, and exact 63-bit Morton Z-order keys with the
//!   Mara-McGuire 2013 closed-form projected sphere diameter for
//!   sub-pixel LOD/Hi-Z culling.
//! * [`json`] — the canonical fail-closed RFC 8259 reader/writer:
//!   [`json::parse_json`] / [`json::parse_json_with_limits`] reject malformed
//!   documents with byte-offset [`json::JsonError`]s under bounded depth,
//!   node and byte [`json::JsonLimits`] (plus opt-in duplicate-key
//!   rejection), and [`json::quote_json_string`] /
//!   [`json::JsonValue::to_json_string`] emit one canonical escaping so every
//!   lane serializes identically.
//! * [`crypto`] — the canonical pure-Rust digests the workspace shares:
//!   streaming + one-shot + hex forms of FIPS 180-4 [`crypto::Sha256`] and
//!   [`crypto::Sha1`] and RFC 1321 [`crypto::Md5`], plus the Minecraft
//!   derivations [`crypto::offline_player_uuid`] (UUID v3 over
//!   `"OfflinePlayer:" + name`), [`crypto::minecraft_sha1_hex`] (Mojang's
//!   signed `BigInteger` server hash) and [`crypto::sha256_file`].
//! * [`morton`] — branchless SWAR magic-bit Morton (Z-order) spatial
//!   encoding for cache locality: [`morton::morton3_encode_u4`] /
//!   [`morton::morton3_decode_u4`] give the 12-bit `0..4096` index space
//!   of a `16³` chunk section (L1/L2-friendly 6-neighbour traversal for
//!   lighting, fluid and meshing), [`morton::morton3_encode_u10`] /
//!   [`morton::morton3_decode_u10`] cover 10-bit-per-axis region/entity
//!   spatial hashing, and [`morton::morton2_encode_i16`] /
//!   [`morton::morton2_decode_u32`] give sign-biased 2D chunk keys.
//! * [`simd_voxel`] — runtime-dispatched, hardware-aware low-level
//!   kernels with bit-identical scalar oracles: [`simd_voxel::build_non_air_bitmask_64`]
//!   (AVX-512 masked compare / AVX2 pack+movemask / NEON narrow /
//!   branchless scalar), [`simd_voxel::batch_aabb_frustum_cull_soa`]
//!   (SoA frustum culling emitting packed `u64` visibility words), and
//!   [`simd_voxel::unpack_palette_4bit_to_u16`] (vectorised nibble
//!   expansion + bounds-safe palette gather). Every `#[target_feature]`
//!   path is guarded by `is_x86_feature_detected!` — Core 2 Duo
//!   (x86-64-v1) always lands on the safe scalar tier.
//! * [`linux_desktop`] — zero-dependency Linux/Unix desktop integration:
//!   [`linux_desktop::MeguPaths`] XDG + `--gameDir` path resolution with
//!   freedesktop-spec fallbacks, [`linux_desktop::OmarchyThemePalette`]
//!   live Omarchy `colors.toml` theme sync, and fail-silent
//!   `xdg-open`/`notify-send` helpers that never surface an error to a
//!   beginner on any OS.
//! * [`os_desktop`] — zero-dependency Windows/macOS desktop
//!   integration: [`os_desktop::OsThemePalette`] native theme sync
//!   (Omarchy/macOS/Windows sources), injection-proof AppleScript and
//!   Toast notification builders, `caffeinate` keep-awake args, and the
//!   fail-silent [`os_desktop::detect_os_theme`] entrypoint.
//! * [`autostart_service`] — opt-in cross-platform boot-service plans with
//!   atomic file installation, ownership-checked uninstall and status checks.
//! * [`landlock_sandbox`] — Linux Landlock kernel filesystem sandboxing
//!   (ABI-aware through v7) with graceful unsupported-kernel degradation.
//! * [`self_updater`] — atomic self-update and instant-rollback engine with
//!   strict release selection, SHA-256 verification and launcher pin sync.
//! * [`coords`] — the canonical dimension-aware spatial keys every
//!   subsystem shares: [`coords::DimIndex`] (`u32`, vanilla `0..=2` plus
//!   modded `>= 3`), [`coords::BlockPos`] with floor-division
//!   chunk/section/section-local accessors, [`coords::ChunkKey`] and
//!   [`coords::SectionKey`] (with a full `i32` `sy` for arbitrary world
//!   heights), plus the deterministic zero-DoS [`coords::CoordHasher`]
//!   and [`coords::CoordMap`]/[`coords::CoordSet`] aliases.
//! * [`lz4`] — the single canonical bounded LZ4 block codec:
//!   [`lz4::compress_block`] / [`lz4::compress_block_into`] and the
//!   fail-closed [`lz4::decompress_block`] /
//!   [`lz4::decompress_block_into`] / [`lz4::decompress_block_exact`],
//!   which reject truncated streams, out-of-range match offsets and any
//!   output past the caller's `max_output` cap *before* writing it.
//! * [`section_rev`] — [`section_rev::SharedSectionRevTable`], the one
//!   shared invalidation spine lighting, meshing, spatial audio, redstone
//!   netlists, writeback and chunk wire caches all subscribe to: O(1)
//!   [`section_rev::SharedSectionRevTable::bump_block`] bumps the owning
//!   section, its chunk aggregate and every border-adjacent neighbour
//!   section, keyed by the [`coords`] types so no duplicate revision
//!   scheme can drift out of sync.
//! * [`zero_change_sim`] — zero-allocation simulation kernels that are
//!   bit-exact drop-ins for the entity-heavy per-tick stages:
//!   [`zero_change_sim::PathFinderScratch`] swaps the pathfinder's
//!   per-call `BTreeMap`/`BTreeSet`/`BinaryHeap` for a
//!   generation-stamped flat record table and a re-warmed heap while
//!   preserving the exact `(f, h, position)` expansion order — plus a
//!   *certified* straight-line fast path that only fires when it can
//!   prove the A* result, with a [`zero_change_sim::LosCache`]
//!   section-rev line-of-sight memo —
//!   [`zero_change_sim::ReusableCellMerger`] runs the exact-order XP-orb
//!   and dropped-item merges over pooled spatial cells with a
//!   generation-stamped `eaten` bitset and a full-stack pre-filter, and
//!   [`zero_change_sim::RestingEntitySleepState`] gives resting items,
//!   orbs and lodged projectiles exact-tick sleep keyed on
//!   `(block_pos, section_rev)`, waking on the precise tick a floor
//!   breaks, fluid arrives or an impulse lands while age/despawn/pickup
//!   ticks keep running untouched.
//! * [`block_flags_lut`] — hot-loop lookup structures that replace
//!   per-tick string comparisons with direct-indexed tables: a
//!   [`block_flags_lut::BlockFlagsLut`] `state_id → u64` flag table (18
//!   block flags plus packed 4-bit emission/opacity nibbles, so
//!   `reg.name_of(state) == "minecraft:fire"` collapses to
//!   `(flags & MASK) != 0`), and a
//!   [`block_flags_lut::SectionBitTree4096`] 520-byte hierarchical bit
//!   index that iterates a section's sparse predicates in ascending order
//!   while skipping empty sections in O(1).

#![deny(unsafe_code)]

pub mod autostart_service;
pub mod bits;
pub mod block_flags_lut;
pub mod coords;
pub mod crash_report;
pub mod crowd_physics;
pub mod crypto;
pub mod diff_engine;
pub mod dye_and_items;
pub mod fault_isolation;
pub mod geom;
pub mod hash;
pub mod hw_adaptive_engine;
pub mod id;
pub mod i18n;
pub mod json;
pub mod landlock_sandbox;
pub mod linux_desktop;
pub mod lz4;
pub mod math;
pub mod math_opt_engine;
pub mod math_opt_foundation;
pub mod morton;
pub mod nbt_wire;
pub mod os_desktop;
pub mod perf_kit;
pub mod random;
pub mod redstone_eda;
pub mod romaji_ime;
pub mod self_updater;
pub mod section_rev;
pub mod server_perf_governors;
pub mod sim_graph;
pub mod simd_voxel;
pub(crate) mod stats;
pub mod sys_metrics;
pub mod task_pool;
pub mod tier25_guard_batch;
pub mod varint;
pub mod version;
pub mod zero_change_sim;
pub mod zip_archive;
