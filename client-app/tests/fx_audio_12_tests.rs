// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane FX-AUDIO-12: quality-preserving speedups for the spatial audio DSP
//! and the acoustic raytracer.
//!
//! * `[FX-1]` Block-head early-reflection precompute + bitmask history ring +
//!   one-pole bypass + linear-guarded rational soft clip + shared 8-channel
//!   FDN bus.
//! * `[FX-2]` Two-level macro-DDA over uniform-air 16³ sections + 2×2×2 m
//!   LRU [`AcousticSceneCache`] + distance-ordered branch-and-bound
//!   diffraction — all verified bit-exact against the reference algorithms.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::f32::consts::PI;

use mc_spine_client::spatial_audio::{
    precomputed_er_gains, soft_clip, AcousticMaterial, AcousticScene, AcousticSceneCache,
    AcousticTracer, DenseVoxelGrid, EarlyReflectionTap, ListenerPose, MaterialClass, OcclusionInfo,
    OnePoleLp, SharedListenerFdnBus, SoundSource, SpatialAudioEngine, SpatialAudioPreset,
    VoxelAcousticWorld, OCCLUSION_MAX_CUTOFF_HZ, OUTPUT_LIMIT, SPEED_OF_SOUND_M_S,
};

const FS: u32 = 48_000;

fn impulse(n: usize) -> Vec<f32> {
    let mut v = vec![0.0; n];
    v[0] = 0.8;
    v
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .max_by(|a, b| {
            a.1.abs()
                .partial_cmp(&b.1.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(usize::MAX)
}

/// Reference (pre-optimization) per-sample tap math: rotate the direction by
/// listener yaw, take the pan azimuth, fold band-weighted amplitude into
/// equal-power ear gains.
fn reference_tap(tap: &EarlyReflectionTap, yaw_deg: f32) -> (f32, f32, f32) {
    let yaw = yaw_deg.to_radians();
    let (sy, cy) = yaw.sin_cos();
    let fwd = [sy, 0.0_f32, cy];
    // cross(fwd, up) where up = +Y → (-cy, 0, sy)
    let right_raw = [-cy, 0.0_f32, sy];
    let n = (right_raw[0] * right_raw[0] + right_raw[2] * right_raw[2]).sqrt();
    let right = if n > 1e-9 {
        [right_raw[0] / n, 0.0, right_raw[2] / n]
    } else {
        [0.0, 0.0, 1.0]
    };
    let rel = [
        tap.direction[0] * right[0] + tap.direction[1] * right[1] + tap.direction[2] * right[2],
        tap.direction[1],
        tap.direction[0] * fwd[0] + tap.direction[2] * fwd[2],
    ];
    let pan = rel[0].atan2(rel[2]).clamp(-PI * 0.5, PI * 0.5) / (PI * 0.5);
    let amp = (0.5 * tap.energy_mid + 0.3 * tap.energy_low + 0.2 * tap.energy_high).sqrt();
    (
        tap.delay_samples as f32,
        amp * ((1.0 - pan) * 0.5).sqrt(),
        amp * ((1.0 + pan) * 0.5).sqrt(),
    )
}

/// Open-air world: earth ground slab below, infinite air above.
fn outdoor() -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new([-24, -16, -24], [48, 32, 48], AcousticMaterial::AIR);
    g.fill_box(
        [-24, -16, -24],
        [23, -2, 23],
        AcousticMaterial::from_class(MaterialClass::Earth),
    );
    g
}

#[test]
fn fx1_precomputed_er_and_bitmask_ring_match_reference_and_bypass_lp() {
    // --- Block-head precompute matches the per-sample reference math. ---
    let norm = |v: [f32; 3]| {
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        [v[0] / n, v[1] / n, v[2] / n]
    };
    let taps = vec![
        EarlyReflectionTap {
            delay_samples: 137,
            direction: norm([-1.0, 0.2, 0.6]),
            energy_low: 0.50,
            energy_mid: 0.62,
            energy_high: 0.31,
        },
        EarlyReflectionTap {
            delay_samples: 977,
            direction: norm([0.8, -0.1, -0.4]),
            energy_low: 0.20,
            energy_mid: 0.44,
            energy_high: 0.18,
        },
        EarlyReflectionTap {
            delay_samples: 2501,
            direction: norm([0.1, 0.9, 0.2]),
            energy_low: 0.33,
            energy_mid: 0.29,
            energy_high: 0.41,
        },
    ];
    for yaw in [0.0_f32, 30.0, -97.5, 179.9] {
        let pre = precomputed_er_gains(&taps, yaw);
        assert_eq!(pre.len(), taps.len());
        for (p, tap) in pre.iter().zip(taps.iter()) {
            let (rd, rl, rr) = reference_tap(tap, yaw);
            assert!(
                (p[0] - rd).abs() < 1e-5 && (p[1] - rl).abs() < 1e-5 && (p[2] - rr).abs() < 1e-5,
                "yaw {yaw}: precomputed {p:?} vs reference ({rd}, {rl}, {rr})"
            );
        }
    }

    // --- End-to-end: taps land at the expected sample offset with the
    //     expected stereo gain ratio through the bitmask history ring. ---
    let world = outdoor();
    let listener = ListenerPose {
        position: [0.5, 0.5, 0.5],
        yaw_deg: 0.0,
        ..ListenerPose::default()
    };
    let source = SoundSource {
        position: [0.5, 0.5, 20.5],
        ..SoundSource::default()
    };
    let mut engine = SpatialAudioEngine::new(FS, SpatialAudioPreset::Ultra);
    engine.analyze(&world, &source, &listener);
    // Craft a scene: keep the traced clear occlusion, inject two taps —
    // one panned hard left, one hard right — on an otherwise dry scene.
    let mut scene = engine.scene().clone();
    scene.rt60_low_s = 0.0;
    scene.rt60_mid_s = 0.0;
    scene.rt60_high_s = 0.0;
    scene.wet_gain = 0.0;
    let tap_delay = 600_u32;
    scene.early_reflections = vec![EarlyReflectionTap {
        delay_samples: tap_delay,
        direction: norm([-1.0, 0.0, 0.0]), // hard right of +Z facing
        energy_low: 0.6,
        energy_mid: 0.6,
        energy_high: 0.6,
    }];
    engine.set_scene(scene);
    // Re-run geometry so wet_send/er_gain reflect the crafted dry scene.
    engine.set_source(source);

    let mut out = Vec::new();
    engine.process(&impulse(8192), &mut out);
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let right: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();

    // Expected propagation offset: dist 20 m → base delay d/c·fs, re-anchored
    // four samples short of it (the engine's drift clamp).
    let dist = 20.0_f32;
    let base = dist / SPEED_OF_SOUND_M_S * FS as f32;
    let expected_direct = (base - 4.0).round() as usize;
    let direct_idx = argmax(&left);
    assert!(
        direct_idx.abs_diff(expected_direct) <= 4,
        "direct peak {direct_idx} vs expected ~{expected_direct}"
    );
    // The early tap repeats the impulse `tap_delay` samples later.
    let lo = expected_direct + tap_delay as usize - 8;
    let tap_region = &right[lo..lo + 24];
    assert!(
        tap_region.iter().any(|s| s.abs() > 1e-3),
        "no early-reflection energy near sample {lo} on the right channel"
    );
    // Hard-right tap: right channel must dominate left at the tap.
    let l_tap = left[lo..lo + 24].iter().fold(0.0_f32, |a, s| a.max(s.abs()));
    let r_tap = right[lo..lo + 24]
        .iter()
        .fold(0.0_f32, |a, s| a.max(s.abs()));
    assert!(
        r_tap > l_tap * 2.0,
        "right-panned tap l={l_tap} r={r_tap} should favor the right ear"
    );

    // --- OnePoleLp bypass is a bit-exact identity. ---
    // 24 kHz: the 20 kHz bypass ceiling clears 0.48·fs (11.52 kHz).
    let mut lp = OnePoleLp::new(OCCLUSION_MAX_CUTOFF_HZ);
    lp.settle(1.0, 24_000.0);
    for &x in &[0.0_f32, 0.3, -0.7, 0.999_999, -1.5, 1e-6] {
        assert_eq!(lp.process(x).to_bits(), x.to_bits(), "bypass not identity for {x}");
    }
    // Poisoned input still flushes to zero through the bypass.
    assert_eq!(lp.process(f32::NAN), 0.0);
    assert_eq!(lp.process(1e-30), 0.0);

    // --- soft_clip: strictly linear under the knee, bounded above it. ---
    for i in -85..=85 {
        let x = i as f32 / 100.0;
        assert_eq!(soft_clip(x).to_bits(), x.to_bits(), "not linear at {x}");
    }
    let mut prev = soft_clip(0.85);
    for i in 86..400 {
        let x = i as f32 / 100.0;
        let y = soft_clip(x);
        assert!(y > prev, "not monotone at {x}: {y} <= {prev}");
        assert!(y.abs() <= 1.0 && y <= OUTPUT_LIMIT, "out of bounds: {y}");
        prev = y;
    }
    assert_eq!(soft_clip(-4.0), -soft_clip(4.0), "clipper must stay odd");
    // Asymptotic bound stays inside both OUTPUT_LIMIT and the [-1, 1] spec.
    assert!(soft_clip(1e30).abs() <= OUTPUT_LIMIT && soft_clip(1e30).abs() <= 1.0);
    assert!(soft_clip(-1e30).abs() <= OUTPUT_LIMIT && soft_clip(-1e30).abs() <= 1.0);
}

#[test]
fn fx1_shared_8ch_fdn_bus_matches_linear_sum_of_individual_fdns() {
    let rt60 = [1.6_f32, 0.9, 0.4];
    let mut shared = SharedListenerFdnBus::new(FS);
    shared.set_rt60(rt60);

    let mut independent: Vec<SharedListenerFdnBus> = (0..4)
        .map(|_| {
            let mut b = SharedListenerFdnBus::new(FS);
            b.set_rt60(rt60);
            b
        })
        .collect();

    // Four distinct voices: different tones, amplitudes, and wet sends.
    let freqs = [220.0_f32, 331.0, 523.25, 97.0];
    let amps = [0.5_f32, 0.3, 0.7, 0.4];
    let sends = [0.7_f32, 0.4, 0.25, 0.9];
    let voices: Vec<Vec<f32>> = (0..4)
        .map(|v| {
            (0..4096)
                .map(|i| (2.0 * PI * freqs[v] * i as f32 / FS as f32).sin() * amps[v])
                .collect()
        })
        .collect();

    let [v0, v1, v2, v3] = voices.as_slice() else {
        unreachable!()
    };
    let frames = v0.iter().zip(v1).zip(v2).zip(v3);
    let mut max_diff = 0.0_f32;
    let mut energy = 0.0_f32;
    for (((&s0, &s1), &s2), &s3) in frames {
        let frame = [s0, s1, s2, s3];
        for (v, &s) in frame.iter().enumerate() {
            shared.send(s, sends[v]);
        }
        let (sl, sr) = shared.step();

        let (mut il, mut ir) = (0.0_f32, 0.0_f32);
        for (v, bus) in independent.iter_mut().enumerate() {
            bus.send(frame[v], sends[v]);
            let (l, r) = bus.step();
            il += l;
            ir += r;
        }
        energy += sl * sl + sr * sr;
        max_diff = max_diff.max((sl - il).abs()).max((sr - ir).abs());
    }

    // LTI superposition: the summed send through one FDN equals the sum of
    // four independent FDNs to well under the 1e-4 bound.
    assert!(max_diff < 1e-4, "shared bus diverged by {max_diff}");
    assert!(
        energy / 4096.0 > 1e-8,
        "shared bus is silent — FDN is not running"
    );
}

/// World wrapper that knows which 16³ sections are uniform air and counts
/// how many material queries the tracer actually issues.
struct MacroWorld<'g> {
    grid: &'g DenseVoxelGrid,
    air_sections: BTreeSet<(i32, i32, i32)>,
    queries: Cell<u64>,
}

impl<'g> MacroWorld<'g> {
    /// Scan every section overlapping `[min, max]` (inclusive cell range);
    /// a section is macro-skippable when all 4096 of its cells read as air.
    fn new(grid: &'g DenseVoxelGrid, min: [i32; 3], max: [i32; 3]) -> Self {
        let mut air_sections = BTreeSet::new();
        for sz in min[2].div_euclid(16)..=max[2].div_euclid(16) {
            for sy in min[1].div_euclid(16)..=max[1].div_euclid(16) {
                for sx in min[0].div_euclid(16)..=max[0].div_euclid(16) {
                    let all_air = (0..16).all(|dy| {
                        (0..16).all(|dz| {
                            (0..16).all(|dx| {
                                grid.get(sx * 16 + dx, sy * 16 + dy, sz * 16 + dz)
                                    == AcousticMaterial::AIR
                            })
                        })
                    });
                    if all_air {
                        air_sections.insert((sx, sy, sz));
                    }
                }
            }
        }
        Self {
            grid,
            air_sections,
            queries: Cell::new(0),
        }
    }

    fn query_count(&self) -> u64 {
        self.queries.get()
    }
}

/// Naive world that still answers `is_uniform_air_section` with the default
/// `false`, while counting queries for comparison.
struct NaiveWorld<'g> {
    grid: &'g DenseVoxelGrid,
    queries: Cell<u64>,
}

impl NaiveWorld<'_> {
    fn query_count(&self) -> u64 {
        self.queries.get()
    }
}

impl VoxelAcousticWorld for MacroWorld<'_> {
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        self.queries.set(self.queries.get() + 1);
        self.grid.get(x, y, z)
    }

    fn is_uniform_air_section(&self, sx: i32, sy: i32, sz: i32) -> bool {
        self.air_sections.contains(&(sx, sy, sz))
    }
}

impl VoxelAcousticWorld for NaiveWorld<'_> {
    fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
        self.queries.set(self.queries.get() + 1);
        self.grid.get(x, y, z)
    }
}

/// Stone wall slab across x∈[-12..11], y∈[-8..11], z∈[2..5] (4 thick) with an
/// optional doorway portal (x∈[2..5], y∈[-1..1]) — same fixture the occlusion
/// tests use, so the diffraction path is a real doorway.
fn wall_world(door: bool) -> DenseVoxelGrid {
    let mut g = DenseVoxelGrid::new([-24, -16, -24], [48, 32, 48], AcousticMaterial::AIR);
    g.fill_box(
        [-12, -8, 2],
        [11, 11, 5],
        AcousticMaterial::from_class(MaterialClass::StoneLike),
    );
    if door {
        g.fill_box([2, -1, 2], [5, 1, 5], AcousticMaterial::AIR);
    }
    g
}

#[test]
fn fx2_macro_dda_matches_voxel_dda_and_bnb_diffraction_cuts_rays_exact_loss() {
    // --- Macro-DDA: bit-identical scene with far fewer cell queries. ---
    let mut grid = DenseVoxelGrid::new([-32, -16, -32], [64, 48, 64], AcousticMaterial::AIR);
    // Earth floor slab.
    grid.fill_box(
        [-32, -10, -32],
        [31, -9, 31],
        AcousticMaterial::from_class(MaterialClass::Earth),
    );
    // Stone room shell at x∈[16..23], y∈[-2..5], z∈[16..23], air interior.
    let stone = AcousticMaterial::from_class(MaterialClass::StoneLike);
    grid.fill_box([16, -2, 16], [23, 5, 23], stone);
    grid.fill_box([17, -1, 17], [22, 4, 22], AcousticMaterial::AIR);
    // A freestanding wool pillar (soft absorber).
    grid.fill_box(
        [-20, -8, 10],
        [-18, 2, 12],
        AcousticMaterial::from_class(MaterialClass::SoftAbsorber),
    );

    // Listener and source high above the floor: most of the Fibonacci
    // bundle traverses pure air sections (plus open sky beyond the grid).
    let source = [0.5, 20.5, 0.5];
    let listener = [0.5, 20.5, 8.5];
    let naive_world = NaiveWorld {
        grid: &grid,
        queries: Cell::new(0),
    };
    // Scan generously: sections outside the dense grid are still uniform
    // air to the acoustic world (out-of-bounds reads are AIR).
    let macro_world = MacroWorld::new(&grid, [-48, -16, -48], [47, 96, 47]);
    assert!(
        !macro_world.air_sections.is_empty(),
        "fixture must contain uniform-air sections"
    );

    let naive_tracer = AcousticTracer::new(&naive_world, SpatialAudioPreset::Ultra);
    let naive_scene = naive_tracer.compute_scene(source, listener, FS);
    let naive_queries = naive_world.query_count();

    let macro_tracer = AcousticTracer::new(&macro_world, SpatialAudioPreset::Ultra);
    let macro_scene = macro_tracer.compute_scene(source, listener, FS);
    let macro_queries = macro_world.query_count();

    // Hit coordinates/distances/normals must be identical — tap directions
    // and delay samples are derived directly from cast results.
    assert_eq!(
        naive_scene.early_reflections, macro_scene.early_reflections,
        "macro-DDA changed early reflections"
    );
    assert_eq!(naive_scene.rays_traced, macro_scene.rays_traced);
    assert_eq!(
        naive_scene.outdoor_openness, macro_scene.outdoor_openness,
        "escape fraction changed"
    );
    assert_eq!(naive_scene.occlusion, macro_scene.occlusion);
    for (a, b, name) in [
        (naive_scene.rt60_low_s, macro_scene.rt60_low_s, "rt60_low"),
        (naive_scene.rt60_mid_s, macro_scene.rt60_mid_s, "rt60_mid"),
        (naive_scene.rt60_high_s, macro_scene.rt60_high_s, "rt60_high"),
        (naive_scene.wet_gain, macro_scene.wet_gain, "wet_gain"),
    ] {
        assert_eq!(a.to_bits(), b.to_bits(), "{name} differs: {a} vs {b}");
    }

    assert!(
        macro_tracer.macro_dda_skips() > 0,
        "no sections were warped — fixture cannot prove the macro path ran"
    );
    eprintln!(
        "macro-DDA: {} section warps, cell queries {} -> {} ({:.1}x fewer)",
        macro_tracer.macro_dda_skips(),
        naive_queries,
        macro_queries,
        naive_queries as f64 / macro_queries.max(1) as f64
    );
    assert!(
        macro_queries * 2 < naive_queries,
        "macro queries {macro_queries} should be under half of naive {naive_queries}"
    );

    // segment_loss_db takes the same warp: occlusion through macro world is
    // bit-identical to the naive world.
    let occ_naive = naive_tracer.occlusion_between(listener, source);
    let occ_macro = macro_tracer.occlusion_between(listener, source);
    assert_eq!(occ_naive, occ_macro);

    // --- AcousticSceneCache: same-cell hits, revision invalidation, LRU. ---
    let mut cache = AcousticSceneCache::new(64);
    assert_eq!(cache.capacity(), 64);
    assert_eq!(AcousticSceneCache::new(1).capacity(), AcousticSceneCache::MIN_CAPACITY);
    assert_eq!(
        AcousticSceneCache::new(1_000_000).capacity(),
        AcousticSceneCache::MAX_CAPACITY
    );

    let cell_a = AcousticSceneCache::cell_of([3.7, 1.2, -5.9]);
    let cell_a2 = AcousticSceneCache::cell_of([3.9, 1.99, -5.01]);
    let cell_b = AcousticSceneCache::cell_of([4.1, 1.2, -5.9]);
    assert_eq!(cell_a, cell_a2, "positions inside one 2 m cell must share a key");
    assert_ne!(cell_a, cell_b, "crossing a 2 m boundary must change the key");

    assert!(cache.get(cell_a, 7).is_none());
    assert_eq!(cache.misses(), 1);
    cache.insert(cell_a, 7, naive_scene.clone());
    let hit = cache.get(cell_a, 7).expect("fresh insert must hit");
    assert_eq!(hit.rays_traced, naive_scene.rays_traced);
    assert_eq!(cache.hits(), 1);
    // Same cell via a moved position inside it.
    assert!(cache.get(cell_a2, 7).is_some());
    // Revision bump invalidates.
    assert!(cache.get(cell_a, 8).is_none());
    cache.insert(cell_a, 8, naive_scene.clone());
    assert!(cache.get(cell_a, 8).is_some());
    assert!(cache.get(cell_a, 7).is_none(), "stale revision must stay dead");

    // LRU bound: inserting more cells than capacity evicts the oldest.
    let mut small = AcousticSceneCache::new(AcousticSceneCache::MIN_CAPACITY);
    for i in 0..(AcousticSceneCache::MIN_CAPACITY as i32 + 8) {
        small.insert([i, 0, 0], 1, AcousticScene::default());
    }
    assert_eq!(small.len(), AcousticSceneCache::MIN_CAPACITY);
    assert!(
        small.get([0, 0, 0], 1).is_none(),
        "oldest cell should have been evicted"
    );
    assert!(
        small
            .get([AcousticSceneCache::MIN_CAPACITY as i32 + 7, 0, 0], 1)
            .is_some(),
        "newest cell must survive"
    );

    // --- Branch-and-bound diffraction: identical minimum loss, fewer rays. ---
    let door = wall_world(true);
    let l_pos = [0.5, 0.5, -4.5];
    let s_pos = [0.5, 0.5, 6.5];

    let bnb_tracer = AcousticTracer::new(&door, SpatialAudioPreset::Ultra);
    let occ_bnb = bnb_tracer.occlusion_between(l_pos, s_pos);
    let rays_bnb = bnb_tracer.diffraction_rays_cast();

    let ex_tracer = AcousticTracer::new(&door, SpatialAudioPreset::Ultra);
    let occ_ex = ex_tracer.occlusion_between_exhaustive(l_pos, s_pos);
    let rays_ex = ex_tracer.diffraction_rays_cast();

    assert_eq!(
        occ_bnb, occ_ex,
        "branch-and-bound changed the minimum-loss result"
    );
    assert!(occ_bnb.diffracted, "doorway detour should register");
    eprintln!(
        "BnB diffraction: doorway rays {} -> {} ({:.1}x fewer)",
        rays_ex,
        rays_bnb,
        rays_ex as f64 / rays_bnb.max(1) as f64
    );
    assert!(
        rays_bnb * 2 <= rays_ex,
        "BnB rays {rays_bnb} should be at most half of exhaustive {rays_ex}"
    );

    // Solid wall (no doorway): still identical loss, and never more rays.
    let solid = wall_world(false);
    let bnb_solid = AcousticTracer::new(&solid, SpatialAudioPreset::Ultra);
    let occ_bnb_solid = bnb_solid.occlusion_between(l_pos, s_pos);
    let ex_solid = AcousticTracer::new(&solid, SpatialAudioPreset::Ultra);
    let occ_ex_solid = ex_solid.occlusion_between_exhaustive(l_pos, s_pos);
    assert_eq!(occ_bnb_solid, occ_ex_solid);
    assert!(
        occ_bnb_solid.transmission_loss_db >= 30.0,
        "4 m of stone must stay heavily occluded: {occ_bnb_solid:?}"
    );
    assert!(
        bnb_solid.diffraction_rays_cast() <= ex_solid.diffraction_rays_cast(),
        "BnB must never cast more rays than exhaustive"
    );

    // OcclusionInfo fields used above must stay complete for downstream DSP.
    let OcclusionInfo {
        transmission_loss_db,
        direct_loss_db,
        cutoff_hz,
        diffracted,
        blocked,
    } = occ_bnb;
    assert!(transmission_loss_db <= direct_loss_db);
    assert!(cutoff_hz > 0.0);
    assert!(diffracted && !blocked);
}
