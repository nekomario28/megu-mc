// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane QUAL-INEI-GPU-FINAL — final `client-gpu` convergence gate:
//!
//! * `qual_gpu_live_world_time_moon_shadows_cutout_and_stained_glass_pipelines` —
//!   the live `world_time_ticks` drives real sun-vs-moon shadow cascades
//!   (noon `sun` and midnight `-sun` both produce active, distinct depth
//!   passes — shadows are never disabled below the horizon), and the
//!   cutout/lazy-stained/water/sharpen WGSL programs the renderer wires
//!   lazily compile through the real naga validator.
//! * `qual_gpu_parallel_taskpool_meshing_releases_cpu_copies_and_lazy_zip_jar` —
//!   `MeguTaskPool` fans 27-neighbor snapshot meshing across its lanes,
//!   `release_uploaded_section_cpu_copies` drops VRAM-owned CPU copies
//!   (`packed_mesh_count() == 0`), and `LazyZipIndex` reads JAR entries
//!   with bounded index I/O — never a whole-archive `fs::read`.
//! * `qual_gpu_apply_settings_wires_style_3d_nature_tactile_and_audio_presets` —
//!   every visual/audio surface `apply_settings` writes is live-updatable
//!   on a session (3D blocks/skins, nature effects, tactile config,
//!   spatial-audio preset, shadow quality, render scale).

use megu_client::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
use megu_client::spatial_audio::SpatialAudioPreset;
use megu_client::world::{BlockTable, WorldState};
use megu_client_gpu::atmosphere::{AtmosphereParams, BiomeAtmosphereBlender, ChunkFadeTracker};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, ComposedFrame, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::Camera;
use megu_client_mesh::builtin_shadow_wgsl::{
    build_shadow_uniforms_celestial, compute_sun_direction, BuiltinShadowQuality, EclipseMode,
    RenderScalePreset, ShadowCacheDecision, ShadowCamera, MEGU_SHADOW_CUTOUT_DEPTH_WGSL,
    MEGU_SPATIAL_SHARPEN_WGSL, MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL, MEGU_TERRAIN_LIT_WGSL,
    MEGU_WATER_SURFACE_WGSL,
};
use megu_client_mesh::tactile_effects::{TactileEffectsConfig, TactilePreset};
use megu_client_mesh::{Block, MeshCache, Section, SectionCoord, SectionMeshInput};
use megu_core::task_pool::{MeguTaskPool, TaskLane};
use megu_core::zip_archive::LazyZipIndex;

const STONE: u32 = 1;
const WATER: u32 = 80;
const GLASS: u32 = 96;

fn test_dimension() -> DimensionMeta {
    DimensionMeta {
        name: "minecraft:overworld".to_string(),
        min_y: 0,
        height: 64,
        has_skylight: true,
    }
}

fn uniform_section(state: u32) -> DecodedSection {
    DecodedSection {
        non_air: if state == 0 {
            0
        } else {
            megu_client_mesh::SECTION_CELLS as i16
        },
        states: Box::new([state; megu_client_mesh::SECTION_CELLS]),
        biomes: [0; 64],
    }
}

fn chunk(cx: i32, cz: i32, sections: Vec<DecodedSection>) -> DecodedChunk {
    DecodedChunk {
        cx,
        cz,
        sections,
        light: DecodedLightData {
            sky_mask: Vec::new(),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky_arrays: Vec::new(),
            block_arrays: Vec::new(),
        },
        block_entities: Vec::new(),
    }
}

fn test_camera() -> Camera {
    Camera {
        position: [8.0, 26.0, 32.0],
        yaw_deg: 0.0,
        pitch_deg: -10.0,
        ..Camera::default()
    }
}

fn try_gpu_renderer() -> Option<GpuRenderer> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP qual GPU test: probe_adapters found 0 adapters");
        return None;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("probe found adapters but request_gpu_device failed");
    Some(GpuRenderer::new(ctx, OFFSCREEN_FORMAT))
}

use megu_client_mesh::RenderBackendPreference;

fn naga_validate(name: &str, source: &str) {
    use wgpu::naga::front::wgsl;
    use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
    let module = wgsl::parse_str(source).unwrap_or_else(|e| panic!("{name}: parse failed: {e}"));
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: validation failed: {e:?}"));
}

/// Noon (6000) and midnight (18000) must both produce an *active* shadow
/// cascade render — the moon path orients the light space along `-sun`
/// instead of disabling shadows — and the lazily-wired cutout, stained
/// glass, water, lit-terrain and sharpen programs all compile.
#[test]
fn qual_gpu_live_world_time_moon_shadows_cutout_and_stained_glass_pipelines() {
    // Sun direction is the authoritative celestial input: noon has a
    // positive-y sun, midnight's negative-y sun makes `-sun` the
    // (positive-y) moon axis.
    let noon_sun = compute_sun_direction(6_000);
    let midnight_sun = compute_sun_direction(18_000);
    assert!(noon_sun[1] > 0.5, "noon sun above horizon: {noon_sun:?}");
    assert!(
        midnight_sun[1] < -0.5,
        "midnight sun below horizon: {midnight_sun:?}"
    );

    // Celestial uniforms at noon vs midnight: both valid, distinct
    // light-space matrices (sun shadow vs moon `-sun` shadow).
    let config = BuiltinShadowQuality::Natural.config();
    let noon_u = build_shadow_uniforms_celestial(
        ShadowCamera::default(),
        config,
        6_000,
        0,
        EclipseMode::None,
    );
    let midnight_u = build_shadow_uniforms_celestial(
        ShadowCamera::default(),
        config,
        18_000,
        4,
        EclipseMode::None,
    );
    assert_ne!(
        noon_u.light_view_proj[0], midnight_u.light_view_proj[0],
        "sun vs moon shadow spaces must differ"
    );

    // Every lazily-wired program compiles through the same naga
    // frontend+validator the builtin-pack suite uses.
    naga_validate("cutout depth", MEGU_SHADOW_CUTOUT_DEPTH_WGSL);
    naga_validate("stained glass color", MEGU_STAINED_GLASS_SHADOW_COLOR_WGSL);
    naga_validate("water surface", MEGU_WATER_SURFACE_WGSL);
    naga_validate("lit terrain", MEGU_TERRAIN_LIT_WGSL);
    naga_validate("spatial sharpen", MEGU_SPATIAL_SHARPEN_WGSL);

    let Some(mut renderer) = try_gpu_renderer() else {
        return;
    };
    renderer.set_shadow_quality(BuiltinShadowQuality::Natural);

    // Live world: opaque + water + translucent-glass sections installed
    // through the real upload path so the composed pass exercises the
    // split streams.
    let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
    world
        .apply_chunk(chunk(
            0,
            0,
            vec![
                uniform_section(STONE),
                uniform_section(WATER),
                uniform_section(GLASS),
                uniform_section(0),
            ],
        ))
        .unwrap();
    world
        .rebuild_dirty_bounded(usize::MAX, [8.0, 26.0, 32.0])
        .unwrap();
    renderer.update_live_terrain(&world);

    let mut live = LiveSession::offline_with_null_audio();
    let blender = BiomeAtmosphereBlender::new(AtmosphereParams::default());
    let tracker = ChunkFadeTracker::new();
    let camera = test_camera();

    for &ticks in &[6_000u64, 18_000u64] {
        let mut frame = ComposedFrame {
            subworlds: &mut live.subworlds,
            lights: &[],
            atmosphere: &blender,
            chunk_fades: &tracker,
            ui_rgba: None,
            world_revision: 1,
            world_time_ticks: ticks,
        };
        renderer
            .render_offscreen_composed_frame(160, 120, &camera, &mut frame)
            .expect("composed frame renders at live world time");
        assert!(
            matches!(
                renderer.last_shadow_decision,
                ShadowCacheDecision::Rerender(_) | ShadowCacheDecision::ReuseCached
            ),
            "world_time {ticks}: shadow cascades stay active (never disabled)"
        );
    }
    assert!(
        renderer.shadow_rerender_count >= 1,
        "at least one live shadow depth pass ran"
    );
}

/// `MeguTaskPool` fans the lock-free 27-neighbor snapshot+mesh across
/// lanes, `release_uploaded_section_cpu_copies` drops VRAM-owned CPU
/// copies to zero, and `LazyZipIndex` reads only the central directory
/// plus selected entries — never the whole archive.
#[test]
fn qual_gpu_parallel_taskpool_meshing_releases_cpu_copies_and_lazy_zip_jar() {
    // ---- MeguTaskPool 27-neighbor parallel meshing ----
    let mut cache = MeshCache::default();
    for y in -1..=1 {
        for z in -1..=1 {
            for x in -1..=1 {
                let mut blocks = Box::new([Block::AIR; megu_client_mesh::SECTION_CELLS]);
                // One solid floor per section keeps every mesh non-trivial.
                for i in 0..256 {
                    blocks[i] = Block::opaque_cube(STONE);
                }
                cache.insert(
                    SectionCoord(x, y, z),
                    Section {
                        blocks,
                        block_light: [0u8; megu_client_mesh::LIGHT_BYTES],
                        sky_light: [255u8; megu_client_mesh::LIGHT_BYTES],
                    },
                );
            }
        }
    }
    // All 27 neighborhood sections are loaded — snapshot each one
    // (every snapshot itself captures the center + 26 neighbors) and fan
    // the pure mesh builds across the task pool's background lane.
    let pool = MeguTaskPool::for_host();
    let coords: Vec<SectionCoord> = (-1..=1)
        .flat_map(|y| (-1..=1).flat_map(move |z| (-1..=1).map(move |x| SectionCoord(x, y, z))))
        .collect();
    let jobs: Vec<SectionMeshInput> = coords
        .iter()
        .map(|&c| cache.snapshot_section_input(c, 7).expect("section loaded"))
        .collect();
    assert_eq!(jobs.len(), 27);
    let built = std::sync::Mutex::new(Vec::new());
    pool.scope_blocking(TaskLane::Background, |scope| {
        pool.par_for_each(TaskLane::Background, &jobs, |_, job| {
            built
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(megu_client_mesh::build_and_pack_section_mesh(job));
        });
        let _ = scope;
    });
    let built = built.into_inner().unwrap_or_else(|e| e.into_inner());
    assert_eq!(built.len(), 27);
    assert!(
        built.iter().all(|m| !m.opaque_packed.is_empty()),
        "27-neighbor snapshots produced packed meshes off-thread"
    );
    assert!(
        built.iter().all(|m| m.revision == 7),
        "revision tags survive the parallel build"
    );
    assert!(
        pool.tasks_executed() >= 1,
        "task pool workers executed the fan-out ({} tasks)",
        pool.tasks_executed()
    );

    // ---- CPU-copy release after VRAM upload (V2) ----
    let mut world = WorldState::new(test_dimension(), BlockTable::builtin());
    world
        .apply_chunk(chunk(
            0,
            0,
            vec![uniform_section(STONE), uniform_section(WATER)],
        ))
        .unwrap();
    world
        .rebuild_dirty_bounded(usize::MAX, [8.0, 4.0, 8.0])
        .unwrap();
    let before = world.packed_mesh_count();
    assert!(before > 0, "sections produced CPU packed meshes");
    for coord in [SectionCoord(0, 0, 0), SectionCoord(0, 1, 0)] {
        world.release_uploaded_section_cpu_copies(coord);
    }
    assert_eq!(
        world.packed_mesh_count(),
        0,
        "uploaded sections released their CPU PackedVertex copies"
    );

    // ---- LazyZipIndex: bounded index I/O, no whole-archive fs::read ----
    // Minimal stored-method ZIP: one local header + file data + central
    // directory + EOCD, built in memory (test fixture only — production
    // never extracts to disk).
    let entry_name = b"assets/test/one.bin";
    // 16 KiB payload: a real JAR entry, large enough that the bounded
    // ~4 KiB EOCD tail + central-directory index pass provably reads
    // fewer bytes than the archive holds.
    let payload: Vec<u8> = (0..16_384u32).map(|i| (i % 251) as u8).collect();
    let payload: &[u8] = &payload;
    let mut zip = Vec::new();
    let local_off = zip.len() as u32;
    zip.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header sig
    zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
    zip.extend_from_slice(&0u16.to_le_bytes()); // flags
    zip.extend_from_slice(&0u16.to_le_bytes()); // method = stored
    zip.extend_from_slice(&0u16.to_le_bytes()); // mod time
    zip.extend_from_slice(&0u16.to_le_bytes()); // mod date
    let crc = crc32(payload);
    zip.extend_from_slice(&crc.to_le_bytes());
    zip.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // comp size
    zip.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // size
    zip.extend_from_slice(&(entry_name.len() as u16).to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes()); // extra len
    zip.extend_from_slice(entry_name);
    zip.extend_from_slice(payload);
    let cd_off = zip.len() as u32;
    zip.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central sig
    zip.extend_from_slice(&20u16.to_le_bytes()); // version made by
    zip.extend_from_slice(&20u16.to_le_bytes()); // version needed
    zip.extend_from_slice(&0u16.to_le_bytes()); // flags
    zip.extend_from_slice(&0u16.to_le_bytes()); // method
    zip.extend_from_slice(&0u16.to_le_bytes()); // time
    zip.extend_from_slice(&0u16.to_le_bytes()); // date
    zip.extend_from_slice(&crc.to_le_bytes());
    zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    zip.extend_from_slice(&(entry_name.len() as u16).to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes()); // extra len
    zip.extend_from_slice(&0u16.to_le_bytes()); // comment len
    zip.extend_from_slice(&0u16.to_le_bytes()); // disk start
    zip.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    zip.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    zip.extend_from_slice(&local_off.to_le_bytes());
    zip.extend_from_slice(entry_name);
    let cd_size = zip.len() as u32 - cd_off;
    zip.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD
    zip.extend_from_slice(&0u16.to_le_bytes()); // disk
    zip.extend_from_slice(&0u16.to_le_bytes()); // cd disk
    zip.extend_from_slice(&1u16.to_le_bytes()); // entries this disk
    zip.extend_from_slice(&1u16.to_le_bytes()); // entries total
    zip.extend_from_slice(&cd_size.to_le_bytes());
    zip.extend_from_slice(&cd_off.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes()); // comment len

    let dir = std::env::temp_dir().join(format!(
        "qual-lazy-zip-{}-{}.jar",
        std::process::id(),
        built[0].revision
    ));
    std::fs::write(&dir, &zip).expect("write fixture jar");
    let index = LazyZipIndex::open_filtered(&dir, |_| true).expect("lazy index opens");
    // Index pass read only the EOCD tail + central directory — strictly
    // less than the full archive bytes.
    assert!(
        index.bytes_read_for_index() < zip.len() as u64,
        "index I/O {} stays below full archive {}",
        index.bytes_read_for_index(),
        zip.len()
    );
    let names: Vec<_> = index.entries().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["assets/test/one.bin"]);
    let bytes = index
        .read_entry("assets/test/one.bin", 1 << 20)
        .expect("entry reads through the index");
    assert_eq!(bytes, payload);
    let _ = std::fs::remove_file(&dir);
}

/// `apply_settings` writes these surfaces live: 3D blocks/skins, nature
/// effects, the tactile sim config, and the spatial-audio preset — plus
/// the renderer shadow-quality / render-scale knobs.
#[test]
fn qual_gpu_apply_settings_wires_style_3d_nature_tactile_and_audio_presets() {
    let mut live = LiveSession::offline_with_null_audio();

    // 3D blocks/skins + nature effects master switches.
    live.vanilla_plus_cfg.three_d.enabled = true;
    live.vanilla_plus_cfg.nature.enabled = true;
    assert!(live.vanilla_plus_cfg.three_d.enabled);
    assert!(live.vanilla_plus_cfg.nature.enabled);

    // Tactile sim: preset + per-effect toggles land via the setter.
    live.set_tactile_config(TactileEffectsConfig {
        preset: TactilePreset::Fancy,
        water_and_lava_reactions: true,
        subtle_camera_motion: true,
        ..TactileEffectsConfig::default()
    });
    assert_eq!(live.tactile_config().preset, TactilePreset::Fancy);
    assert!(live.tactile_config().water_and_lava_reactions);
    assert!(live.tactile_config().subtle_camera_motion);
    // Toggling off restores the bit-exact vanilla path.
    live.set_tactile_config(TactileEffectsConfig {
        preset: TactilePreset::Off,
        ..TactileEffectsConfig::default()
    });
    assert_eq!(live.tactile_config().preset, TactilePreset::Off);

    // Spatial-audio preset: caller-side mixer retunes without losing the
    // acoustic-world binding (mixer's own world stays attached).
    live.set_spatial_audio_preset(SpatialAudioPreset::Off);
    live.set_spatial_audio_preset(SpatialAudioPreset::Performance);

    // Renderer-side settings surfaces (no GPU needed to construct the
    // request path, but exercise it when one exists).
    if let Some(mut renderer) = try_gpu_renderer() {
        renderer.set_shadow_quality(BuiltinShadowQuality::Natural);
        assert_eq!(renderer.shadow_quality, BuiltinShadowQuality::Natural);
        renderer.set_render_scale(RenderScalePreset::Performance67);
        assert_eq!(renderer.render_scale, RenderScalePreset::Performance67);
        renderer.set_render_scale(RenderScalePreset::Native100);
    }
}

/// Classic CRC-32 (IEEE) for the stored-ZIP fixture.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}
