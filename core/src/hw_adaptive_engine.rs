// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc_spine_core::hw_adaptive_engine` — a pure-`std`, zero-dependency,
//! LLM-inference-inspired and hardware-adaptive ultra-lightweight compute
//! toolkit for the voxel engine hot paths.
//!
//! The five primitives in this module borrow the structural tricks that
//! made modern LLM serving stacks fast and re-apply them to chunk data:
//!
//! * [`detect_host_hardware_caps`] + [`select_compute_strategy`] — a real
//!   host hardware probe (Linux sysfs L3 cache size, `cpu_capacity` hybrid
//!   core detection, `available_parallelism`, runtime SIMD feature
//!   detection, `/sys/class/drm` VRAM topology hint) feeding an adaptive
//!   [`ComputeStrategy`]: FlashAttention-style recompute-over-bandwidth on
//!   cache-poor hosts, KV-cache-style LUT batching on large-L3 hosts such
//!   as 3D V-Cache parts, unified-memory zero-copy vs staged-arena transfer
//!   plans, and a foreground/background [`WorkerSplitPlan`].
//! * [`max_nibble_arrays_adaptive`] — a bit-exact packed 4-bit
//!   element-wise max over two 2048-byte light buffers (4096 nibbles each),
//!   with both a resident LUT path (cache-rich) and a pure-safe-Rust
//!   `u64`-SWAR recompute path (cache-poor) that auto-vectorizes cleanly
//!   under every [`SimdKernelTier`]. Every dispatch path shares the same
//!   primitive, so bit-exact parity is structural rather than accidental.
//! * [`PagedSectionPool`] — a vLLM-style fixed-page pool: 4 KiB section
//!   pages in one contiguous slab, O(1) free-list reuse, copy-on-write
//!   prefix sharing so a thousand identical air/stone sections cost a
//!   single physical page each, and generation-checked handles that fail
//!   closed on stale or double-released references.
//! * [`SparseDirtyTileMask`] — a sparse-attention-style 64-bit bitmap over
//!   the 64 `4×4×4` sub-tiles of a `16³` chunk section, with O(popcount)
//!   draining via `trailing_zeros` so only dirty sub-voxels are visited.
//! * [`QuantizedVoxelVertex`] — an LLM-quantization-style 8-byte packed
//!   mesh vertex replacing the usual 32–40 byte vertex, with exact
//!   bounds-checked `pack`/`unpack` round-tripping.
//! * [`SpeculativeChunkPrefetcher`] — speculative-decoding-style chunk
//!   prefetch: emit a prioritized forward cone of chunk coordinates along
//!   the player's velocity, and invalidate every stale entry in O(1) via a
//!   generation bump when the motion vector sharply reverses.
//!
//! Everything here is pure `std`, panic-free, clock-free and unsafe-free:
//! detection degrades gracefully off Linux, pool handles carry generation
//! counters, and every packed-layout write is bounds-checked fail-closed.

use std::collections::{HashMap, VecDeque};
use std::fmt;

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;

// ===========================================================================
// 1. Host hardware detection & adaptive compute strategy
// ===========================================================================

/// L3 sizes at or above this select [`CacheStrategy::LargeL3LutBatch`]:
/// 64 MiB is the classic 3D V-Cache / Epyc-X cutoff where a resident LUT
/// pays for itself many times over.
pub const L3_LARGE_KIB: u32 = 65_536;

/// L3 sizes at or above this select [`CacheStrategy::BalancedTile`].
pub const L3_MEDIUM_KIB: u32 = 16_384;

/// Total reported VRAM at or above this is treated as a discrete-GPU hint.
/// Integrated parts typically report a carve-out below 2 GiB, while real
/// discrete cards start at 4 GiB and up.
const DISCRETE_VRAM_HINT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Upper bound on sysfs `indexN` cache directories scanned.
const MAX_CACHE_INDEX: u32 = 8;

/// Upper bound on `/sys/class/drm/cardN` entries scanned for VRAM hints.
const MAX_DRM_CARDS_SCANNED: u32 = 8;

/// Upper bound on CPUs scanned for `cpu_capacity` heterogeneity.
const MAX_CPUS_SCANNED: u32 = 128;

/// The instruction-set kernel tier a dispatch should schedule for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SimdKernelTier {
    /// AVX-512 class x86_64 host (runtime `avx512f` feature detected).
    Avx512,
    /// AVX2 class x86_64 host (runtime `avx2` feature detected).
    Avx2,
    /// AArch64 host — NEON is architecturally mandatory.
    Neon,
    /// Portable scalar fallback for everything else.
    Scalar,
}

/// How a host's memory is physically attached — caller-overridable when
/// the driver topology is known ahead of the sysfs heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryTopology {
    /// CPU and GPU share one physical address space (Apple Silicon, iGPU
    /// APUs): zero-copy mapping is legal and cheapest.
    UnifiedUma,
    /// Device-private VRAM behind a bus: transfers must be staged.
    DiscreteVram,
}

/// Measured hardware capabilities of the machine we are running on.
/// Everything is fail-safe: undetectable facts degrade to neutral values
/// (`l3_cache_kib = 0`, `hybrid_cores = false`) rather than errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostHardwareCaps {
    /// Logical CPU count from `available_parallelism` (>= 1).
    pub logical_cores: u32,
    /// Highest-level CPU cache size in KiB (`0` = unknown). On a 96 MiB
    /// 3D V-Cache part this reports `98304`.
    pub l3_cache_kib: u32,
    /// `true` when heterogeneous core capacities were observed
    /// (P-core/E-core hybrids, ARM big.LITTLE).
    pub hybrid_cores: bool,
    /// Best SIMD kernel tier detected at runtime.
    pub simd_tier: SimdKernelTier,
    /// Unified-memory vs discrete-VRAM hint.
    pub memory_topology: MemoryTopology,
}

impl HostHardwareCaps {
    /// Return a copy with the memory topology overridden by the caller,
    /// for cases where the driver reports it more reliably than sysfs.
    pub fn with_memory_topology(mut self, topology: MemoryTopology) -> Self {
        self.memory_topology = topology;
        self
    }
}

/// Cache-resident strategy chosen from the L3 size, mirroring how LLM
/// runtimes pick between KV-cache retention and FlashAttention recompute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CacheStrategy {
    /// Huge L3 (>= 64 MiB): keep lookup tables resident and batch four
    /// chunk columns per working edge.
    LargeL3LutBatch,
    /// Moderate L3 (>= 16 MiB): tiled batching of two columns.
    BalancedTile,
    /// Small/unknown L3: recompute on the fly instead of trusting the
    /// cache — the FlashAttention recompute-over-bandwidth trade.
    FlashRecomputeStream,
}

impl CacheStrategy {
    /// Chunk columns batched along a working edge before results spill.
    pub const fn chunk_batch_edge(self) -> u32 {
        match self {
            Self::LargeL3LutBatch => 4,
            Self::BalancedTile => 2,
            Self::FlashRecomputeStream => 1,
        }
    }

    /// `true` when lookup tables should be kept resident; `false` when
    /// recompute (SWAR) beats trusting cache residency.
    pub const fn prefer_lut_over_recompute(self) -> bool {
        match self {
            Self::LargeL3LutBatch | Self::BalancedTile => true,
            Self::FlashRecomputeStream => false,
        }
    }
}

/// How bulk data should cross the CPU/GPU boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryTransferStrategy {
    /// Unified memory: map the same pages, copy nothing.
    UnifiedZeroCopy,
    /// Discrete VRAM: stage through a pinned arena sized for the bus.
    DiscreteStagedArena,
}

/// Split of the logical cores between latency-sensitive foreground work
/// and throughput background work. Both fields are always `>= 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkerSplitPlan {
    /// Threads for the tick/render critical path.
    pub foreground_threads: u32,
    /// Threads for offloaded/background jobs (prefetch, relight, encode).
    pub background_threads: u32,
}

/// A fully-resolved adaptive compute plan for this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputeStrategy {
    /// Cache-residency plan.
    pub cache: CacheStrategy,
    /// Memory-transfer plan.
    pub memory: MemoryTransferStrategy,
    /// SIMD kernel tier to schedule.
    pub simd: SimdKernelTier,
    /// Foreground/background worker split.
    pub workers: WorkerSplitPlan,
}

impl ComputeStrategy {
    /// Chunk columns per batch under the selected cache strategy.
    pub const fn chunk_batch_edge(&self) -> u32 {
        self.cache.chunk_batch_edge()
    }

    /// Whether the adaptive kernels should keep LUTs resident.
    pub const fn prefer_lut_over_recompute(&self) -> bool {
        self.cache.prefer_lut_over_recompute()
    }
}

/// Parse a Linux sysfs cache `size` file (`"98304K"`, `"96M"`, `"32K"`)
/// into KiB. Returns `None` on anything unexpected.
fn parse_sysfs_cache_size_kib(text: &str) -> Option<u32> {
    let t = text.trim();
    let (digits, multiplier) = match t.as_bytes().last() {
        Some(b'K' | b'k') => (&t[..t.len() - 1], 1u64),
        Some(b'M' | b'm') => (&t[..t.len() - 1], 1_024u64),
        Some(b'G' | b'g') => (&t[..t.len() - 1], 1_048_576u64),
        _ => (t, 1u64),
    };
    let n: u64 = digits.trim().parse().ok()?;
    Some(u32::try_from(n.saturating_mul(multiplier)).unwrap_or(u32::MAX))
}

/// Linux: read `/sys/devices/system/cpu/cpu0/cache/indexN/{level,size}`
/// and report the size of the *highest* level cache — on a 9800X3D that
/// is the 96 MiB L3 3D V-Cache slice at `index3`.
#[cfg(target_os = "linux")]
fn detect_last_level_cache_kib() -> Option<u32> {
    let base = Path::new("/sys/devices/system/cpu/cpu0/cache");
    let mut best_level = 0u32;
    let mut best_kib = 0u32;
    for idx in 0..MAX_CACHE_INDEX {
        let dir = base.join(format!("index{idx}"));
        let Ok(level_text) = fs::read_to_string(dir.join("level")) else {
            continue;
        };
        let Ok(level) = level_text.trim().parse::<u32>() else {
            continue;
        };
        // Instruction-only last-level caches do not help data streaming;
        // treat a missing `type` file as acceptable rather than fatal.
        if let Ok(kind) = fs::read_to_string(dir.join("type")) {
            let kind = kind.trim();
            if kind != "Unified" && kind != "Data" {
                continue;
            }
        }
        let Ok(size_text) = fs::read_to_string(dir.join("size")) else {
            continue;
        };
        let Some(kib) = parse_sysfs_cache_size_kib(&size_text) else {
            continue;
        };
        if level > best_level {
            best_level = level;
            best_kib = kib;
        }
    }
    (best_level > 0).then_some(best_kib)
}

/// Off Linux there is no sysfs contract; report "unknown" (`0`).
#[cfg(not(target_os = "linux"))]
fn detect_last_level_cache_kib() -> Option<u32> {
    None
}

/// Linux: heterogeneous cores expose per-CPU `cpu_capacity` files with
/// different values (big.LITTLE / P+E). Uniform or absent files are not
/// evidence of hybrid.
#[cfg(target_os = "linux")]
fn detect_hybrid_cores(logical_cores: u32) -> bool {
    let mut first: Option<u64> = None;
    let scanned = logical_cores.clamp(1, MAX_CPUS_SCANNED);
    for cpu in 0..scanned {
        let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity");
        let Ok(text) = fs::read_to_string(&path) else {
            return false; // No capacity contract at all -> treat as homogeneous.
        };
        let Ok(cap) = text.trim().parse::<u64>() else {
            return false;
        };
        match first {
            None => first = Some(cap),
            Some(f) if f != cap => return true,
            _ => {}
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn detect_hybrid_cores(_logical_cores: u32) -> bool {
    false
}

/// Runtime SIMD tier detection. Pure feature probes — no unsafe, no
/// inline asm, and every non-x86/aarch64 target falls back to scalar.
#[cfg(target_arch = "x86_64")]
fn detect_simd_tier() -> SimdKernelTier {
    if std::arch::is_x86_feature_detected!("avx512f") {
        SimdKernelTier::Avx512
    } else if std::arch::is_x86_feature_detected!("avx2") {
        SimdKernelTier::Avx2
    } else {
        SimdKernelTier::Scalar
    }
}

#[cfg(target_arch = "aarch64")]
fn detect_simd_tier() -> SimdKernelTier {
    SimdKernelTier::Neon
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn detect_simd_tier() -> SimdKernelTier {
    SimdKernelTier::Scalar
}

/// Linux: any DRM card reporting >= 2 GiB of device VRAM is treated as a
/// discrete GPU; below the hint threshold (or absent) we assume UMA.
#[cfg(target_os = "linux")]
fn detect_memory_topology() -> MemoryTopology {
    for card in 0..MAX_DRM_CARDS_SCANNED {
        let path = format!("/sys/class/drm/card{card}/device/mem_info_vram_total");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(bytes) = text.trim().parse::<u64>() {
            if bytes >= DISCRETE_VRAM_HINT_BYTES {
                return MemoryTopology::DiscreteVram;
            }
        }
    }
    MemoryTopology::UnifiedUma
}

/// Apple Silicon is architecturally UMA; everything else non-Linux keeps
/// the conservative staged-arena assumption.
#[cfg(not(target_os = "linux"))]
fn detect_memory_topology() -> MemoryTopology {
    #[cfg(target_os = "macos")]
    {
        MemoryTopology::UnifiedUma
    }
    #[cfg(not(target_os = "macos"))]
    {
        MemoryTopology::DiscreteVram
    }
}

/// Probe the real host. Never fails, never panics, never reads a clock:
/// every unreadable sysfs file degrades to a neutral default.
pub fn detect_host_hardware_caps() -> HostHardwareCaps {
    let logical_cores = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1)
        .max(1);
    HostHardwareCaps {
        logical_cores,
        l3_cache_kib: detect_last_level_cache_kib().unwrap_or(0),
        hybrid_cores: detect_hybrid_cores(logical_cores),
        simd_tier: detect_simd_tier(),
        memory_topology: detect_memory_topology(),
    }
}

/// Split `logical_cores` into foreground/background workers. On hybrid
/// parts the background half soaks the little cores; otherwise a quarter
/// goes to background. Both sides always get at least one thread.
fn plan_workers(logical_cores: u32, hybrid: bool) -> WorkerSplitPlan {
    let cores = logical_cores.max(1);
    let background = if hybrid { cores / 2 } else { cores / 4 }.max(1);
    let foreground = cores.saturating_sub(background).max(1);
    WorkerSplitPlan {
        foreground_threads: foreground,
        background_threads: background,
    }
}

/// Resolve a [`HostHardwareCaps`] snapshot into a concrete
/// [`ComputeStrategy`]. Pure and total — same caps, same strategy.
pub fn select_compute_strategy(caps: &HostHardwareCaps) -> ComputeStrategy {
    let cache = if caps.l3_cache_kib >= L3_LARGE_KIB {
        CacheStrategy::LargeL3LutBatch
    } else if caps.l3_cache_kib >= L3_MEDIUM_KIB {
        CacheStrategy::BalancedTile
    } else {
        CacheStrategy::FlashRecomputeStream
    };
    let memory = match caps.memory_topology {
        MemoryTopology::UnifiedUma => MemoryTransferStrategy::UnifiedZeroCopy,
        MemoryTopology::DiscreteVram => MemoryTransferStrategy::DiscreteStagedArena,
    };
    ComputeStrategy {
        cache,
        memory,
        simd: caps.simd_tier,
        workers: plan_workers(caps.logical_cores, caps.hybrid_cores),
    }
}

// ===========================================================================
// Bit-exact adaptive light/occupancy kernel (packed 4-bit max)
// ===========================================================================

/// Byte length of one packed light/occupancy buffer: 4096 nibbles.
pub const LIGHT_BUFFER_BYTES: usize = 2_048;

/// Number of packed 4-bit lanes in one buffer.
pub const LIGHT_NIBBLES: usize = LIGHT_BUFFER_BYTES * 2;

/// `u64` words per buffer.
const LIGHT_WORDS: usize = LIGHT_BUFFER_BYTES / 8;

/// Low 3 bits of every nibble lane.
const NIBBLE_LO3: u64 = 0x7777_7777_7777_7777;
/// High bit of every nibble lane.
const NIBBLE_HI: u64 = 0x8888_8888_8888_8888;
/// A single `1` in every nibble lane.
const NIBBLE_ONE: u64 = 0x1111_1111_1111_1111;

/// Nibble-wise max of two packed `u64` lanes — pure SWAR, no unsafe.
///
/// For each nibble `i`, `ge3` computes `((a_i & 7) | 8) - (b_i & 7)` which
/// stays inside `[1, 15]`, so no borrow ever crosses a lane boundary and
/// its high bit is exactly `a_i_low3 >= b_i_low3`. The full per-lane
/// `a_i >= b_i` predicate is then `(a_hi & !b_hi) | (eq_hi & ge3)`, and
/// expanding that single bit to a 4-bit select mask picks the winner.
#[inline]
fn nibble_max_word(a: u64, b: u64) -> u64 {
    let ge3 = ((a & NIBBLE_LO3) | NIBBLE_HI).wrapping_sub(b & NIBBLE_LO3) & NIBBLE_HI;
    let ge = ((a & !b) | (!(a ^ b) & ge3)) & NIBBLE_HI;
    let ones = (ge >> 3) & NIBBLE_ONE;
    let sel = ones | (ones << 1) | (ones << 2) | (ones << 3);
    (a & sel) | (b & !sel)
}

/// 64 KiB const-evaluated LUT: `LUT[(a << 8) | b]` = nibble-wise max byte.
/// This is the KV-cache-style resident table the `prefer_lut` path uses on
/// hosts with an L3 large enough to keep it hot.
static NIBBLE_MAX_LUT: [u8; 65_536] = build_nibble_max_lut();

const fn build_nibble_max_lut() -> [u8; 65_536] {
    let mut lut = [0u8; 65_536];
    let mut i = 0usize;
    while i < lut.len() {
        let a = (i >> 8) as u8;
        let b = i as u8;
        let lo = if (a & 0x0F) >= (b & 0x0F) {
            a & 0x0F
        } else {
            b & 0x0F
        };
        let hi = if (a >> 4) >= (b >> 4) { a >> 4 } else { b >> 4 };
        lut[i] = lo | (hi << 4);
        i += 1;
    }
    lut
}

/// Reinterpret a 2048-byte buffer as 256 little-endian `u64` words.
fn bytes_to_words(buf: &[u8; LIGHT_BUFFER_BYTES]) -> [u64; LIGHT_WORDS] {
    let (chunks, rest) = buf.as_chunks::<8>();
    debug_assert!(rest.is_empty());
    let mut words = [0u64; LIGHT_WORDS];
    for (dst, chunk) in words.iter_mut().zip(chunks.iter()) {
        *dst = u64::from_le_bytes(*chunk);
    }
    words
}

/// SWAR pass with an unroll width chosen per [`SimdKernelTier`]. Every
/// tier shares [`nibble_max_word`], so outputs are bit-identical by
/// construction; `UNROLL` only shapes the loop the autovectorizer sees.
fn swar_nibble_max<const UNROLL: usize>(
    a: &[u64; LIGHT_WORDS],
    b: &[u64; LIGHT_WORDS],
    out: &mut [u64; LIGHT_WORDS],
) {
    debug_assert_eq!(LIGHT_WORDS % UNROLL, 0);
    let mut i = 0;
    while i < LIGHT_WORDS {
        let mut j = 0;
        while j < UNROLL {
            out[i + j] = nibble_max_word(a[i + j], b[i + j]);
            j += 1;
        }
        i += UNROLL;
    }
}

/// Element-wise packed 4-bit max over two 2048-byte light buffers with an
/// explicit tier/LUT choice. `prefer_lut` selects the resident-table path;
/// `tier` selects the SWAR recompute unroll when LUT is not preferred.
///
/// Both paths are provably bit-exact: the LUT is the exhaustive table of
/// the same per-nibble max the SWAR lanes compute.
pub fn max_nibble_arrays_tiered(
    a: &[u8; LIGHT_BUFFER_BYTES],
    b: &[u8; LIGHT_BUFFER_BYTES],
    tier: SimdKernelTier,
    prefer_lut: bool,
) -> [u8; LIGHT_BUFFER_BYTES] {
    let mut out = [0u8; LIGHT_BUFFER_BYTES];
    if prefer_lut {
        for i in 0..LIGHT_BUFFER_BYTES {
            out[i] = NIBBLE_MAX_LUT[((a[i] as usize) << 8) | b[i] as usize];
        }
        return out;
    }
    let aw = bytes_to_words(a);
    let bw = bytes_to_words(b);
    let mut words = [0u64; LIGHT_WORDS];
    match tier {
        SimdKernelTier::Avx512 => swar_nibble_max::<8>(&aw, &bw, &mut words),
        SimdKernelTier::Avx2 => swar_nibble_max::<4>(&aw, &bw, &mut words),
        SimdKernelTier::Neon => swar_nibble_max::<2>(&aw, &bw, &mut words),
        SimdKernelTier::Scalar => swar_nibble_max::<1>(&aw, &bw, &mut words),
    }
    let (out_chunks, rest) = out.as_chunks_mut::<8>();
    debug_assert!(rest.is_empty());
    for (dst, word) in out_chunks.iter_mut().zip(words.iter()) {
        *dst = word.to_le_bytes();
    }
    out
}

/// Fully adaptive entry point: dispatch on the resolved
/// [`ComputeStrategy`] — LUT-resident on cache-rich hosts, SWAR recompute
/// shaped by the detected [`SimdKernelTier`] on cache-poor ones.
pub fn max_nibble_arrays_adaptive(
    a: &[u8; LIGHT_BUFFER_BYTES],
    b: &[u8; LIGHT_BUFFER_BYTES],
    strategy: &ComputeStrategy,
) -> [u8; LIGHT_BUFFER_BYTES] {
    max_nibble_arrays_tiered(a, b, strategy.simd, strategy.prefer_lut_over_recompute())
}

// ===========================================================================
// 2. vLLM-inspired PagedSectionPool (4 KiB pages + CoW prefix sharing)
// ===========================================================================

/// Fixed page payload size — one 16³ section's packed bytes.
pub const PAGE_SIZE: usize = 4_096;

/// A lease on one physical page inside a [`PagedSectionPool`].
///
/// Handles are validated against a per-page generation counter, so a
/// stale handle (already released, or consumed by a copy-on-write) fails
/// closed instead of aliasing whatever page reused the slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageHandle {
    index: u32,
    generation: u32,
}

/// Errors from fallible pool operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagePoolError {
    /// The handle is stale, already released, or never belonged to us.
    InvalidHandle,
    /// Byte index outside `0..PAGE_SIZE`.
    IndexOutOfBounds {
        /// The offending index.
        index: usize,
    },
}

impl fmt::Display for PagePoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHandle => write!(f, "page handle is stale or invalid"),
            Self::IndexOutOfBounds { index } => {
                write!(f, "page byte index {index} out of range 0..{PAGE_SIZE}")
            }
        }
    }
}

impl std::error::Error for PagePoolError {}

/// A snapshot of how well the pool is compressing logical sections into
/// physical pages.
#[derive(Clone, Copy, Debug)]
pub struct PoolStats {
    /// Live outstanding handles (logical sections).
    pub logical_sections: usize,
    /// Live physical pages (`refcount >= 1`).
    pub physical_pages: usize,
    /// Live pages shared by more than one logical section.
    pub shared_pages: usize,
    /// Physical pages parked in the free list awaiting reuse.
    pub free_pages: usize,
    /// `logical_sections / physical_pages`; `0.0` while the pool is empty.
    /// 1 500 uniform sections on 2 pages report `750.0`.
    pub compression_ratio: f64,
}

/// Sentinel in `canonical_fill` marking a page as *not* the canonical
/// uniform page for any fill byte.
const NOT_CANONICAL: u16 = u16::MAX;

/// vLLM-style fixed-page section pool.
///
/// All 4 KiB pages live in one contiguous `Vec` slab: allocation pops a
/// free-list slot or extends the slab once, so per-chunk traffic causes
/// zero heap fragmentation. `alloc_uniform` makes every section with the
/// same fill byte share *one* canonical physical page (refcounted);
/// `write_byte_cow` privatizes a page only at the moment a sharer writes,
/// exactly like vLLM's CoW on shared prompt prefixes.
#[derive(Debug)]
pub struct PagedSectionPool {
    slab: Vec<[u8; PAGE_SIZE]>,
    refcounts: Vec<u32>,
    generations: Vec<u32>,
    free_list: Vec<u32>,
    /// Canonical uniform page per fill byte (refcount >= 1 by invariant).
    canonical: HashMap<u8, u32>,
    /// Reverse of `canonical`: which fill byte each page is canonical for.
    canonical_fill: Vec<u16>,
    logical_sections: usize,
}

impl Default for PagedSectionPool {
    fn default() -> Self {
        Self::new()
    }
}

impl PagedSectionPool {
    /// An empty pool with no slab allocation yet.
    pub fn new() -> Self {
        Self {
            slab: Vec::new(),
            refcounts: Vec::new(),
            generations: Vec::new(),
            free_list: Vec::new(),
            canonical: HashMap::new(),
            canonical_fill: Vec::new(),
            logical_sections: 0,
        }
    }

    /// Resolve a handle to its live slab index, failing closed on stale,
    /// released, or never-issued handles.
    fn resolve(&self, handle: PageHandle) -> Option<usize> {
        let i = handle.index as usize;
        if i < self.slab.len() && self.generations[i] == handle.generation && self.refcounts[i] > 0
        {
            Some(i)
        } else {
            None
        }
    }

    /// Take a physical page — from the free list when possible (generation
    /// bumps so old handles cannot alias the recycled slot), else grow
    /// the slab once. The page comes back with `refcount = 1`.
    fn claim_page(&mut self) -> usize {
        if let Some(idx) = self.free_list.pop() {
            let i = idx as usize;
            // Skip the poison generation so released handles can never
            // alias a recycled page even after u32 wraparound.
            let mut gen = self.generations[i].wrapping_add(1);
            if gen == u32::MAX {
                gen = 0;
            }
            self.generations[i] = gen;
            self.refcounts[i] = 1;
            self.canonical_fill[i] = NOT_CANONICAL;
            i
        } else {
            self.slab.push([0u8; PAGE_SIZE]);
            self.refcounts.push(1);
            self.generations.push(0);
            self.canonical_fill.push(NOT_CANONICAL);
            self.slab.len() - 1
        }
    }

    /// If `i` is still registered as the canonical page for its fill
    /// byte, de-register it — the content is no longer uniform.
    fn decay_canonical(&mut self, i: usize) {
        let fill = self.canonical_fill[i];
        if fill != NOT_CANONICAL {
            self.canonical.remove(&(fill as u8));
            self.canonical_fill[i] = NOT_CANONICAL;
        }
    }

    /// Allocate (or share) a uniformly-filled page. A thousand sections
    /// with the same `fill_byte` share *one* physical page.
    pub fn alloc_uniform(&mut self, fill_byte: u8) -> PageHandle {
        if let Some(&idx) = self.canonical.get(&fill_byte) {
            let i = idx as usize;
            self.refcounts[i] += 1;
            self.logical_sections += 1;
            return PageHandle {
                index: idx,
                generation: self.generations[i],
            };
        }
        let i = self.claim_page();
        self.slab[i].fill(fill_byte);
        self.canonical.insert(fill_byte, i as u32);
        self.canonical_fill[i] = fill_byte as u16;
        self.logical_sections += 1;
        PageHandle {
            index: i as u32,
            generation: self.generations[i],
        }
    }

    /// Allocate a private page seeded from an arbitrary 4 KiB payload.
    /// Never shared via the canonical map.
    pub fn alloc_copy(&mut self, payload: &[u8; PAGE_SIZE]) -> PageHandle {
        let i = self.claim_page();
        self.slab[i] = *payload;
        self.logical_sections += 1;
        PageHandle {
            index: i as u32,
            generation: self.generations[i],
        }
    }

    /// Borrow the page contents for a live handle; `None` if stale.
    pub fn read_slice(&self, handle: PageHandle) -> Option<&[u8; PAGE_SIZE]> {
        self.resolve(handle).map(|i| &self.slab[i])
    }

    /// `true` while a handle's page is shared by more than one section.
    pub fn is_shared(&self, handle: PageHandle) -> bool {
        self.resolve(handle)
            .map(|i| self.refcounts[i] > 1)
            .unwrap_or(false)
    }

    /// Write one byte, copy-on-write. If the page is private
    /// (`refcount == 1`) it is mutated in place — zero copies — and the
    /// handle is left pointing at the same page. If shared, a fresh page
    /// is claimed, the 4 KiB content copied, the write applied, and the
    /// handle updated to the new private page while every other sharer
    /// keeps the untouched original.
    ///
    /// On success `*handle` always names the written page, and the
    /// previous reference the handle held is consumed (its refcount is
    /// released on the CoW path).
    pub fn write_byte_cow(
        &mut self,
        handle: &mut PageHandle,
        index: usize,
        value: u8,
    ) -> Result<PageHandle, PagePoolError> {
        let i = self.resolve(*handle).ok_or(PagePoolError::InvalidHandle)?;
        if index >= PAGE_SIZE {
            return Err(PagePoolError::IndexOutOfBounds { index });
        }
        if self.refcounts[i] == 1 {
            // Sole owner: mutate in place and stop being the canonical
            // uniform page for our fill byte.
            self.slab[i][index] = value;
            self.decay_canonical(i);
            return Ok(*handle);
        }
        // Shared page: copy out, apply the write, swap the reference.
        let j = self.claim_page();
        self.slab[j] = self.slab[i];
        self.slab[j][index] = value;
        self.refcounts[i] -= 1;
        // `i` had refcount >= 2, so it stays live and — if it was the
        // canonical uniform page — remains correctly registered.
        let new_handle = PageHandle {
            index: j as u32,
            generation: self.generations[j],
        };
        *handle = new_handle;
        Ok(new_handle)
    }

    /// Release one section reference. When the last sharer lets go the
    /// page returns to the free list for O(1) reuse. The handle is
    /// poisoned on success so a double release fails closed (`false`)
    /// instead of decrementing a reference the caller no longer owns.
    pub fn release(&mut self, handle: &mut PageHandle) -> bool {
        let Some(i) = self.resolve(*handle) else {
            *handle = PageHandle {
                index: u32::MAX,
                generation: u32::MAX,
            };
            return false;
        };
        self.refcounts[i] -= 1;
        self.logical_sections -= 1;
        if self.refcounts[i] == 0 {
            self.decay_canonical(i);
            self.free_list.push(i as u32);
        }
        *handle = PageHandle {
            index: u32::MAX,
            generation: u32::MAX,
        };
        true
    }

    /// Snapshot of pool occupancy and achieved compression.
    pub fn stats(&self) -> PoolStats {
        let free = self.free_list.len();
        let physical = self.slab.len() - free;
        let shared = self.refcounts.iter().filter(|&&r| r > 1).count();
        let compression_ratio = if physical == 0 {
            0.0
        } else {
            self.logical_sections as f64 / physical as f64
        };
        PoolStats {
            logical_sections: self.logical_sections,
            physical_pages: physical,
            shared_pages: shared,
            free_pages: free,
            compression_ratio,
        }
    }
}

// ===========================================================================
// 3. Sparse-attention-inspired 64-bit dirty sub-tile mask
// ===========================================================================

/// A `u64` bitmap over the 64 `4×4×4` sub-tiles of a `16×16×16` section.
///
/// Bit index for block `(x, y, z)` with each coord in `0..16`:
/// `((y >> 2) << 4) | ((z >> 2) << 2) | (x >> 2)` — i.e. tile index
/// `ty * 16 + tz * 4 + tx` over `tx, ty, tz in 0..4`. Like a sparse
/// attention mask, only tiles whose bit is raised are ever visited.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SparseDirtyTileMask(u64);

impl SparseDirtyTileMask {
    /// A mask with nothing dirty.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Wrap a raw bit pattern.
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw 64-bit mask.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` while no tile is dirty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Tile bit index for sub-tile coords `(tx, ty, tz)` in `0..4`.
    const fn tile_bit(tx: u8, ty: u8, tz: u8) -> u32 {
        ((ty as u32) << 4) | ((tz as u32) << 2) | tx as u32
    }

    /// Mark the `4×4×4` sub-tile containing block `(x, y, z)`.
    /// Coords outside `0..16` are ignored fail-closed. Returns `true`
    /// when a tile was marked.
    pub fn mark_block(&mut self, x: u8, y: u8, z: u8) -> bool {
        if x >= 16 || y >= 16 || z >= 16 {
            return false;
        }
        self.0 |= 1u64 << Self::tile_bit(x >> 2, y >> 2, z >> 2);
        true
    }

    /// Mark sub-tile `(tx, ty, tz)` directly. Coords outside `0..4` are
    /// ignored fail-closed. Returns `true` when a tile was marked.
    pub fn mark_tile(&mut self, tx: u8, ty: u8, tz: u8) -> bool {
        if tx >= 4 || ty >= 4 || tz >= 4 {
            return false;
        }
        self.0 |= 1u64 << Self::tile_bit(tx, ty, tz);
        true
    }

    /// `true` when sub-tile `(tx, ty, tz)` is dirty; `false` for
    /// out-of-range coords.
    pub fn is_tile_dirty(&self, tx: u8, ty: u8, tz: u8) -> bool {
        if tx >= 4 || ty >= 4 || tz >= 4 {
            return false;
        }
        (self.0 >> Self::tile_bit(tx, ty, tz)) & 1 == 1
    }

    /// Number of dirty sub-tiles — the hardware popcount.
    pub fn dirty_count(&self) -> u32 {
        self.0.count_ones()
    }

    /// Clear every dirty bit.
    pub fn clear(&mut self) {
        self.0 = 0;
    }

    /// Visit every dirty tile exactly once in ascending bit order,
    /// clearing each as it is yielded — O(popcount) via `trailing_zeros`,
    /// never O(64). The mask is empty once the iterator is dropped after
    /// full consumption.
    pub fn drain_dirty_tiles(&mut self) -> impl Iterator<Item = (u8, u8, u8)> + '_ {
        let bits = &mut self.0;
        std::iter::from_fn(move || {
            if *bits == 0 {
                return None;
            }
            let idx = bits.trailing_zeros() as u8;
            *bits &= *bits - 1;
            Some((idx & 3, (idx >> 4) & 3, (idx >> 2) & 3))
        })
    }
}

// ===========================================================================
// 4. LLM-quantized 8-byte packed voxel vertex
// ===========================================================================

/// An 8-byte (`#[repr(C)]`, two `u32` words) lossless voxel mesh vertex —
/// ~75–80% smaller than the typical 32–40 byte unpacked form.
///
/// Bit layout (little-endian fields, word0 LSB→MSB):
/// `x[5] y[5] z[5] face[3] ao[2] sky_light[4] block_light[4] flags[4]`
/// — exactly 32 bits — then word1: `texture_id[16] u[8] v[8]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct QuantizedVoxelVertex {
    /// Positions, face, AO, light and flag fields.
    pub word0: u32,
    /// Texture and UV fields.
    pub word1: u32,
}

/// Fully-decoded form of a [`QuantizedVoxelVertex`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnpackedVoxelVertex {
    /// Local x coordinate `0..=16`.
    pub x: u8,
    /// Local y coordinate `0..=16`.
    pub y: u8,
    /// Local z coordinate `0..=16`.
    pub z: u8,
    /// Face index `0..6`.
    pub face: u8,
    /// Ambient-occlusion level `0..4`.
    pub ao: u8,
    /// Sky light `0..=15`.
    pub sky_light: u8,
    /// Block light `0..=15`.
    pub block_light: u8,
    /// Mesh flags `0..=15`.
    pub flags: u8,
    /// Full 16-bit texture index.
    pub texture_id: u16,
    /// U texture coordinate.
    pub u: u8,
    /// V texture coordinate.
    pub v: u8,
}

impl QuantizedVoxelVertex {
    /// Pack a vertex. Fails closed (`Err`) on any field outside its
    /// representable range — nothing is silently truncated.
    #[allow(clippy::too_many_arguments)]
    pub fn pack(
        x: u8,
        y: u8,
        z: u8,
        face: u8,
        ao: u8,
        sky_light: u8,
        block_light: u8,
        flags: u8,
        texture_id: u16,
        u: u8,
        v: u8,
    ) -> Result<Self, &'static str> {
        if x > 16 || y > 16 || z > 16 {
            return Err("vertex coordinate out of range: x, y, z must be <= 16");
        }
        if face >= 6 {
            return Err("face out of range: must be < 6");
        }
        if ao >= 4 {
            return Err("ao out of range: must be < 4");
        }
        if sky_light > 15 || block_light > 15 {
            return Err("light out of range: sky_light and block_light must be <= 15");
        }
        if flags > 15 {
            return Err("flags out of range: must be <= 15");
        }
        let word0 = (x as u32)
            | ((y as u32) << 5)
            | ((z as u32) << 10)
            | ((face as u32) << 15)
            | ((ao as u32) << 18)
            | ((sky_light as u32) << 20)
            | ((block_light as u32) << 24)
            | ((flags as u32) << 28);
        let word1 = (texture_id as u32) | ((u as u32) << 16) | ((v as u32) << 24);
        Ok(Self { word0, word1 })
    }

    /// Decode every field back out — the exact inverse of [`pack`].
    ///
    /// [`pack`]: Self::pack
    pub fn unpack(self) -> UnpackedVoxelVertex {
        UnpackedVoxelVertex {
            x: (self.word0 & 0x1F) as u8,
            y: ((self.word0 >> 5) & 0x1F) as u8,
            z: ((self.word0 >> 10) & 0x1F) as u8,
            face: ((self.word0 >> 15) & 0x7) as u8,
            ao: ((self.word0 >> 18) & 0x3) as u8,
            sky_light: ((self.word0 >> 20) & 0xF) as u8,
            block_light: ((self.word0 >> 24) & 0xF) as u8,
            flags: ((self.word0 >> 28) & 0xF) as u8,
            texture_id: (self.word1 & 0xFFFF) as u16,
            u: ((self.word1 >> 16) & 0xFF) as u8,
            v: ((self.word1 >> 24) & 0xFF) as u8,
        }
    }
}

// ===========================================================================
// 5. Speculative-decoding-inspired chunk prefetcher
// ===========================================================================

/// Chunk grid bound for speculation: a view distance past 32 rings is
/// outside any vanilla-or-modded shipping range.
const MAX_VIEW_DISTANCE: u32 = 32;

/// Speeds below this emit no speculation — there is no direction worth
/// betting on. Also the guard against normalizing a zero vector.
const MIN_SPEED: f64 = 1.0e-6;

/// Normalized-direction dot product below this marks a sharp reversal;
/// every queued entry from the previous heading is canceled in O(1).
const REVERSAL_DOT: f64 = -0.25;

/// Minimum cosine of the forward speculation cone (~75° half-angle).
/// Entries must be *strictly* ahead of the motion vector.
const FORWARD_CONE_MIN_COS: f64 = 0.25;

/// Lifecycle of one speculative chunk entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpeculativeState {
    /// Emitted under the current heading generation; eligible for work.
    Pending,
    /// Belongs to a superseded heading — canceled in O(1) by a reversal.
    CanceledStale,
}

/// One speculative chunk coordinate plus its lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeculativeEntry {
    /// Chunk X coordinate.
    pub cx: i32,
    /// Chunk Z coordinate.
    pub cz: i32,
    /// Priority rank within its emission batch (`0` = best).
    pub priority: u32,
    /// Whether the entry is still bet on.
    pub state: SpeculativeState,
}

/// Internal queued record; the state is derived from `generation` vs the
/// prefetcher's current generation, which is what makes reversal O(1).
#[derive(Clone, Copy, Debug)]
struct QueuedChunk {
    cx: i32,
    cz: i32,
    generation: u32,
    priority: u32,
}

/// Speculative-decoding-style chunk prefetch planner.
///
/// Each [`update`] emits a prioritized forward cone of chunk coordinates
/// ahead of the player's motion vector, skipping chunks the caller says
/// are already loaded. When the heading reverses sharply
/// (`dot < -0.25`), a single generation bump cancels every outstanding
/// speculative entry — idle cores never grind chunks behind the player.
///
/// [`update`]: Self::update
#[derive(Debug)]
pub struct SpeculativeChunkPrefetcher {
    /// Normalized heading from the last update that had real motion.
    prev_dir: Option<(f64, f64)>,
    /// Monotonic heading epoch; bumping it marks every older entry stale.
    generation: u32,
    queue: VecDeque<QueuedChunk>,
}

impl Default for SpeculativeChunkPrefetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeculativeChunkPrefetcher {
    /// An empty prefetcher at generation 0.
    pub fn new() -> Self {
        Self {
            prev_dir: None,
            generation: 0,
            queue: VecDeque::new(),
        }
    }

    /// Current heading generation.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Iterate the whole tracked queue, resolving each entry's state
    /// against the current generation.
    pub fn entries(&self) -> impl Iterator<Item = SpeculativeEntry> + '_ {
        let generation = self.generation;
        self.queue.iter().map(move |q| SpeculativeEntry {
            cx: q.cx,
            cz: q.cz,
            priority: q.priority,
            state: if q.generation == generation {
                SpeculativeState::Pending
            } else {
                SpeculativeState::CanceledStale
            },
        })
    }

    /// Count of entries still bet on under the current heading.
    pub fn pending_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|q| q.generation == self.generation)
            .count()
    }

    /// Emit a prioritized forward cone of chunk coords ahead of the
    /// motion vector. Returns the *newly emitted* entries (all
    /// `Pending`); the full lifecycle queue is observable via
    /// [`entries`].
    ///
    /// Safety: any non-finite input, or a speed below [`MIN_SPEED`],
    /// emits nothing and leaves the queue untouched. `view_distance` is
    /// clamped to [`MAX_VIEW_DISTANCE`] and `max_speculative` caps the
    /// emission; `is_loaded` filters chunks already resident.
    ///
    /// [`entries`]: Self::entries
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        pos_x: f64,
        pos_z: f64,
        vel_x: f64,
        vel_z: f64,
        view_distance: u32,
        max_speculative: usize,
        mut is_loaded: impl FnMut(i32, i32) -> bool,
    ) -> Vec<SpeculativeEntry> {
        if !pos_x.is_finite() || !pos_z.is_finite() || !vel_x.is_finite() || !vel_z.is_finite() {
            return Vec::new();
        }
        let speed = vel_x.hypot(vel_z);
        if speed < MIN_SPEED {
            return Vec::new();
        }
        let dir = (vel_x / speed, vel_z / speed);
        if let Some(prev) = self.prev_dir {
            let dot = prev.0 * dir.0 + prev.1 * dir.1;
            if dot < REVERSAL_DOT {
                // O(1) cancellation: one bump marks every prior entry
                // CanceledStale without touching the queue itself.
                self.generation = self.generation.wrapping_add(1);
            }
        }
        self.prev_dir = Some(dir);

        let base_cx = (pos_x / 16.0)
            .floor()
            .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        let base_cz = (pos_z / 16.0)
            .floor()
            .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        let vd = view_distance.min(MAX_VIEW_DISTANCE) as i32;

        // Rank the forward cone: heading alignment first, then distance.
        let mut candidates: Vec<(i32, i32, f64, f64)> = Vec::new();
        for dx in -vd..=vd {
            for dz in -vd..=vd {
                let dist = ((dx * dx + dz * dz) as f64).sqrt();
                if dist < 0.5 || dist > vd as f64 {
                    continue;
                }
                let cos = (dx as f64 * dir.0 + dz as f64 * dir.1) / dist;
                if cos < FORWARD_CONE_MIN_COS {
                    continue;
                }
                let cx = base_cx.saturating_add(dx);
                let cz = base_cz.saturating_add(dz);
                if is_loaded(cx, cz) {
                    continue;
                }
                if self
                    .queue
                    .iter()
                    .any(|q| q.generation == self.generation && q.cx == cx && q.cz == cz)
                {
                    continue; // Already an active bet under this heading.
                }
                candidates.push((cx, cz, cos, dist));
            }
        }
        candidates.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
        });
        candidates.truncate(max_speculative);

        let mut emitted = Vec::with_capacity(candidates.len());
        for (priority, (cx, cz, _, _)) in candidates.into_iter().enumerate() {
            self.queue.push_back(QueuedChunk {
                cx,
                cz,
                generation: self.generation,
                priority: priority as u32,
            });
            emitted.push(SpeculativeEntry {
                cx,
                cz,
                priority: priority as u32,
                state: SpeculativeState::Pending,
            });
        }

        // Bounded retention: never let stale entries outgrow four batches.
        let cap = max_speculative.max(1).saturating_mul(4);
        while self.queue.len() > cap {
            self.queue.pop_front();
        }
        emitted
    }
}
