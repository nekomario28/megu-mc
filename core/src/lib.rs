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
//! * [`rev`] — the universal zero-allocation revision spine for UI and
//!   retained rendering: [`rev::Revision`] monotonic generation tokens,
//!   the fixed-capacity [`rev::LayerRevBus`] dirty-layer bus carrying
//!   the canonical [`rev::UiLayer`] render stack (backdrop → mod
//!   overlay, with dedicated `ModHud`/`ModOverlay` layers so modded
//!   surfaces never re-tessellate the vanilla HUD), and
//!   [`rev::RetainedLayerTracker`], whose [`rev::DirtyLayers`] bitmask
//!   makes a no-change frame cost 0 rebuilds, 0 heap allocs and 0 GPU
//!   uploads.
//! * [`registry`] — the universal namespaced registry spine vanilla and
//!   mods share: [`registry::NamespacedId`] `namespace:path` identity
//!   validated once through [`id`], dense [`registry::RegistryId`]
//!   handles for zero-string-hash hot paths, and
//!   [`registry::UniversalRegistry`] with deterministic insertion order,
//!   duplicate rejection, a post-load [`registry::UniversalRegistry::freeze`]
//!   and a 4-byte [`registry::UniversalRegistry::digest_crc32`]
//!   fingerprint for server ↔ client registry-sync checks.
//! * [`ui_contract`] — the canonical Vanilla 1.21.1 UI theme and
//!   declarative mod/vanilla UI contract: [`ui_contract::Rgba8`] plus
//!   the [`ui_contract::VANILLA_1_21_1`] token table (stone bevels,
//!   recessed slots, tooltip gradient, exact text/shadow colors and
//!   widget metrics), the vanilla-exact
//!   [`ui_contract::compute_gui_scale`] integer-scale rule, and the
//!   bounded [`ui_contract::ModUiManifest`] /
//!   [`ui_contract::UiWidgetSpec`] declarative tree every Fabric,
//!   NeoForge, Create and WASM surface emits — with title-screen purity
//!   kept as a type-level invariant.
//! * [`intern`] — [`intern::StringInterner`]/[`intern::Symbol`] string
//!   deduplication for O(1) `u32` ID equality/hashing, plus
//!   [`intern::GenArena`]/[`intern::GenIndex`], a dense generational arena
//!   giving ABA-free O(1) insert/remove/lookup entity, mod-object, UI
//!   widget and session handles with no dangling IDs.
//! * [`bvh`] — [`bvh::DynamicBvh`], a Box2D/Bullet-style incremental-SAH
//!   dynamic AABB tree with fattened leaves for moving entities and
//!   contraptions: O(log N) insert/remove/query, a `NaN`-safe slab-test
//!   [`bvh::Aabb3::ray_intersects`], stable generational [`bvh::BvhNodeId`]s,
//!   and caller-scratch zero-alloc AABB/ray queries that answer on *tight*
//!   bounds — bit-identical to a brute-force scan.
//! * [`dep_graph`] — [`dep_graph::IncrementalDag`], an Adapton/Salsa-style
//!   dependency DAG for derived computation (`SectionBlocks → Light →
//!   Mesh`): cycle-rejecting edges, digest-driven [`dep_graph::IncrementalDag::mark_input_changed`]
//!   invalidation, [`dep_graph::IncrementalDag::recompute_with_cutoff`]
//!   early-cutoff verification that rescues untouched downstream
//!   subgraphs, and zero-alloc topological [`dep_graph::IncrementalDag::dirty_order_into`].
//! * [`mesh_elision`] — fail-closed vanilla/mod render-trait occlusion
//!   classification for 100% buried-mesh rebuild elision, shared content-hash
//!   section meshes through canonical FNV-1a/CRC32 fingerprints, and a
//!   frame-paced GPU upload ring that bounds per-frame staging bytes without
//!   starving oversized sections.
//! * [`dst_oracle`] — TigerStyle deterministic simulation fuzzing with a
//!   SplitMix64 operation stream, exact item-conservation and dimension-
//!   membership invariants, canonical LZ4 restart archives, and SHA-256 Merkle
//!   root equality with exact divergent-chunk pinpointing.
//! * [`bloom`] — [`bloom::BlockedBloomFilter`], a 64-byte cache-line
//!   blocked Bloom filter probing exactly one L1 line per lookup via
//!   Kirsch–Mitzenmacher double hashing over canonical [`hash::fnv1a64`],
//!   and [`bloom::VoxelBitset4096`], a 520-byte two-level bitset whose
//!   `u64` summary word makes `is_empty`/`for_each_set` O(popcount) for
//!   `16³` section occupancy.
//! * [`ring_spsc`] — cache-line-padded single-producer/single-consumer
//!   primitives: [`ring_spsc::spsc_u64`]'s true wait-free `AtomicU64`
//!   `Acquire`/`Release` ring ([`ring_spsc::SpscU64Producer`] /
//!   [`ring_spsc::SpscU64Consumer`]) for audio/network/wakeup tokens, the
//!   generic [`ring_spsc::spsc_channel`], [`ring_spsc::CachePadded`], and
//!   [`ring_spsc::BoundedRing`], an O(1) fixed-capacity FIFO replacing
//!   `Vec::remove(0)`.
//! * [`search_index`] — Unicode-safe trigram inverted search with ranked
//!   prefix/boundary/substring/subsequence matching and reusable query
//!   scratch for recipes, settings, commands, catalogs, items, and mods.
//! * [`timing_wheel`] — a hierarchical Varghese & Lauck timing wheel with
//!   O(1) scheduling, generation-checked O(1) cancellation, exact-tick
//!   cascading, and an overflow horizon for long-delayed callbacks.
//! * [`union_find`] — Tarjan disjoint-set union with path halving and
//!   union-by-rank for kinetic/pipe/multiblock/claim/room connectivity.
//! * [`merkle_cas`] — domain-separated SHA-256 Merkle trees with subtree-
//!   pruning key diffs plus an LZ4-compressed content-addressed blob store.
//! * [`sparse_set`] — Briggs & Torczon sparse-set storage whose live values
//!   stay dense and cache-contiguous for components, ticking objects, and
//!   active widget tables.
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
//! * [`quiescent`] — a zero-work quiescent chunk scheduler:
//!   [`quiescent::QuiescentChunkScheduler`] stores only chunks whose
//!   [`quiescent::ActivityMask`] is non-zero in the canonical `SparseSet`
//!   (idle chunks are physically absent — `O(0)`, never visited in the
//!   tick loop), delayed wakeups ride the canonical `TimingWheel`, and
//!   [`quiescent::QuiescentChunkScheduler::on_block_mutated`] bumps the
//!   canonical `SharedSectionRevTable` while waking border-adjacent
//!   neighbour columns.
//! * [`bounded`] — compile-time bug prevention:
//!   [`bounded::BoundedMap`]'s `const MAX` makes unbounded registries
//!   structurally impossible (deterministic `O(1)` lookup, oldest-write
//!   eviction at capacity), `#[must_use]` [`bounded::LootSeedToken`] /
//!   [`bounded::SpawnerDrawToken`] / [`bounded::MustConsume`] turn dropped
//!   critical draws into compile errors, and [`bounded::DimBound`] blocks
//!   cross-dimension coordinate mix-ups.
//! * [`local_transport`] — a zero-socket, zero-copy integrated-singleplayer
//!   loopback: [`local_transport::local_pair`] gives a full-duplex lane
//!   built on the canonical `WaitFreeSpscU64` rings plus shared `Arc<T>`
//!   slot tables, so payloads cross server ↔ client threads as bare
//!   pointers (`Arc::ptr_eq`-identical) with `Full`/`Disconnected`
//!   backpressure and no serialization, compression or copies.
//! * [`net_budget`] — symmetric streaming bandwidth savers:
//!   [`net_budget::EncodeOnceFrameCache`] encodes a chunk frame once and
//!   hands the same `Arc<[u8]>` to every player, invalidated the instant
//!   `SharedSectionRevTable::chunk_rev` moves, and
//!   [`net_budget::classify_perceptual_band`] +
//!   [`net_budget::should_emit_tick`] cut entity/chunk update rates to
//!   20/10/4/0 Hz by frustum, distance and motion — usable by both the
//!   server streamer and the client LOD lane.
//! * [`render_cs`] — the five foundational graphics/GPU primitives the
//!   Vanilla renderer and modded subworlds share: [`render_cs::SectionVisBits`]
//!   8-byte portal connectivity with the Tommo/Sodium no-backtrack
//!   [`render_cs::bfs_visible_sections`], [`render_cs::visible_face_mask`]
//!   six-axis half-space culling feeding [`render_cs::SixAxisBuckets`]
//!   (with a never-culled `UnculledCustom` bucket for protruding/diagonal
//!   mod geometry), [`render_cs::BinaryGreedyMesher`] bit-parallel greedy
//!   meshing, [`render_cs::TranslucentSortCache`] half-block-quantized lazy
//!   radix sorting, and [`render_cs::RenderGraph`] dead-pass culling with
//!   linear-scan transient VRAM aliasing — all fail-closed for modded
//!   blocks whose visuals leave the `1×1×1` cube.
//! * [`break_progress`] — the canonical Vanilla `getDestroyProgress` model:
//!   [`break_progress::destroy_progress_per_tick`]'s
//!   `speed / hardness / (can_harvest ? 30 : 100)` formula, the
//!   [`break_progress::instant_break`] predicate, closed-form
//!   [`break_progress::ticks_to_break`], and the reusable
//!   [`break_progress::BreakProgressModel`] accumulator every player,
//!   machine, and prediction-side dig shares.
//! * [`anticheat_v2`] — bounded-memory competitive-survival anti-cheat:
//!   [`anticheat_v2::TransactionEchoQueue`] confirmed-RTT transaction
//!   tracking, [`anticheat_v2::HitboxRewindBuffer`] lag-compensated
//!   NaN-safe ray-reach validation, [`anticheat_v2::PhysicsPredictionStep`]
//!   with [`anticheat_v2::AcModGrantToken`] mod-declared motion,
//!   [`anticheat_v2::BehavioralCusumDetector`] Page-CUSUM/EWMA cadence
//!   analysis, and [`anticheat_v2::ProtocolInvariantFsm`] O(1)
//!   packet-order invariants.

#![deny(unsafe_code)]

pub mod anticheat_v2;
pub mod autostart_service;
pub mod bits;
pub mod bloom;
pub mod bounded;
pub mod bvh;
pub mod block_flags_lut;
pub mod break_progress;
pub mod coords;
pub mod crash_report;
pub mod crowd_physics;
pub mod crypto;
pub mod dep_graph;
pub mod diff_engine;
pub mod dst_oracle;
pub mod dye_and_items;
pub mod fault_isolation;
pub mod geom;
pub mod hash;
pub mod hw_adaptive_engine;
pub mod id;
pub mod i18n;
pub mod intern;
pub mod json;
pub mod landlock_sandbox;
pub mod linux_desktop;
pub mod local_transport;
pub mod lz4;
pub mod math;
pub mod math_opt_engine;
pub mod math_opt_foundation;
pub mod merkle_cas;
pub mod mesh_elision;
pub mod morton;
pub mod nbt_wire;
pub mod net_budget;
pub mod os_desktop;
pub mod perf_kit;
pub mod pose_ray;
pub mod quiescent;
pub mod random;
pub mod render_cs;
pub mod registry;
pub mod rev;
pub mod ring_spsc;
pub mod redstone_eda;
pub mod romaji_ime;
pub mod self_updater;
pub mod search_index;
pub mod section_rev;
pub mod server_perf_governors;
pub mod sim_graph;
pub mod simd_voxel;
pub mod slot_coords;
pub mod sparse_set;
pub(crate) mod stats;
pub mod sys_metrics;
pub mod task_pool;
pub mod tier25_guard_batch;
pub mod ui_contract;
pub mod timing_wheel;
pub mod union_find;
pub mod varint;
pub mod version;
pub mod world_view;
pub mod xp_curve;
pub mod zero_change_sim;
pub mod zip_archive;

pub use pose_ray::{view_ray, PlayerPoseDimensions, PlayerPoseKind, ReachPolicy, ViewRay};
pub use slot_coords::{
    HotbarSlot, MenuInventoryLayout, PlayerMenuSlot, PlayerNbtSlot, SlotIndexError,
};
pub use xp_curve::{
    moon_atlas_uv, moon_phase_from_day_time, total_xp_for_level, xp_for_next_level,
};
