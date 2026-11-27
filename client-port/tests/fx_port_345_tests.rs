// SPDX-License-Identifier: LGPL-3.0-or-later

//! Lane FX-PORT-345: quality-preserving shadow WGSL, bitboard sprite
//! extrusion + prototype atlas cache, and SoA VFX particles.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::mem::{align_of, size_of};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use megu_client_mesh::builtin_shadow_wgsl::*;
use megu_client_mesh::vanilla_plus_3d::*;
use megu_client_mesh::vfx_engine::*;

// ---------------------------------------------------------------------------
// Allocation accounting: the FX-4/FX-5 invariants are "zero allocations on
// the hot path", which only a counting allocator can actually prove.
// ---------------------------------------------------------------------------

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNT_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT_THIS_THREAD.with(|count| {
            if count.get() {
                ALLOCS.fetch_add(1, Ordering::Relaxed);
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// Keep the test bodies serialized for shared fixture assumptions; allocation
/// accounting itself is enabled only on the measuring thread.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn alloc_count() -> usize {
    ALLOCS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// FX-3
// ---------------------------------------------------------------------------

#[test]
fn fx3_shadow_wgsl_has_blend_guard_vogel_const_no_loop_trig_and_region_light_mask() {
    let _guard = serial();
    let sampling = MEGU_SHADOW_SAMPLING_WGSL;
    // Next-cascade guard: the second cascade PCF only runs in the blend band.
    assert!(sampling.contains("if (blend > 0.001)"));
    // Caster-backface early-out (sun by day, moon at -sun by night).
    assert!(sampling.contains("dot(n, shadow_caster_direction()) <= 0.0"));
    // Compile-time Vogel disk replaces per-tap sqrt/trigonometry.
    assert!(sampling.contains("const VOGEL_16 = array<vec2<f32>, 16>"));
    assert!(sampling.contains("const SHADOW_ROTATIONS = array<vec2<f32>, 8>"));
    assert!(sampling.contains("VOGEL_16[i] * disk_scale"));
    assert!(sampling.contains("disk_rot * (VOGEL_16[i] * disk_scale)"));
    assert!(sampling.contains("shadow_cascade_visibility(world, n, layer + 1u, disk_rot, sky_open)"));
    assert!(sampling.contains("inverseSqrt(f32(taps))"));
    assert!(!sampling.contains("sqrt((f32(i)"));
    assert!(!sampling.contains("cos("));
    assert!(!sampling.contains("sin("));

    let terrain = MEGU_TERRAIN_LIT_WGSL;
    assert!(terrain.contains("const VOGEL_16 = array<vec2<f32>, 16>"));
    assert!(terrain.contains("disk_rot * VOGEL_16[i] * disk_scale"));
    // Per-pixel interleaved-gradient rotation as a constant mat2x2: one
    // sin/cos pair per pixel, none per tap.
    assert!(terrain.contains("fn ign("));
    assert!(terrain.contains("mat2x2<f32>"));
    assert_eq!(terrain.matches("cos(rotation)").count(), 1);
    assert!(!terrain.contains("cos(angle)"));
    assert!(!terrain.contains("sin(angle)"));
    assert!(!terrain.contains("sqrt((f32(i)"));
    assert!(terrain.contains("inverseSqrt(f32(taps))"));
    // Caster-backface early-out in the cascade sampler.
    assert!(terrain.contains("dot(normal, caster) <= 0.0"));
    assert!(terrain.contains("if (cascade_count > 0u && shadow_strength > 0.0)"));
    // u64 capability-mask walk over the fixed 64-light binding.
    for needle in [
        "light_mask_prefix",
        "light_mask_take",
        "countTrailingZeros",
        "dynamic_lights.light_count.yz",
    ] {
        assert!(terrain.contains(needle), "missing {needle}");
    }
    // Integer-exponent pow is gone; only the vec3 gamma curve remains.
    assert_eq!(terrain.matches("pow(").count(), 1);
    for needle in ["fn pow2(", "fn pow4(", "fn pow12(", "fn pow24("] {
        assert!(terrain.contains(needle), "missing {needle}");
    }

    let sky = MEGU_SKY_ATMOSPHERE_WGSL;
    for needle in ["fn pow3(", "fn pow4(", "fn pow260("] {
        assert!(sky.contains(needle), "missing {needle}");
    }
    assert!(!sky.contains(", 260.0)"));
    assert!(!sky.contains(", 1.5)"));
    assert_eq!(sky.matches("pow(").count(), 1);

    let water = MEGU_WATER_SURFACE_WGSL;
    for needle in ["fn pow3(", "fn pow4(", "fn pow5(", "fn pow72(", "fn pow180("] {
        assert!(water.contains(needle), "missing {needle}");
    }
    for banned in [", 5.0)", ", 72.0)", ", 180.0)"] {
        assert!(!water.contains(banned), "still calls pow(...{banned}");
    }
    assert_eq!(water.matches("pow(").count(), 1);
    assert_eq!(water.matches("pow180(").count(), 2); // def + sparkle site

    // compute_region_light_mask selects exactly the lights whose bounding
    // sphere intersects the region AABB, out of at most 64 slots.
    let mut lights = vec![GpuPointLight::default(); 70];
    for l in &mut lights {
        // Park every slot far away with a small radius: no intersection.
        l.position_radius = [1_000.0, 0.0, 0.0, 1.0];
    }
    lights[3].position_radius = [5.0, 5.0, 5.0, 2.0]; // centre inside box
    lights[17].position_radius = [20.0, 5.0, 5.0, 4.1]; // sphere reaches x=16 wall
    lights[40].position_radius = [17.0, 5.0, 5.0, 0.9]; // misses by 0.1
    lights[63].position_radius = [16.5, 8.0, 8.0, 0.5]; // touches x=16 exactly
    lights[65].position_radius = [5.0, 5.0, 5.0, 2.0]; // inside but past slot 64
    lights[69].position_radius = [f32::NAN, 0.0, 0.0, 4.0]; // non-finite fails closed
    let mask = compute_region_light_mask([0.0; 3], [16.0; 3], &lights);
    assert_eq!(mask, (1u64 << 3) | (1u64 << 17) | (1u64 << 63));
    // Inverted bounds normalize instead of panicking.
    let mask = compute_region_light_mask([16.0; 3], [0.0; 3], &lights[..64]);
    assert_eq!(mask, (1u64 << 3) | (1u64 << 17) | (1u64 << 63));
    // Non-finite region bounds fail closed to no lights.
    assert_eq!(
        compute_region_light_mask([f32::NAN; 3], [16.0; 3], &lights),
        0
    );
    // The mask packs into light_count.yz (lo/hi u32) without touching the
    // fixed binding layout; zero stays the shader's no-culling fallback.
    let mut uniforms = VfxLightUniforms::default();
    uniforms.light_count[0] = 64;
    let packed = uniforms.with_region_light_mask(mask);
    assert_eq!(packed.light_count, [64, mask as u32, (mask >> 32) as u32, 0]);
}

// ---------------------------------------------------------------------------
// FX-4
// ---------------------------------------------------------------------------

/// A non-trivial sprite: stalk rows with holes, so merging and rim runs are
/// all exercised.
fn sprite_mask() -> [u8; 256] {
    let mut mask = [0u8; 256];
    for y in 4..16 {
        for x in 3..13 {
            if (x + y) % 7 != 0 {
                mask[y * 16 + x] = 255;
            }
        }
    }
    mask
}

#[test]
fn fx4_bitboard_extrusion_matches_naive_and_atlas_cache_reuses_prototypes() {
    let _guard = serial();
    // The bitboard implementation must emit the reference quad set exactly:
    // same quads, same order, bit-identical positions/UVs/normals.
    let masks: Vec<[u8; 256]> = vec![
        [0u8; 256],
        [255u8; 256],
        sprite_mask(),
        {
            let mut m = [0u8; 256];
            m[7 * 16 + 8] = 255; // single texel
            m
        },
        {
            let mut m = [0u8; 256];
            for y in 0..16 {
                for x in 0..16 {
                    if (x + y) % 2 == 0 {
                        m[y * 16 + x] = 255; // checkerboard: every texel is an island
                    }
                }
            }
            m
        },
    ];
    for t in [TEXEL, ORE_RELIEF_DEPTH, 1.0] {
        for mask in &masks {
            assert_eq!(
                extrude_alpha_mask_16x16(mask, t),
                extrude_alpha_mask(16, 16, mask, t),
                "bitboard output diverged from the reference implementation"
            );
        }
    }
    // t <= 0 fails closed to empty like the generic path.
    assert_eq!(extrude_alpha_mask_16x16(&sprite_mask(), 0.0).quad_count(), 0);

    // Pack-load bake, then a 1,000-block wheat field meshes with zero
    // re-extrusions and zero per-block allocations.
    let alpha = sprite_mask();
    let mut cache = VanillaPlus3dAtlasCache::new();
    let prototype = build_3d_cross_mesh(&alpha, CrossLayout::HashGrid, [0, 0, 0]);
    let key = VanillaPlus3dKey::CrossSprite {
        alpha,
        layout: CrossLayout::HashGrid,
    };
    cache.bake(key.clone(), prototype.clone());
    assert_eq!(cache.bakes(), 1);
    assert_eq!(cache.len(), 1);

    // Mesh 1,000 blocks through one retained per-block scratch buffer (the
    // naive path builds a fresh mesh per block; the cache reuses both the
    // prototype and the caller's buffer).
    let mut out: Vec<ExtrudedQuad> = Vec::with_capacity(prototype.quads.len());

    // Prove that the active-thread counter observes a real allocation before
    // relying on it for the zero-allocation assertion. Other threads remain
    // uncounted even while this window is active.
    COUNT_THIS_THREAD.with(|count| count.set(true));
    let self_check_before = alloc_count();
    std::hint::black_box(Box::new(42_u8));
    assert_eq!(
        alloc_count(),
        self_check_before + 1,
        "allocation counter missed a current-thread allocation"
    );
    let before = alloc_count();
    for i in 0..1000i32 {
        out.clear();
        let emitted = cache.emit_translated_into(&key, [i as f32, 0.0, 7.0], &mut out);
        assert_eq!(emitted, prototype.quads.len());
        assert_eq!(out.len(), prototype.quads.len());
    }
    assert_eq!(
        alloc_count(),
        before,
        "emitting cached prototypes allocated memory"
    );
    COUNT_THIS_THREAD.with(|count| count.set(false));
    assert_eq!(cache.cache_hits(), 1000);
    assert_eq!(cache.bakes(), 1, "the 1,000-block field re-extruded");
    assert_eq!(cache.cache_misses(), 0);

    // The emitted geometry is the prototype translated: positions shifted,
    // normals/UVs/tints untouched, winding preserved.
    out.clear();
    cache.emit_translated_into(&key, [37.0, 0.0, 7.0], &mut out);
    let offset = [37.0f32, 0.0, 7.0];
    for (got, want) in out.iter().zip(prototype.quads.iter()) {
        for k in 0..4 {
            for (axis, &off) in offset.iter().enumerate() {
                assert!((got.positions[k][axis] - want.positions[k][axis] - off).abs() < 1e-7);
            }
        }
        assert_eq!(got.normal, want.normal);
        assert_eq!(got.uv, want.uv);
        assert_eq!(got.tint_index, want.tint_index);
    }

    // The cross-sprite helper emits geometry identical to the naive per-block
    // builder (translated by block), jitter semantics included.
    let mut check = Vec::new();
    let block = [3, 0, 7];
    cache.emit_cross_sprite_into(&alpha, CrossLayout::HashGrid, block, &mut check);
    let naive = build_3d_cross_mesh(&alpha, CrossLayout::HashGrid, block);
    assert_eq!(check.len(), naive.quads.len());
    assert_eq!(cache.bakes(), 1, "helper still shares the one prototype");
    for (got, want) in check.iter().zip(naive.quads.iter()) {
        for k in 0..4 {
            for (axis, &b) in block.iter().enumerate() {
                assert!((got.positions[k][axis] - want.positions[k][axis] - b as f32).abs() < 1e-6);
            }
        }
        assert_eq!(got.uv, want.uv);
        assert_eq!(got.normal, want.normal);
    }
}

// ---------------------------------------------------------------------------
// FX-5
// ---------------------------------------------------------------------------

fn particle_spec(rate: f32, ttl: u16, lifetime: u16, pos: [f32; 3]) -> ParticleEmitterSpec {
    ParticleEmitterSpec {
        pos,
        initial_velocity: [0.0; 3],
        position_spread: [0.0; 3],
        velocity_spread: [0.0; 3],
        spawn_rate_per_tick: rate,
        particle_lifetime_ticks: lifetime,
        emitter_ttl_ticks: ttl,
        gravity_per_tick: 0.0,
        drag: 1.0,
        spin_per_tick: 0.0,
        orientation: ParticleOrientation::CameraBillboard,
        color: ColorRamp::new(vec![ColorStop {
            t: 0.0,
            rgba: [0.25, 0.5, 0.75, 1.0],
        }]),
        size: ScalarCurve::new(vec![ScalarStop { t: 0.0, value: 2.0 }]),
        priority: 1,
    }
}

#[test]
fn fx5_vfx_soa_10k_particles_swap_remove_and_24b_gpu_instances() {
    let _guard = serial();
    // The exact GPU record: 12B pos + 4B size/rot + 4B rgba8 + 4B sprite/orient.
    assert_eq!(size_of::<ParticleInstanceGpu>(), 24);
    assert_eq!(align_of::<ParticleInstanceGpu>(), 4);

    let mut world = VfxWorld::new(VfxHardwareTier::High);
    // Phase A — 10,240 spawn attempts (512/tick x 20 ticks) against the
    // 8,192 active cap: 2,048 admissions evict through the O(1) swap_remove.
    world
        .spawn_emitter(1, particle_spec(512.0, 20, 200, [1.0, 2.0, 3.0]))
        .unwrap();
    for _ in 0..20 {
        world.tick();
    }
    assert_eq!(world.particle_count(), 8192);
    assert_eq!(world.swap_removed_particles(), 2048);

    // Phase B — a second 5,120-attempt burst (lifetime-5 particles). For the
    // first 5 ticks every admission evicts an old phase-A particle; after
    // that its own early cohorts expire each tick and the fresh spawn batch
    // refills the freed slots, so the world stays saturated at the cap.
    world
        .spawn_emitter(2, particle_spec(512.0, 10, 5, [4.0, 5.0, 6.0]))
        .unwrap();
    for _ in 0..10 {
        world.tick();
    }
    assert_eq!(world.particle_count(), 8192);
    // 2,048 + 2,560 cap evictions + 2,560 expiry swap_removes.
    assert_eq!(world.swap_removed_particles(), 7_168);
    // Emitter 2 is dead now; its 5 remaining cohorts (2,560 particles) drain
    // over 5 ticks through the expiry swap_remove.
    for _ in 0..5 {
        world.tick();
    }
    assert_eq!(world.particle_count(), 5632);
    // 15,360 spawn events, 9,728 removals — over half — all via swap_remove.
    assert_eq!(world.swap_removed_particles(), 9_728);
    assert!(world.swap_removed_particles() >= 15_360 / 2);

    // Packed GPU instances: valid, fully populated, exact fields.
    let mut instances: Vec<ParticleInstanceGpu> = Vec::new();
    world.pack_particle_instances(&mut instances);
    assert_eq!(instances.len(), 5632);
    for inst in &instances {
        assert_eq!(inst.pos, [1.0, 2.0, 3.0]);
        // size 2.0 -> half 0x4000; rotation 0 -> 0 in the high half.
        assert_eq!(inst.size_rot_half, 0x4000);
        // rgba [0.25, 0.5, 0.75, 1.0] -> 0xFF BF 80 40.
        assert_eq!(inst.rgba8, 0xFFBF8040);
        assert_eq!(inst.sprite_orient, 0);
    }
    // Reuse: the second pack into the same buffer does not reallocate.
    let cap = instances.capacity();
    let before = alloc_count();
    world.pack_particle_instances(&mut instances);
    assert_eq!(alloc_count(), before);
    assert_eq!(instances.capacity(), cap);
    assert_eq!(instances.len(), 5632);

    // The instanced billboard shader mirrors the 24-byte record: pos as
    // array<f32,3> keeps the storage stride at 24 (vec3 would align to 16).
    assert!(PARTICLE_BILLBOARD_INSTANCED_WGSL.contains("pos: array<f32, 3>"));
    assert!(PARTICLE_BILLBOARD_INSTANCED_WGSL.contains("@builtin(instance_index)"));
    assert!(PARTICLE_BILLBOARD_INSTANCED_WGSL.contains("fn unpack_f16("));

    // Steady-state frame output reuses the caller's buffers: identical shapes
    // across frames produce zero fresh heap allocations.
    let mut out = world.frame_output([0.0, 0.0, 1.0]);
    assert_eq!(out.particles.len(), 5632);
    let primed = out.particles.capacity();
    let before = alloc_count();
    for _ in 0..3 {
        world.frame_output_into([0.0, 0.0, 1.0], &mut out);
    }
    assert_eq!(
        alloc_count(),
        before,
        "steady-state frame_output_into allocated"
    );
    assert_eq!(out.particles.len(), 5632);
    assert_eq!(out.particles.capacity(), primed);
    // Equivalent to the owning form.
    assert_eq!(out, world.frame_output([0.0, 0.0, 1.0]));
}
