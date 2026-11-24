// SPDX-License-Identifier: LGPL-3.0-or-later
//! Unit, parity and live-host tests for `megu_core::hw_adaptive_engine`.

use megu_core::hw_adaptive_engine::{
    build_unified_optimization_plan, classify_cpu_vendor, detect_host_hardware_caps,
    detect_universal_hardware_profile, max_nibble_arrays_adaptive, max_nibble_arrays_tiered,
    select_compute_strategy, CacheDomainTopology, CacheStrategy, ComputeKernelMode,
    ComputeStrategy, CoreAffinityHint, CoreHeterogeneity, CpuVendor, DomainKind, ExtendedSimdTier,
    GpuComputeParadigm, HostHardwareCaps, KernelAutotuner, MemoryFootprintClass, MemoryTopology,
    MemoryTransferStrategy, PagePoolError, PagedSectionPool, PowerThermalProfile,
    QuantizedVoxelVertex, SimdKernelTier, SparseDirtyTileMask, SpeculativeChunkPrefetcher,
    SpeculativeState, StorageIoClass, UniversalHardwareProfile, L3_LARGE_KIB, L3_MEDIUM_KIB,
    LIGHT_BUFFER_BYTES, PAGE_SIZE,
};

/// Deterministic per-nibble reference implementation every optimized path
/// must agree with bit-for-bit.
fn reference_nibble_max(
    a: &[u8; LIGHT_BUFFER_BYTES],
    b: &[u8; LIGHT_BUFFER_BYTES],
) -> [u8; LIGHT_BUFFER_BYTES] {
    let mut out = [0u8; LIGHT_BUFFER_BYTES];
    for i in 0..LIGHT_BUFFER_BYTES {
        let lo = (a[i] & 0x0F).max(b[i] & 0x0F);
        let hi = (a[i] >> 4).max(b[i] >> 4);
        out[i] = lo | (hi << 4);
    }
    out
}

fn caps(
    logical_cores: u32,
    l3_cache_kib: u32,
    hybrid_cores: bool,
    simd_tier: SimdKernelTier,
    memory_topology: MemoryTopology,
) -> HostHardwareCaps {
    HostHardwareCaps {
        logical_cores,
        l3_cache_kib,
        hybrid_cores,
        simd_tier,
        memory_topology,
    }
}

fn synthetic_desktop() -> UniversalHardwareProfile {
    UniversalHardwareProfile {
        base: caps(
            16,
            32 * 1024,
            false,
            SimdKernelTier::Avx2,
            MemoryTopology::DiscreteVram,
        ),
        l2_per_core_kib: 1024,
        cache_line_bytes: 64,
        cache_domains: CacheDomainTopology::SingleSharedLlc,
        core_layout: CoreHeterogeneity::Uniform { cores: 16 },
        total_ram_mib: 32 * 1024,
        memory_class: MemoryFootprintClass::Abundant,
        gpu_paradigm: GpuComputeParadigm::DiscretePcieHighBw,
        power_profile: PowerThermalProfile::AcDesktop,
        storage_io: StorageIoClass::NvmeFastRandom,
        extended_simd: ExtendedSimdTier::Avx2,
        cpu_vendor: CpuVendor::Amd,
    }
}

// ===========================================================================
// Host hardware probe
// ===========================================================================

#[test]
fn live_detect_reports_nonzero_cores_and_finite_strategy() {
    let caps = detect_host_hardware_caps();
    assert!(
        caps.logical_cores >= 1,
        "available_parallelism must be >= 1"
    );
    if cfg!(target_os = "linux") {
        // This host's sysfs exposes cache topology; the parse must be a
        // positive KiB value (9800X3D reports 98304 = 96 MiB V-Cache).
        assert!(
            caps.l3_cache_kib > 0,
            "linux sysfs must expose a last-level cache"
        );
    }
    let strategy = select_compute_strategy(&caps);
    assert!(strategy.workers.foreground_threads >= 1);
    assert!(strategy.workers.background_threads >= 1);
    assert!(strategy.workers.foreground_threads + strategy.workers.background_threads >= 1);
    eprintln!(
        "live caps: cores={} l3_kib={} hybrid={} simd={:?} topo={:?} -> strategy={:?}",
        caps.logical_cores,
        caps.l3_cache_kib,
        caps.hybrid_cores,
        caps.simd_tier,
        caps.memory_topology,
        strategy,
    );
}

// ===========================================================================
// select_compute_strategy across hardware profiles
// ===========================================================================

#[test]
fn strategy_large_vcache_dgpu_selects_lut_batch_and_staged_arena() {
    let s = select_compute_strategy(&caps(
        16,
        96 * 1024,
        false,
        SimdKernelTier::Avx2,
        MemoryTopology::DiscreteVram,
    ));
    assert_eq!(s.cache, CacheStrategy::LargeL3LutBatch);
    assert_eq!(s.chunk_batch_edge(), 4);
    assert!(s.prefer_lut_over_recompute());
    assert_eq!(s.memory, MemoryTransferStrategy::DiscreteStagedArena);
    assert_eq!(s.simd, SimdKernelTier::Avx2);
    // 16 non-hybrid cores: quarter to background, rest foreground.
    assert_eq!(s.workers.background_threads, 4);
    assert_eq!(s.workers.foreground_threads, 12);
}

#[test]
fn strategy_medium_l3_selects_balanced_tile() {
    let s = select_compute_strategy(&caps(
        8,
        32 * 1024,
        false,
        SimdKernelTier::Avx2,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(s.cache, CacheStrategy::BalancedTile);
    assert_eq!(s.chunk_batch_edge(), 2);
    assert!(s.prefer_lut_over_recompute());
    assert_eq!(s.memory, MemoryTransferStrategy::UnifiedZeroCopy);
    assert_eq!(s.workers.background_threads, 2);
    assert_eq!(s.workers.foreground_threads, 6);
}

#[test]
fn strategy_small_l3_laptop_uma_selects_recompute_stream() {
    let s = select_compute_strategy(&caps(
        8,
        8 * 1024,
        false,
        SimdKernelTier::Avx2,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(s.cache, CacheStrategy::FlashRecomputeStream);
    assert_eq!(s.chunk_batch_edge(), 1);
    assert!(!s.prefer_lut_over_recompute());
    assert_eq!(s.memory, MemoryTransferStrategy::UnifiedZeroCopy);
}

#[test]
fn strategy_apple_silicon_neon_uma() {
    let s = select_compute_strategy(&caps(
        10,
        0,
        true,
        SimdKernelTier::Neon,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(s.simd, SimdKernelTier::Neon);
    assert_eq!(s.memory, MemoryTransferStrategy::UnifiedZeroCopy);
    // Hybrid 10-core part: half to background (efficiency cores).
    assert_eq!(s.workers.background_threads, 5);
    assert_eq!(s.workers.foreground_threads, 5);
}

#[test]
fn strategy_single_core_minimal_fallback() {
    let s = select_compute_strategy(&caps(
        1,
        0,
        false,
        SimdKernelTier::Scalar,
        MemoryTopology::DiscreteVram,
    ));
    assert_eq!(s.cache, CacheStrategy::FlashRecomputeStream);
    assert_eq!(s.workers.foreground_threads, 1);
    assert!(s.workers.background_threads >= 1);
    assert_eq!(s.memory, MemoryTransferStrategy::DiscreteStagedArena);
}

#[test]
fn strategy_l3_thresholds_are_exact() {
    let at_large = select_compute_strategy(&caps(
        4,
        L3_LARGE_KIB,
        false,
        SimdKernelTier::Scalar,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(at_large.cache, CacheStrategy::LargeL3LutBatch);
    let just_below_large = select_compute_strategy(&caps(
        4,
        L3_LARGE_KIB - 1,
        false,
        SimdKernelTier::Scalar,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(just_below_large.cache, CacheStrategy::BalancedTile);
    let at_medium = select_compute_strategy(&caps(
        4,
        L3_MEDIUM_KIB,
        false,
        SimdKernelTier::Scalar,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(at_medium.cache, CacheStrategy::BalancedTile);
    let just_below_medium = select_compute_strategy(&caps(
        4,
        L3_MEDIUM_KIB - 1,
        false,
        SimdKernelTier::Scalar,
        MemoryTopology::UnifiedUma,
    ));
    assert_eq!(just_below_medium.cache, CacheStrategy::FlashRecomputeStream);
}

// ===========================================================================
// Universal profile, domain plans and deterministic feedback
// ===========================================================================

#[test]
fn universal_live_probe_has_sane_cores_ram_and_cache_line() {
    let profile = detect_universal_hardware_profile();
    assert!(profile.base.logical_cores > 0);
    assert!(matches!(profile.cache_line_bytes, 64 | 128));
    if cfg!(target_os = "linux") {
        assert!(profile.total_ram_mib > 0);
        assert!(profile.l2_per_core_kib > 0);
    }
    let plan = build_unified_optimization_plan(&profile, None);
    for domain in [
        DomainKind::Worldgen,
        DomainKind::Lighting,
        DomainKind::Meshing,
        DomainKind::PhysicsAndContraptions,
        DomainKind::NetworkBroadcast,
        DomainKind::SpatialAudio,
    ] {
        let specialized = plan.for_domain(domain);
        assert!(specialized.worker_threads > 0);
        assert!(specialized.pool_budget_pages > 0);
        assert_eq!(
            specialized.false_sharing_pad_bytes,
            profile.cache_line_bytes
        );
    }
}

#[test]
fn apple_silicon_tbdr_uses_128_byte_padding_uma_and_efficiency_cores() {
    let mut apple = synthetic_desktop();
    apple.base = caps(
        10,
        0,
        true,
        SimdKernelTier::Neon,
        MemoryTopology::UnifiedUma,
    );
    apple.l2_per_core_kib = 12 * 1024;
    apple.cache_line_bytes = 128;
    apple.core_layout = CoreHeterogeneity::Hybrid {
        p_cores: 6,
        e_cores: 4,
    };
    apple.gpu_paradigm = GpuComputeParadigm::TileBasedTbdr;
    apple.extended_simd = ExtendedSimdTier::Neon;
    let plan = build_unified_optimization_plan(&apple, None);
    assert_eq!(
        plan.memory_transfer,
        MemoryTransferStrategy::UnifiedZeroCopy
    );

    let lighting = plan.for_domain(DomainKind::Lighting);
    let physics = plan.for_domain(DomainKind::PhysicsAndContraptions);
    let worldgen = plan.for_domain(DomainKind::Worldgen);
    assert_eq!(lighting.false_sharing_pad_bytes, 128);
    assert!(lighting.prefer_single_pass_gpu);
    assert_eq!(lighting.core_affinity, CoreAffinityHint::Performance);
    assert_eq!(physics.core_affinity, CoreAffinityHint::Performance);
    assert_eq!(lighting.worker_threads, 6);
    assert_eq!(worldgen.core_affinity, CoreAffinityHint::Efficiency);
    assert_eq!(worldgen.worker_threads, 4);
}

#[test]
fn asymmetric_dual_ccd_schedules_latency_sensitive_domains_near_vcache() {
    let mut x3d = synthetic_desktop();
    // cpu0 may sit on the 32 MiB CCD; the asymmetric cache domain still
    // enables the 96 MiB tier for critical processing.
    x3d.cache_domains = CacheDomainTopology::AsymmetricDualCcd {
        big_llc_cores: 8,
        fast_cores: 8,
    };
    let plan = build_unified_optimization_plan(&x3d, None);
    assert_eq!(plan.compute.cache, CacheStrategy::LargeL3LutBatch);
    for domain in [DomainKind::Lighting, DomainKind::PhysicsAndContraptions] {
        let specialized = plan.for_domain(domain);
        assert!(specialized.pin_to_primary_llc_domain);
        assert!(specialized.use_lut_over_recompute);
        assert_eq!(specialized.tile_edge_chunks, 4);
    }
    assert!(
        !plan
            .for_domain(DomainKind::Worldgen)
            .pin_to_primary_llc_domain
    );
}

#[test]
fn two_gib_pi_slow_sd_budgets_less_than_desktop_and_coalesces_io() {
    let desktop = synthetic_desktop();
    let desktop_budget = build_unified_optimization_plan(&desktop, None)
        .for_domain(DomainKind::Worldgen)
        .pool_budget_pages;
    let mut pi = desktop;
    pi.base = caps(
        4,
        1024,
        false,
        SimdKernelTier::Neon,
        MemoryTopology::UnifiedUma,
    );
    pi.total_ram_mib = 2048;
    pi.memory_class = MemoryFootprintClass::from_ram_mib(pi.total_ram_mib);
    pi.storage_io = StorageIoClass::HddOrSlowSd;
    pi.gpu_paradigm = GpuComputeParadigm::LowPowerIntegratedWgpu;
    let plan = build_unified_optimization_plan(&pi, None);
    let worldgen = plan.for_domain(DomainKind::Worldgen);
    assert!(worldgen.pool_budget_pages < desktop_budget);
    assert!(worldgen.pool_budget_pages * PAGE_SIZE <= 1024 * 1024);
    assert!(worldgen.coalesce_background_bursts);
    assert!(worldgen.prefer_single_pass_gpu);
    assert!(!worldgen.use_lut_over_recompute);
    assert_eq!(plan.compute.cache, CacheStrategy::FlashRecomputeStream);
}

#[test]
fn unknown_storage_does_not_assume_slow_disk_policy() {
    let mut profile = synthetic_desktop();
    profile.storage_io = StorageIoClass::Unknown;
    let plan = build_unified_optimization_plan(&profile, None);
    assert!(
        !plan
            .for_domain(DomainKind::Worldgen)
            .coalesce_background_bursts
    );
}

#[test]
fn handheld_apu_uses_uma_bounded_workers_and_burst_coalescing() {
    let mut deck = synthetic_desktop();
    deck.base.memory_topology = MemoryTopology::UnifiedUma;
    deck.gpu_paradigm = GpuComputeParadigm::UnifiedApuSharedRam;
    deck.power_profile = PowerThermalProfile::HandheldTdp;
    let plan = build_unified_optimization_plan(&deck, None);
    assert_eq!(
        plan.memory_transfer,
        MemoryTransferStrategy::UnifiedZeroCopy
    );
    for domain in [
        DomainKind::Worldgen,
        DomainKind::Lighting,
        DomainKind::Meshing,
    ] {
        let specialized = plan.for_domain(domain);
        assert!(specialized.coalesce_background_bursts);
        assert!((1..=2).contains(&specialized.worker_threads));
    }
}

#[test]
fn discrete_gpu_uses_staged_transfers_even_if_legacy_memory_hint_is_uma() {
    let mut discrete = synthetic_desktop();
    discrete.base.memory_topology = MemoryTopology::UnifiedUma;
    let plan = build_unified_optimization_plan(&discrete, None);
    assert_eq!(
        plan.memory_transfer,
        MemoryTransferStrategy::DiscreteStagedArena
    );
}

#[test]
fn ram_band_edges_and_battery_policy_are_stable() {
    assert_eq!(
        MemoryFootprintClass::from_ram_mib(0),
        MemoryFootprintClass::Constrained
    );
    assert_eq!(
        MemoryFootprintClass::from_ram_mib(4096),
        MemoryFootprintClass::Constrained
    );
    assert_eq!(
        MemoryFootprintClass::from_ram_mib(4097),
        MemoryFootprintClass::Standard
    );
    assert_eq!(
        MemoryFootprintClass::from_ram_mib(16383),
        MemoryFootprintClass::Standard
    );
    assert_eq!(
        MemoryFootprintClass::from_ram_mib(16384),
        MemoryFootprintClass::Abundant
    );
    let mut laptop = synthetic_desktop();
    laptop.power_profile = PowerThermalProfile::BatteryLaptop;
    assert!(
        build_unified_optimization_plan(&laptop, None)
            .for_domain(DomainKind::NetworkBroadcast)
            .coalesce_background_bursts
    );
}

#[test]
fn autotuner_converges_after_sustained_win_and_overrides_static_cache_hint() {
    let mut tuner = KernelAutotuner::new(ComputeKernelMode::LutBatch);
    for _ in 0..4 {
        assert_eq!(
            tuner.observe_sample(ComputeKernelMode::LutBatch, 1000),
            ComputeKernelMode::LutBatch
        );
    }
    // A 20% faster challenger needs four qualifying updates after warmup.
    for _ in 0..6 {
        assert_eq!(
            tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 800),
            ComputeKernelMode::LutBatch
        );
    }
    assert_eq!(
        tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 800),
        ComputeKernelMode::RecomputeSwar
    );
    let plan = build_unified_optimization_plan(&synthetic_desktop(), Some(tuner.selected_mode()));
    assert!(!plan.for_domain(DomainKind::Lighting).use_lut_over_recompute);
}

#[test]
fn autotuner_hysteresis_ignores_noise_and_subthreshold_advantage() {
    let mut tuner = KernelAutotuner::default();
    for _ in 0..8 {
        tuner.observe_sample(ComputeKernelMode::LutBatch, 1000);
        tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 900);
    }
    assert_eq!(tuner.selected_mode(), ComputeKernelMode::LutBatch);
    // A single unusually fast observation may improve an EMA briefly,
    // but cannot switch without sustained independent measurements.
    tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 1);
    assert_eq!(tuner.selected_mode(), ComputeKernelMode::LutBatch);
    for _ in 0..4 {
        tuner.observe_sample(ComputeKernelMode::LutBatch, 1000);
        tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 1000);
    }
    assert_eq!(tuner.selected_mode(), ComputeKernelMode::LutBatch);
}

#[test]
fn autotuner_rejects_invalid_samples_without_overflow_or_mode_changes() {
    let mut tuner = KernelAutotuner::new(ComputeKernelMode::RecomputeSwar);
    assert_eq!(tuner.estimated_ns(ComputeKernelMode::LutBatch), None);
    for invalid in [0, 60_000_000_001, u64::MAX] {
        assert_eq!(
            tuner.observe_sample(ComputeKernelMode::LutBatch, invalid),
            ComputeKernelMode::RecomputeSwar
        );
    }
    assert_eq!(tuner.estimated_ns(ComputeKernelMode::LutBatch), None);
    tuner.observe_sample(ComputeKernelMode::LutBatch, 1);
    assert_eq!(tuner.estimated_ns(ComputeKernelMode::LutBatch), Some(1));
    tuner.observe_sample(ComputeKernelMode::LutBatch, 60_000_000_000);
    assert!(tuner.estimated_ns(ComputeKernelMode::LutBatch).unwrap() > 1);
    assert_eq!(tuner.selected_mode(), ComputeKernelMode::RecomputeSwar);
}

#[test]
fn autotuner_accepts_exactly_twelve_percent_sustained_advantage() {
    let mut tuner = KernelAutotuner::new(ComputeKernelMode::RecomputeSwar);
    for _ in 0..4 {
        tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 1000);
    }
    for _ in 0..6 {
        assert_eq!(
            tuner.observe_sample(ComputeKernelMode::LutBatch, 880),
            ComputeKernelMode::RecomputeSwar
        );
    }
    assert_eq!(
        tuner.observe_sample(ComputeKernelMode::LutBatch, 880),
        ComputeKernelMode::LutBatch
    );
    tuner.observe_sample(ComputeKernelMode::RecomputeSwar, 1);
    assert_eq!(tuner.selected_mode(), ComputeKernelMode::LutBatch);
}

#[test]
fn invalid_synthetic_cache_line_defaults_to_safe_64_byte_padding() {
    let mut profile = synthetic_desktop();
    profile.cache_line_bytes = 0;
    assert_eq!(
        build_unified_optimization_plan(&profile, None)
            .for_domain(DomainKind::Meshing)
            .false_sharing_pad_bytes,
        64
    );
}

// ===========================================================================
// PagedSectionPool
// ===========================================================================

#[test]
fn pool_thousand_uniform_sections_share_two_physical_pages() {
    let mut pool = PagedSectionPool::new();
    let mut handles = Vec::new();
    for _ in 0..1_000 {
        handles.push(pool.alloc_uniform(0x00)); // air
    }
    for _ in 0..500 {
        handles.push(pool.alloc_uniform(0xFF)); // stone
    }
    let stats = pool.stats();
    assert_eq!(stats.logical_sections, 1_500);
    assert_eq!(stats.physical_pages, 2);
    assert_eq!(stats.shared_pages, 2);
    assert!(
        stats.compression_ratio >= 500.0,
        "ratio={}",
        stats.compression_ratio
    );
    for h in &mut handles {
        assert!(pool.release(h));
    }
    let stats = pool.stats();
    assert_eq!(stats.logical_sections, 0);
    assert_eq!(stats.physical_pages, 0);
    assert_eq!(stats.free_pages, 2);
}

#[test]
fn pool_cow_write_isolates_other_sharers() {
    let mut pool = PagedSectionPool::new();
    let mut a = pool.alloc_uniform(0x00);
    let b = pool.alloc_uniform(0x00);
    assert!(pool.is_shared(a));
    assert!(pool.is_shared(b));

    let new_h = pool.write_byte_cow(&mut a, 7, 0xAB).expect("cow write");
    assert_eq!(new_h, a);
    // a moved to a private page; b's page is untouched.
    assert_eq!(pool.read_slice(a).map(|p| p[7]), Some(0xAB));
    assert_eq!(pool.read_slice(b).map(|p| p[7]), Some(0x00));
    assert_eq!(
        pool.read_slice(b).map(|p| p.iter().all(|&v| v == 0)),
        Some(true)
    );
    let stats = pool.stats();
    assert_eq!(stats.physical_pages, 2);
    assert_eq!(stats.shared_pages, 0); // b's page dropped to refcount 1
    assert_eq!(stats.logical_sections, 2);
    // `a`'s copy is fully private and otherwise identical.
    let page = pool.read_slice(a).expect("live");
    assert!(page
        .iter()
        .enumerate()
        .all(|(i, &v)| v == if i == 7 { 0xAB } else { 0 }));
}

#[test]
fn pool_inplace_write_when_sole_owner_keeps_handle() {
    let mut pool = PagedSectionPool::new();
    let mut a = pool.alloc_uniform(0x00);
    let mut b = pool.alloc_uniform(0x00);
    assert!(pool.release(&mut b)); // refcount back to 1
    let same = pool
        .write_byte_cow(&mut a, 3, 0x5A)
        .expect("in-place write");
    assert_eq!(same, a, "sole-owner write must not allocate a new page");
    assert_eq!(pool.read_slice(a).map(|p| p[3]), Some(0x5A));
    // De-canonicalized: a new uniform 0x00 alloc gets a *fresh* zero page.
    let c = pool.alloc_uniform(0x00);
    assert_eq!(pool.read_slice(c).map(|p| p[3]), Some(0x00));
}

#[test]
fn pool_release_reclaims_pages_to_free_list() {
    let mut pool = PagedSectionPool::new();
    let mut h1 = pool.alloc_uniform(0x10);
    let mut h2 = pool.alloc_uniform(0x20);
    assert!(pool.release(&mut h1));
    assert!(pool.release(&mut h2));
    let stats = pool.stats();
    assert_eq!(stats.physical_pages, 0);
    assert_eq!(stats.free_pages, 2);
    // Reuse: two new allocations must not grow the slab past 2 pages.
    let mut h3 = pool.alloc_copy(&[0x77; PAGE_SIZE]);
    let _h4 = pool.alloc_copy(&[0x78; PAGE_SIZE]);
    assert_eq!(pool.read_slice(h3).map(|p| p[0]), Some(0x77));
    let stats = pool.stats();
    assert_eq!(stats.physical_pages, 2);
    assert_eq!(stats.free_pages, 0);
    assert!(pool.release(&mut h3));
}

#[test]
fn pool_out_of_bounds_and_stale_handles_fail_closed() {
    let mut pool = PagedSectionPool::new();
    let mut h = pool.alloc_uniform(0x00);
    assert_eq!(
        pool.write_byte_cow(&mut h, PAGE_SIZE, 1),
        Err(PagePoolError::IndexOutOfBounds { index: PAGE_SIZE })
    );
    assert_eq!(
        pool.write_byte_cow(&mut h, usize::MAX, 1),
        Err(PagePoolError::IndexOutOfBounds { index: usize::MAX })
    );
    assert!(pool.release(&mut h));
    // Stale handle: read -> None, write -> InvalidHandle, release -> false.
    assert!(pool.read_slice(h).is_none());
    assert_eq!(
        pool.write_byte_cow(&mut h, 0, 1),
        Err(PagePoolError::InvalidHandle)
    );
    assert!(!pool.release(&mut h));
}

// ===========================================================================
// SparseDirtyTileMask
// ===========================================================================

#[test]
fn mask_maps_every_block_to_exactly_its_4x4x4_tile() {
    let mut mask = SparseDirtyTileMask::empty();
    for y in 0u8..16 {
        for z in 0u8..16 {
            for x in 0u8..16 {
                let mut single = SparseDirtyTileMask::empty();
                assert!(single.mark_block(x, y, z));
                let bit = ((y >> 2) << 4) | ((z >> 2) << 2) | (x >> 2);
                assert_eq!(single.bits(), 1u64 << bit, "block ({x},{y},{z})");
                mask.mark_block(x, y, z);
            }
        }
    }
    assert_eq!(mask.bits(), u64::MAX);
    assert_eq!(mask.dirty_count(), 64);
    for ty in 0u8..4 {
        for tz in 0u8..4 {
            for tx in 0u8..4 {
                assert!(mask.is_tile_dirty(tx, ty, tz));
            }
        }
    }
}

#[test]
fn mask_out_of_range_coords_fail_closed() {
    let mut mask = SparseDirtyTileMask::empty();
    assert!(!mask.mark_block(16, 0, 0));
    assert!(!mask.mark_block(0, 255, 0));
    assert!(!mask.mark_tile(4, 0, 0));
    assert!(mask.is_empty());
    assert!(!mask.is_tile_dirty(4, 0, 0));
}

#[test]
fn drain_dirty_tiles_visits_only_dirty_in_order_and_clears() {
    let mut mask = SparseDirtyTileMask::empty();
    // Mark a scattered set: corners + an interior tile.
    mask.mark_tile(3, 3, 3); // bit 63
    mask.mark_tile(0, 0, 0); // bit 0
    mask.mark_tile(1, 0, 2); // bit 9
    mask.mark_block(15, 0, 0); // block (15,0,0) -> tile (3,0,0) bit 3
    let drained: Vec<(u8, u8, u8)> = mask.drain_dirty_tiles().collect();
    assert_eq!(drained, vec![(0, 0, 0), (3, 0, 0), (1, 0, 2), (3, 3, 3)]);
    assert!(mask.is_empty());
    assert_eq!(mask.dirty_count(), 0);
}

// ===========================================================================
// QuantizedVoxelVertex
// ===========================================================================

#[test]
fn vertex_is_exactly_eight_bytes() {
    assert_eq!(std::mem::size_of::<QuantizedVoxelVertex>(), 8);
}

#[test]
fn vertex_roundtrips_boundary_cross_product() {
    for &x in &[0u8, 1, 15, 16] {
        for &face in &[0u8, 5] {
            for &sky in &[0u8, 15] {
                for &tex in &[0u16, 1, 255, 4096, 65535] {
                    for &u in &[0u8, 128, 255] {
                        let v =
                            QuantizedVoxelVertex::pack(x, 16, 0, face, 3, sky, 15, 15, tex, u, 255)
                                .expect("pack");
                        let d = v.unpack();
                        assert_eq!(d.x, x);
                        assert_eq!(d.y, 16);
                        assert_eq!(d.z, 0);
                        assert_eq!(d.face, face);
                        assert_eq!(d.ao, 3);
                        assert_eq!(d.sky_light, sky);
                        assert_eq!(d.block_light, 15);
                        assert_eq!(d.flags, 15);
                        assert_eq!(d.texture_id, tex);
                        assert_eq!(d.u, u);
                        assert_eq!(d.v, 255);
                    }
                }
            }
        }
    }
}

#[test]
fn vertex_each_field_sweeps_full_range() {
    for x in 0..=16u8 {
        let d = QuantizedVoxelVertex::pack(x, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)
            .unwrap()
            .unpack();
        assert_eq!(d.x, x);
    }
    for face in 0..6u8 {
        let d = QuantizedVoxelVertex::pack(0, 0, 0, face, 0, 0, 0, 0, 0, 0, 0)
            .unwrap()
            .unpack();
        assert_eq!(d.face, face);
    }
    for ao in 0..4u8 {
        let d = QuantizedVoxelVertex::pack(0, 0, 0, 0, ao, 0, 0, 0, 0, 0, 0)
            .unwrap()
            .unpack();
        assert_eq!(d.ao, ao);
    }
    for light in 0..16u8 {
        let d = QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, light, light, light, 0, 0, 0)
            .unwrap()
            .unpack();
        assert_eq!(d.sky_light, light);
        assert_eq!(d.block_light, light);
        assert_eq!(d.flags, light);
    }
    for tex in 0..=u16::MAX {
        let d = QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, 0, 0, 0, tex, 0, 0)
            .unwrap()
            .unpack();
        assert_eq!(d.texture_id, tex);
    }
    for uv in 0..=u8::MAX {
        let d = QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, 0, 0, 0, 0, uv, uv)
            .unwrap()
            .unpack();
        assert_eq!(d.u, uv);
        assert_eq!(d.v, uv);
    }
}

#[test]
fn vertex_deterministic_cross_field_sample_roundtrips() {
    // Deterministic LCG sampling of the full 11-field space: proves no
    // field bleeds into a neighbor across arbitrary combinations.
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 33
    };
    for _ in 0..100_000 {
        let x = (next() % 17) as u8;
        let y = (next() % 17) as u8;
        let z = (next() % 17) as u8;
        let face = (next() % 6) as u8;
        let ao = (next() % 4) as u8;
        let sky = (next() % 16) as u8;
        let block = (next() % 16) as u8;
        let flags = (next() % 16) as u8;
        let tex = next() as u16;
        let u = next() as u8;
        let v = next() as u8;
        let d = QuantizedVoxelVertex::pack(x, y, z, face, ao, sky, block, flags, tex, u, v)
            .expect("pack")
            .unpack();
        assert_eq!(
            (
                d.x,
                d.y,
                d.z,
                d.face,
                d.ao,
                d.sky_light,
                d.block_light,
                d.flags,
                d.texture_id,
                d.u,
                d.v
            ),
            (x, y, z, face, ao, sky, block, flags, tex, u, v)
        );
    }
}

#[test]
fn vertex_rejects_out_of_range_inputs() {
    let ok = QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    assert!(ok.is_ok());
    assert!(QuantizedVoxelVertex::pack(17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0).is_err());
    assert!(QuantizedVoxelVertex::pack(0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0).is_err());
    // Boundary-legal values still pass.
    assert!(QuantizedVoxelVertex::pack(16, 16, 16, 5, 3, 15, 15, 15, 65535, 255, 255).is_ok());
}

// ===========================================================================
// SpeculativeChunkPrefetcher
// ===========================================================================

#[test]
fn prefetch_emits_chunks_strictly_ahead_of_motion() {
    let mut p = SpeculativeChunkPrefetcher::new();
    let emitted = p.update(8.0, 8.0, 1.0, 0.0, 4, 64, |_cx, _cz| false);
    assert!(!emitted.is_empty());
    assert!(emitted.len() <= 64);
    for e in &emitted {
        assert_eq!(e.state, SpeculativeState::Pending);
        // Strictly ahead of eastward motion: every emitted chunk has cx > base.
        assert!(e.cx > 0, "chunk {:?} is not ahead of +x motion", e);
    }
    // Priorities are a strict ranking starting at 0.
    let mut pr: Vec<u32> = emitted.iter().map(|e| e.priority).collect();
    pr.sort_unstable();
    assert_eq!(pr, (0..emitted.len() as u32).collect::<Vec<_>>());
}

#[test]
fn prefetch_reversal_cancels_stale_entries_in_o1() {
    let mut p = SpeculativeChunkPrefetcher::new();
    let east = p.update(0.0, 0.0, 1.0, 0.0, 4, 32, |_, _| false);
    assert!(!east.is_empty());
    assert!(p.entries().all(|e| e.state == SpeculativeState::Pending));

    // Sharp reversal: heading flips 180 degrees, dot = -1 < -0.25.
    let west = p.update(0.0, 0.0, -1.0, 0.0, 4, 32, |_, _| false);
    assert!(!west.is_empty());
    for e in &west {
        assert!(e.cx < 0, "reversed emission must lead the new heading");
    }
    // Every eastward entry is now CanceledStale; westward stay Pending.
    let stale: Vec<_> = p
        .entries()
        .filter(|e| e.state == SpeculativeState::CanceledStale)
        .collect();
    assert_eq!(stale.len(), east.len(), "all prior entries canceled");
    assert!(stale.iter().all(|e| e.cx > 0));
    assert_eq!(p.pending_count(), west.len());
}

#[test]
fn prefetch_skips_loaded_chunks() {
    let mut p = SpeculativeChunkPrefetcher::new();
    let emitted = p.update(0.0, 0.0, 1.0, 0.0, 4, 64, |cx, cz| cx == 1 && cz == 0);
    assert!(emitted.iter().all(|e| !(e.cx == 1 && e.cz == 0)));
}

#[test]
fn prefetch_respects_max_speculative_cap() {
    let mut p = SpeculativeChunkPrefetcher::new();
    let emitted = p.update(0.0, 0.0, 0.0, 1.0, 8, 5, |_, _| false);
    assert_eq!(emitted.len(), 5);
}

#[test]
fn prefetch_nan_inf_and_zero_velocity_are_safe() {
    let mut p = SpeculativeChunkPrefetcher::new();
    assert!(p.update(0.0, 0.0, 0.0, 0.0, 4, 8, |_, _| false).is_empty());
    assert!(p
        .update(0.0, 0.0, f64::NAN, 0.0, 4, 8, |_, _| false)
        .is_empty());
    assert!(p
        .update(0.0, 0.0, f64::INFINITY, 0.0, 4, 8, |_, _| false)
        .is_empty());
    assert!(p
        .update(f64::NAN, 0.0, 1.0, 0.0, 4, 8, |_, _| false)
        .is_empty());
    assert!(p.entries().next().is_none());
}

// ===========================================================================
// max_nibble_arrays_* bit-exactness
// ===========================================================================

fn deterministic_buffers() -> ([u8; LIGHT_BUFFER_BYTES], [u8; LIGHT_BUFFER_BYTES]) {
    // Splitmix64-filled buffers: fixed seed, full nibble coverage.
    let mut a = [0u8; LIGHT_BUFFER_BYTES];
    let mut b = [0u8; LIGHT_BUFFER_BYTES];
    let mut s = 0x0123456789ABCDEFu64;
    for i in 0..LIGHT_BUFFER_BYTES {
        s = s.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        a[i] = (z & 0xFF) as u8;
        b[i] = ((z >> 32) & 0xFF) as u8;
    }
    (a, b)
}

#[test]
fn nibble_max_is_bit_exact_across_every_tier_and_lut_choice() {
    let (a, b) = deterministic_buffers();
    let reference = reference_nibble_max(&a, &b);
    for tier in [
        SimdKernelTier::Avx512,
        SimdKernelTier::Avx2,
        SimdKernelTier::Neon,
        SimdKernelTier::Scalar,
    ] {
        for prefer_lut in [false, true] {
            let out = max_nibble_arrays_tiered(&a, &b, tier, prefer_lut);
            assert_eq!(out, reference, "tier={tier:?} lut={prefer_lut}");
        }
    }
    // And through the strategy-adaptive entry point on every profile.
    for l3 in [0, L3_MEDIUM_KIB, L3_LARGE_KIB] {
        for tier in [
            SimdKernelTier::Avx512,
            SimdKernelTier::Avx2,
            SimdKernelTier::Neon,
            SimdKernelTier::Scalar,
        ] {
            let strategy = ComputeStrategy {
                cache: if l3 >= L3_LARGE_KIB {
                    CacheStrategy::LargeL3LutBatch
                } else if l3 >= L3_MEDIUM_KIB {
                    CacheStrategy::BalancedTile
                } else {
                    CacheStrategy::FlashRecomputeStream
                },
                memory: MemoryTransferStrategy::UnifiedZeroCopy,
                simd: tier,
                workers: megu_core::hw_adaptive_engine::WorkerSplitPlan {
                    foreground_threads: 1,
                    background_threads: 1,
                },
            };
            assert_eq!(max_nibble_arrays_adaptive(&a, &b, &strategy), reference);
        }
    }
}

#[test]
fn nibble_max_handles_edge_patterns() {
    for (fill_a, fill_b) in [
        (0x00u8, 0x00u8),
        (0xFF, 0x00),
        (0x00, 0xFF),
        (0xFF, 0xFF),
        (0x0F, 0xF0),
        (0x55, 0xAA),
    ] {
        let a = [fill_a; LIGHT_BUFFER_BYTES];
        let b = [fill_b; LIGHT_BUFFER_BYTES];
        let reference = reference_nibble_max(&a, &b);
        for tier in [
            SimdKernelTier::Avx512,
            SimdKernelTier::Avx2,
            SimdKernelTier::Neon,
            SimdKernelTier::Scalar,
        ] {
            for prefer_lut in [false, true] {
                assert_eq!(
                    max_nibble_arrays_tiered(&a, &b, tier, prefer_lut),
                    reference,
                    "fill_a={fill_a:#x} fill_b={fill_b:#x} tier={tier:?} lut={prefer_lut}"
                );
            }
        }
    }
    // Single nibble dominance: per-byte max must pick each nibble lane.
    let mut a = [0u8; LIGHT_BUFFER_BYTES];
    let mut b = [0u8; LIGHT_BUFFER_BYTES];
    a[0] = 0x0F; // lo=15, hi=0
    b[0] = 0xF0; // lo=0, hi=15
    b[1] = 0x00;
    a[1] = 0x21; // lo=1, hi=2 vs b lo=0, hi=0
    let reference = reference_nibble_max(&a, &b);
    assert_eq!(reference[0], 0xFF);
    assert_eq!(reference[1], 0x21);
    for tier in [
        SimdKernelTier::Avx512,
        SimdKernelTier::Avx2,
        SimdKernelTier::Neon,
        SimdKernelTier::Scalar,
    ] {
        for prefer_lut in [false, true] {
            assert_eq!(
                max_nibble_arrays_tiered(&a, &b, tier, prefer_lut),
                reference
            );
        }
    }
}

#[test]
fn fx6_shadow_procedural3d_and_vfx_domains_route_through_unified_plan() {
    let mut hybrid = synthetic_desktop();
    hybrid.core_layout = CoreHeterogeneity::Hybrid {
        p_cores: 8,
        e_cores: 8,
    };
    let plan = build_unified_optimization_plan(&hybrid, None);
    let shadow = plan.for_domain(DomainKind::ShadowRendering);
    let p3d = plan.for_domain(DomainKind::Procedural3dAndBer);
    let vfx = plan.for_domain(DomainKind::VfxAndAtmosphere);
    let audio = plan.for_domain(DomainKind::SpatialAudio);

    assert_eq!(shadow.core_affinity, CoreAffinityHint::Performance);
    assert_eq!(p3d.core_affinity, CoreAffinityHint::Efficiency);
    assert_eq!(vfx.core_affinity, CoreAffinityHint::Efficiency);
    assert_eq!(audio.core_affinity, CoreAffinityHint::Efficiency);
    assert!(shadow.worker_threads >= 1);
    assert!(p3d.worker_threads >= 1);
    assert!(vfx.worker_threads >= 1);
}

// ===========================================================================
// Legacy low-spec & mobile-SoC profiles: Core 2 Duo, single core, Snapdragon
// ===========================================================================

const ALL_DOMAINS: [DomainKind; 9] = [
    DomainKind::Worldgen,
    DomainKind::Lighting,
    DomainKind::Meshing,
    DomainKind::PhysicsAndContraptions,
    DomainKind::NetworkBroadcast,
    DomainKind::SpatialAudio,
    DomainKind::ShadowRendering,
    DomainKind::Procedural3dAndBer,
    DomainKind::VfxAndAtmosphere,
];

/// Exercise every public helper a low-spec host can reach: strategy
/// resolution, domain fan-out, roofline classification, autotuner
/// feedback and the packed-nibble kernels. Any panic, underflow or zero
/// budget fails the test.
fn assert_low_spec_plan_is_sane(plan: &megu_core::hw_adaptive_engine::UnifiedOptimizationPlan) {
    assert!(plan.compute.workers.foreground_threads >= 1);
    assert!(plan.compute.workers.background_threads >= 1);
    assert!(
        plan.compute.workers.foreground_threads + plan.compute.workers.background_threads
            <= plan.profile.base.logical_cores.max(2)
    );
    for domain in ALL_DOMAINS {
        let specialized = plan.for_domain(domain);
        assert!(
            specialized.worker_threads >= 1,
            "{domain:?} got zero workers"
        );
        assert!(
            specialized.worker_threads <= plan.profile.base.logical_cores.max(1),
            "{domain:?} oversubscribed"
        );
        assert!(
            specialized.pool_budget_pages > 0,
            "{domain:?} zero pool budget"
        );
        assert!(specialized.tile_edge_chunks >= 1);
    }
    let roofline = plan.roofline_model();
    assert!(roofline.peak_gflops.is_finite() && roofline.peak_gflops > 0.0);
    assert!(roofline.bandwidth_gbs > 0.0);
    let verdict = roofline.classify_kernel(8.0, 64.0);
    assert!(verdict.attainable_gflops().is_finite());
    let _ = roofline.recommended_strategy(8.0, 64.0);
}

#[test]
fn core2duo_profiles_never_starve_panic_or_oversubscribe() {
    // Core 2 Duo class: 2 logical cores, SSE2 only (no SSE4.2/POPCNT/AVX),
    // 2–4 GiB RAM, spinning rust or a cheap SSD, typically a discrete GPU.
    for total_ram_mib in [2048u64, 4096] {
        for storage_io in [StorageIoClass::HddOrSlowSd, StorageIoClass::SataSsd] {
            let mut duo = synthetic_desktop();
            duo.base = caps(
                2,
                512, // tiny L2/L3 — FlashAttention-style recompute tier
                false,
                SimdKernelTier::Scalar, // SSE2-only part: scalar SWAR path
                MemoryTopology::DiscreteVram,
            );
            duo.core_layout = CoreHeterogeneity::Uniform { cores: 2 };
            duo.total_ram_mib = total_ram_mib;
            duo.memory_class = MemoryFootprintClass::Constrained;
            duo.gpu_paradigm = GpuComputeParadigm::DiscretePcieHighBw;
            duo.power_profile = PowerThermalProfile::AcDesktop;
            duo.storage_io = storage_io;
            duo.extended_simd = ExtendedSimdTier::Scalar;
            duo.cpu_vendor = CpuVendor::Intel;

            // Legacy entry point: exactly one foreground + one background
            // worker on two cores — never zero, never thrashing.
            let strategy = select_compute_strategy(&duo.base);
            assert_eq!(strategy.simd, SimdKernelTier::Scalar);
            assert_eq!(strategy.workers.foreground_threads, 1);
            assert_eq!(strategy.workers.background_threads, 1);
            assert_eq!(strategy.cache, CacheStrategy::FlashRecomputeStream);
            assert_eq!(strategy.chunk_batch_edge(), 1);

            let plan = build_unified_optimization_plan(&duo, None);
            // Constrained RAM forces the recompute kernel regardless of L3.
            assert_eq!(plan.compute.cache, CacheStrategy::FlashRecomputeStream);
            assert_eq!(plan.kernel_mode, ComputeKernelMode::RecomputeSwar);
            assert_eq!(
                plan.memory_transfer,
                MemoryTransferStrategy::DiscreteStagedArena
            );
            assert_low_spec_plan_is_sane(&plan);

            let worldgen = plan.for_domain(DomainKind::Worldgen);
            assert_eq!(worldgen.pool_budget_pages, 256);
            assert_eq!(worldgen.pool_budget_pages * PAGE_SIZE, 1024 * 1024);
            assert_eq!(
                worldgen.coalesce_background_bursts,
                storage_io == StorageIoClass::HddOrSlowSd
            );

            // Scalar (SSE2-safe) kernel output is bit-exact vs reference.
            let (a, b) = deterministic_buffers();
            let reference = reference_nibble_max(&a, &b);
            assert_eq!(
                max_nibble_arrays_tiered(&a, &b, SimdKernelTier::Scalar, false),
                reference
            );
            assert_eq!(max_nibble_arrays_adaptive(&a, &b, &strategy), reference);

            // Autotuner feedback on the constrained tier cannot panic or
            // produce a zero/invalid scale path.
            let mut tuner = KernelAutotuner::new(plan.kernel_mode);
            for i in 0..16u64 {
                tuner.observe_sample(plan.kernel_mode, 1_000 + i);
                tuner.observe_sample(
                    match plan.kernel_mode {
                        ComputeKernelMode::LutBatch => ComputeKernelMode::RecomputeSwar,
                        ComputeKernelMode::RecomputeSwar => ComputeKernelMode::LutBatch,
                    },
                    900 + i,
                );
            }
        }
    }
}

#[test]
fn single_core_low_ram_extreme_fallback_stays_positive() {
    // Absolute floor: 1 core, 1 GiB, scalar-only, headless slow storage.
    let mut tiny = synthetic_desktop();
    tiny.base = caps(
        1,
        0,
        false,
        SimdKernelTier::Scalar,
        MemoryTopology::UnifiedUma,
    );
    tiny.core_layout = CoreHeterogeneity::Uniform { cores: 1 };
    tiny.total_ram_mib = 1024;
    tiny.memory_class = MemoryFootprintClass::Constrained;
    tiny.gpu_paradigm = GpuComputeParadigm::HeadlessNoGpu;
    tiny.power_profile = PowerThermalProfile::ServerHeadless;
    tiny.storage_io = StorageIoClass::HddOrSlowSd;
    tiny.extended_simd = ExtendedSimdTier::Scalar;
    tiny.cpu_vendor = CpuVendor::Other;

    let strategy = select_compute_strategy(&tiny.base);
    assert_eq!(strategy.workers.foreground_threads, 1);
    assert!(strategy.workers.background_threads >= 1);

    let plan = build_unified_optimization_plan(&tiny, None);
    assert_low_spec_plan_is_sane(&plan);
    for domain in ALL_DOMAINS {
        assert_eq!(plan.for_domain(domain).worker_threads, 1);
    }

    // Every helper still answers on the degenerate profile.
    let (a, b) = deterministic_buffers();
    assert_eq!(
        max_nibble_arrays_tiered(&a, &b, SimdKernelTier::Scalar, true),
        reference_nibble_max(&a, &b)
    );
    let mut mask = SparseDirtyTileMask::empty();
    mask.mark_block(3, 3, 3);
    assert_eq!(mask.dirty_count(), 1);
    let mut prefetcher = SpeculativeChunkPrefetcher::new();
    assert!(
        prefetcher
            .update(0.0, 0.0, 1.0, 0.0, 4, 8, |_, _| false)
            .len()
            <= 8
    );
    let mut pool = PagedSectionPool::new();
    let mut handle = pool.alloc_uniform(0x00);
    assert!(pool.write_byte_cow(&mut handle, 0, 0x01).is_ok());
    assert!(pool.release(&mut handle));
}

#[test]
fn snapdragon_arm64_profiles_get_neon_uma_high_tier() {
    // Snapdragon X Elite / X Plus class: 12 logical cores in a
    // big.LITTLE-style cpu_capacity layout, 16–32 GiB LPDDR5X, Adreno
    // tile-based GPU on unified memory, NVMe storage.
    for total_ram_mib in [16 * 1024u64, 32 * 1024] {
        let mut snap = synthetic_desktop();
        snap.base = caps(
            12,
            16 * 1024, // shared L3 slice visible to cpu0
            true,      // cpu_capacity marks P/E clusters
            SimdKernelTier::Neon,
            MemoryTopology::UnifiedUma,
        );
        snap.l2_per_core_kib = 1024;
        snap.cache_line_bytes = 64;
        snap.cache_domains = CacheDomainTopology::SingleSharedLlc;
        snap.core_layout = CoreHeterogeneity::Hybrid {
            p_cores: 4,
            e_cores: 8,
        };
        snap.total_ram_mib = total_ram_mib;
        snap.memory_class = MemoryFootprintClass::Abundant;
        snap.gpu_paradigm = GpuComputeParadigm::TileBasedTbdr;
        snap.power_profile = PowerThermalProfile::BatteryLaptop;
        snap.storage_io = StorageIoClass::NvmeFastRandom;
        snap.extended_simd = ExtendedSimdTier::Neon;
        snap.cpu_vendor = CpuVendor::Qualcomm;

        let strategy = select_compute_strategy(&snap.base);
        assert_eq!(strategy.simd, SimdKernelTier::Neon);
        // Hybrid 12-core part: half the cores soak background work.
        assert_eq!(strategy.workers.foreground_threads, 6);
        assert_eq!(strategy.workers.background_threads, 6);

        let plan = build_unified_optimization_plan(&snap, None);
        assert_eq!(plan.compute.simd, SimdKernelTier::Neon);
        assert_eq!(
            plan.memory_transfer,
            MemoryTransferStrategy::UnifiedZeroCopy
        );
        // 16 MiB L3 → BalancedTile: LUT-resident, two-column batches.
        assert_eq!(plan.compute.cache, CacheStrategy::BalancedTile);
        assert_eq!(plan.compute.chunk_batch_edge(), 2);
        assert_eq!(plan.kernel_mode, ComputeKernelMode::LutBatch);
        assert_low_spec_plan_is_sane(&plan);

        // Critical domains pin to the P-cluster, throughput domains to E.
        let lighting = plan.for_domain(DomainKind::Lighting);
        let worldgen = plan.for_domain(DomainKind::Worldgen);
        assert_eq!(lighting.core_affinity, CoreAffinityHint::Performance);
        assert_eq!(worldgen.core_affinity, CoreAffinityHint::Efficiency);
        assert_eq!(lighting.worker_threads, 4);
        assert_eq!(worldgen.worker_threads, 8);
        assert!(!lighting.pin_to_primary_llc_domain);
        assert!(worldgen.coalesce_background_bursts); // battery profile
        assert!(lighting.use_lut_over_recompute);
        assert_eq!(worldgen.pool_budget_pages, 8_192);

        // NEON-tier kernel output is bit-exact vs the scalar reference.
        let (a, b) = deterministic_buffers();
        let reference = reference_nibble_max(&a, &b);
        assert_eq!(
            max_nibble_arrays_tiered(&a, &b, SimdKernelTier::Neon, false),
            reference
        );
        assert_eq!(
            max_nibble_arrays_tiered(&a, &b, SimdKernelTier::Neon, true),
            reference
        );
        assert_eq!(max_nibble_arrays_adaptive(&a, &b, &strategy), reference);

        // Roofline model is finite and compute-positive on ARM64 NEON.
        let roofline = plan.roofline_model();
        assert!(roofline.peak_gflops > 0.0 && roofline.peak_gflops.is_finite());
        assert!(roofline.ridge_intensity.is_finite() && roofline.ridge_intensity > 0.0);
    }
}

#[test]
fn snapdragon_uniform_oryon_layout_still_scales() {
    // Some Snapdragon parts expose a uniform cpu_capacity — the plan must
    // not depend on big.LITTLE being detected.
    let mut snap = synthetic_desktop();
    snap.base = caps(
        12,
        8 * 1024,
        false,
        SimdKernelTier::Neon,
        MemoryTopology::UnifiedUma,
    );
    snap.core_layout = CoreHeterogeneity::Uniform { cores: 12 };
    snap.total_ram_mib = 16 * 1024;
    snap.memory_class = MemoryFootprintClass::Abundant;
    snap.gpu_paradigm = GpuComputeParadigm::TileBasedTbdr;
    snap.storage_io = StorageIoClass::NvmeFastRandom;
    snap.extended_simd = ExtendedSimdTier::Neon;
    snap.cpu_vendor = CpuVendor::Qualcomm;

    let strategy = select_compute_strategy(&snap.base);
    // Uniform 12 cores: a quarter to background.
    assert_eq!(strategy.workers.background_threads, 3);
    assert_eq!(strategy.workers.foreground_threads, 9);
    let plan = build_unified_optimization_plan(&snap, None);
    assert_eq!(plan.for_domain(DomainKind::Lighting).worker_threads, 9);
    assert_eq!(plan.for_domain(DomainKind::Worldgen).worker_threads, 3);
    assert_low_spec_plan_is_sane(&plan);
}

#[test]
fn cpu_vendor_classification_recognizes_snapdragon_and_falls_back() {
    // Empty / unreadable cpuinfo.
    assert_eq!(classify_cpu_vendor(""), CpuVendor::Unknown);
    assert_eq!(classify_cpu_vendor("   \n\t\n"), CpuVendor::Unknown);

    // Snapdragon X Elite / X Plus: implementer 0x51 alone is enough.
    assert_eq!(
        classify_cpu_vendor("processor\t: 0\nCPU implementer\t: 0x51\nCPU part\t: 0x001\n"),
        CpuVendor::Qualcomm
    );
    // Brand strings alone are also sufficient (Kryo/Oryon marketing names).
    assert_eq!(
        classify_cpu_vendor("Hardware\t: Qualcomm Technologies, Inc. SM8550\n"),
        CpuVendor::Qualcomm
    );
    assert_eq!(
        classify_cpu_vendor("model name\t: Snapdragon X Elite\n"),
        CpuVendor::Qualcomm
    );
    assert_eq!(
        classify_cpu_vendor("Processor\t: Qualcomm Kryo 485 Gold\n"),
        CpuVendor::Qualcomm
    );

    // Other ARM implementers.
    assert_eq!(
        classify_cpu_vendor("CPU implementer\t: 0x41\n"),
        CpuVendor::Arm
    );
    assert_eq!(
        classify_cpu_vendor("CPU implementer\t: 0x61\n"),
        CpuVendor::Apple
    );
    assert_eq!(
        classify_cpu_vendor("CPU implementer\t: 0x53\n"),
        CpuVendor::Samsung
    );
    assert_eq!(
        classify_cpu_vendor("CPU implementer\t: 0x4e\n"),
        CpuVendor::Nvidia
    );

    // x86 vendor_id strings.
    assert_eq!(
        classify_cpu_vendor("vendor_id\t: GenuineIntel\nmodel name\t: Intel Core2 Duo\n"),
        CpuVendor::Intel
    );
    assert_eq!(
        classify_cpu_vendor("vendor_id\t: AuthenticAMD\n"),
        CpuVendor::Amd
    );

    // Unrecognized but non-empty cpuinfo degrades to Other, never panics.
    assert_eq!(
        classify_cpu_vendor("garbage with no colons"),
        CpuVendor::Other
    );
    assert_eq!(
        classify_cpu_vendor("CPU implementer\t: 0xc0\n"),
        CpuVendor::Other
    );
    assert_eq!(
        classify_cpu_vendor("vendor_id\t: SomethingElse\n"),
        CpuVendor::Other
    );
}

#[test]
fn prefetch_queue_stays_bounded_when_speculative_cap_is_huge() {
    // `usize::MAX` as the per-batch cap must not turn the four-batch
    // retention bound into usize::MAX: the queue is clamped by the
    // maximum forward-cone size instead.
    let mut p = SpeculativeChunkPrefetcher::new();
    for i in 0..64u32 {
        // Alternating headings force a generation bump each update, so
        // stale entries pile up exactly like a zig-zagging player.
        let dir = if i % 2 == 0 { 1.0 } else { -1.0 };
        p.update(0.0, 0.0, dir, 0.0, 32, usize::MAX, |_, _| false);
    }
    let queued = p.entries().count();
    // 4 batches x (2*32+1)^2 cone cells = 16_900 hard ceiling.
    assert!(queued <= 16_900, "queue grew to {queued}");
    // Pending entries still resolve correctly after the clamp.
    assert!(p.pending_count() <= queued);
}
