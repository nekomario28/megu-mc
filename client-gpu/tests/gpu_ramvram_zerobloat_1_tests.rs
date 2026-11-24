// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane GPU-RAMVRAM-ZEROBLOAT-1 proofs:
//!
//! 1. V1+V3: `PendingAtlasPixels` CPU bytes are dropped the moment
//!    `upload_block_atlas` succeeds (metadata stays usable) and a UMA
//!    `UnifiedZeroCopy` topology keeps `Native100` render scale + the
//!    negotiated shadow preset while `TransferTopologyBudget` switches to
//!    the UMA transfer sizing.
//! 2. V5 + shared index: a bit-17 diagonal-flip quad rotated by
//!    `rotate_opaque_quads_for_shared_index` draws the identical
//!    `(v1,v3)`-diagonal triangles through the canonical
//!    `[0,1,2,0,2,3]` pattern, and the persistent `UploadStagingRing`
//!    serves 20 consecutive uploads with zero extra `create_buffer` calls.
//! 3. V4: `HIZ_BUILD_WGSL`/`SECTION_CULL_WGSL` validate through real Naga
//!    and execute on a real GPU adapter — the occluded section behind a
//!    solid wall is culled (`instance_count == 0`) while every
//!    visible/front section keeps `instance_count == 1` (zero false
//!    negatives).

use std::collections::BTreeMap;

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client_gpu::gpu_hiz::{
    CullStats, CullUniforms, GpuHizCuller, IndirectArgs, SectionGpuRecord, HIZ_BUILD_WGSL,
    HIZ_DOWNSAMPLE_WGSL, SECTION_CULL_WGSL,
};
use megu_client_gpu::live_session::hw_adaptive_engine::{
    MemoryFootprintClass, MemoryTransferStrategy,
};
use megu_client_gpu::live_session::{CompiledAssetMeta, LiveSession, PendingAtlasPixels};
use megu_client_gpu::renderer::{
    frustum_planes_from_view_proj, probe_adapters, request_gpu_device,
    rotate_opaque_quads_for_shared_index, GpuRenderer, TransferTopologyBudget, UploadStagingRing,
    OFFSCREEN_FORMAT, QUAD_DIAGONAL_FLIP_BIT,
};
use megu_client_gpu::Camera;
use megu_client_mesh::atlas::{AtlasMip, PackedAtlas, SpritePlacement};
use megu_client_mesh::builtin_shadow_wgsl::{BuiltinShadowQuality, RenderScalePreset};
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::{RenderBackendPreference, SectionCoord};
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};

/// A headless renderer, or `None` when the host has no GPU adapter (the
/// same graceful skip the live-session GPU tests use).
fn try_gpu() -> Option<megu_client_gpu::renderer::GpuDevice> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP gpu-ramvram-zerobloat test: probe_adapters found 0 adapters");
        return None;
    }
    Some(
        request_gpu_device(RenderBackendPreference::Auto, None, None)
            .expect("probe found adapters but request_gpu_device failed"),
    )
}

fn varint(bytes: &mut Vec<u8>, mut value: u32) {
    loop {
        let part = (value & 0x7f) as u8;
        value >>= 7;
        bytes.push(part | if value == 0 { 0 } else { 0x80 });
        if value == 0 {
            break;
        }
    }
}

/// Protocol 767 `level_chunk_with_light` body, dimension -64..320 (24
/// sections). Section index 4 covers world y 0..16 — a single solid-stone
/// section surrounded by air, matching the live-session test fixture.
fn chunk_body(cx: i32, cz: i32, stone_section: i32) -> Vec<u8> {
    let mut sections = Vec::new();
    for section in 0..24 {
        let filled = section == stone_section;
        sections.extend_from_slice(&(if filled { 4096i16 } else { 0 }).to_be_bytes());
        sections.push(0); // single-valued block palette
        varint(&mut sections, if filled { 1 } else { 0 });
        varint(&mut sections, 0); // no packed block values
        sections.push(0); // single-valued biome palette
        varint(&mut sections, 0);
        varint(&mut sections, 0);
    }
    let mut body = Vec::new();
    body.extend_from_slice(&cx.to_be_bytes());
    body.extend_from_slice(&cz.to_be_bytes());
    body.extend_from_slice(&[10, 0]); // empty unnamed heightmap compound
    varint(&mut body, sections.len() as u32);
    body.extend_from_slice(&sections);
    for _ in 0..7 {
        varint(&mut body, 0); // block entities, light masks and arrays
    }
    body
}

/// Ingest one stone-section chunk per `(cx, cz)` and rebuild every pending
/// mesh so `world.packed_meshes()`/`section_graphs()` are populated.
fn session_with_stone_chunks(chunks: &[(i32, i32)]) -> LiveSession {
    let mut live = LiveSession::offline_with_null_audio();
    live.apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
        x: 8.0,
        y: 20.0,
        z: 24.0,
        yaw: 0.0,
        pitch: -20.0,
        flags: 0,
        teleport_id: 1,
    })))
    .unwrap();
    for &(cx, cz) in chunks {
        let chunk =
            decode_chunk_packet(&chunk_body(cx, cz, 4), 24, 64).expect("decoded real chunk");
        live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
            .unwrap();
    }
    for _ in 0..16 {
        if live.world.pending_mesh_sections() == 0 {
            break;
        }
        live.world
            .rebuild_dirty_bounded(256, [8.0, 20.0, 24.0])
            .expect("mesh rebuild");
    }
    assert_eq!(live.world.pending_mesh_sections(), 0);
    live
}

/// A 16x16 single-sprite atlas payload with the full mip chain
/// `upload_block_atlas` validates.
fn pending_atlas() -> PendingAtlasPixels {
    let (w, h) = (16u32, 16u32);
    let mut placements = BTreeMap::new();
    placements.insert(
        "minecraft:block/stone".to_string(),
        SpritePlacement {
            x: 0,
            y: 0,
            width: w,
            height: h,
        },
    );
    let atlas = PackedAtlas {
        width: w,
        height: h,
        padding: 0,
        rgba: vec![0xAB; (w * h * 4) as usize],
        placements,
    };
    let mut mipmaps = Vec::new();
    let (mut mw, mut mh, mut level) = (w, h, 1u32);
    while mw > 1 || mh > 1 {
        mw = (mw / 2).max(1);
        mh = (mh / 2).max(1);
        mipmaps.push(AtlasMip {
            level,
            width: mw,
            height: mh,
            rgba: vec![0xCD; (mw * mh * 4) as usize],
        });
        level += 1;
    }
    PendingAtlasPixels { atlas, mipmaps }
}

// ---------------------------------------------------------------------------
// Test 1: V1 atlas pixel drop + V3 UMA keeps user quality
// ---------------------------------------------------------------------------

#[test]
fn v1_v3_atlas_pixels_dropped_after_upload_and_uma_keeps_native100_quality() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    let mut live = LiveSession::offline_with_null_audio();

    // Stage a pending upload exactly the way `ensure_asset_bundle` does:
    // metadata retained, pixel payloads held only until upload.
    live.atlas_meta = Some(CompiledAssetMeta {
        atlas_width: 16,
        atlas_height: 16,
        atlas_padding: 0,
        ..CompiledAssetMeta::default()
    });
    live.pending_atlas_pixels = Some(pending_atlas());

    assert!(
        live.upload_pending_atlas(&mut renderer),
        "upload_pending_atlas reports the pending pixels consumed"
    );
    // V1: zero CPU atlas RGBA/mipmap bytes remain resident after upload…
    assert!(live.pending_atlas_pixels.is_none());
    // …while the metadata half stays usable for mesh UV remapping.
    let meta = live.atlas_meta.as_ref().expect("atlas metadata retained");
    assert_eq!((meta.atlas_width, meta.atlas_height), (16, 16));

    // V3: force the UMA topology and a deterministic (Standard) RAM class,
    // then apply the connect-time strategy. Visual settings must survive
    // untouched — only the transfer budget tracks the topology.
    let chosen_shadows = renderer.set_shadow_quality(BuiltinShadowQuality::Medium);
    renderer.set_memory_footprint_class(MemoryFootprintClass::Standard);
    live.compute_strategy.memory = MemoryTransferStrategy::UnifiedZeroCopy;
    let negotiated = live.apply_compute_strategy(&mut renderer);

    assert_eq!(
        renderer.render_scale,
        RenderScalePreset::Native100,
        "UMA must keep the Native100 render scale (no forced Balanced75)"
    );
    assert_eq!(
        renderer.shadow_quality, chosen_shadows,
        "UMA must keep the user-negotiated shadow preset (no forced Low)"
    );
    assert_eq!(negotiated, chosen_shadows);

    // The UMA transfer row (Standard class): 3x2 MiB staging ring,
    // 8 MiB arena slabs, 60-frame idle grace, 3 MiB/frame DMA cap.
    let budget = renderer.transfer_topology_budget();
    assert_eq!(budget.staging_ring_slots, 3);
    assert_eq!(budget.staging_slot_bytes, 2 * 1024 * 1024);
    assert_eq!(budget.arena_slab_bytes, 8 * 1024 * 1024);
    assert_eq!(budget.idle_slab_reclaim_frames, 60);
    assert_eq!(budget.frame_dma_cap_bytes, 3 * 1024 * 1024);
}

// ---------------------------------------------------------------------------
// Test 2: shared quad index rotation parity + staging ring zero allocs
// ---------------------------------------------------------------------------

#[test]
fn v5_shared_quad_index_rotation_parity_and_staging_ring_zero_steady_allocs() {
    // A flip-marked quad: distinct vertex tags 0..3 in `position` so the
    // rotated order can be verified verbatim.
    let tag = |i: u32, attributes: u32| PackedVertex {
        position: i,
        state_id: 0,
        attributes,
        tint: 0,
    };
    let mut quad = [
        tag(0, QUAD_DIAGONAL_FLIP_BIT),
        tag(1, 0),
        tag(2, 0),
        tag(3, 0),
    ];
    rotate_opaque_quads_for_shared_index(&mut quad);
    assert_eq!(
        quad.iter().map(|v| v.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 0],
        "bit-17 quad must rotate [v0,v1,v2,v3] -> [v1,v2,v3,v0]"
    );
    // Triangle parity: the canonical pattern on the rotated array draws
    // (v1,v2,v3)+(v1,v3,v0) — the same two triangles as the original
    // alternate pattern's (v0,v1,v3)+(v1,v2,v3), sharing the (v1,v3)
    // diagonal. An unmarked quad must be left untouched.
    let mut plain = [tag(10, 0), tag(11, 0), tag(12, 0), tag(13, 0)];
    rotate_opaque_quads_for_shared_index(&mut plain);
    assert_eq!(
        plain.iter().map(|v| v.position).collect::<Vec<_>>(),
        vec![10, 11, 12, 13]
    );

    // UploadStagingRing: 20 consecutive staged copies must not grow the
    // ring — `staging_ring_allocs` stays at the construction count.
    let Some(ctx) = try_gpu() else {
        return;
    };
    let budget = TransferTopologyBudget::for_topology(
        MemoryTransferStrategy::DiscreteStagedArena,
        MemoryFootprintClass::Standard,
    );
    let mut ring = UploadStagingRing::new(&ctx.device, &budget);
    let construction_allocs = ring.staging_ring_allocs;
    assert_eq!(construction_allocs, u64::from(budget.staging_ring_slots));

    let dst = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ring-test-dst"),
        size: 64 * 1024,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let payload = vec![0x5Au8; 4096];
    for i in 0..20 {
        // Each iteration's encoder is its own submission batch: the previous
        // batch's copies submitted last pass, so the slots are reusable.
        ring.begin_batch();
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ring-test"),
            });
        ring.stage_copy(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            &dst,
            (i % 8) * 4096,
            &payload,
        );
        ctx.queue.submit([encoder.finish()]);
    }
    ctx.device.poll(wgpu::PollType::wait_indefinitely()).ok();
    assert_eq!(ring.staging_ring_uploads, 20);
    assert_eq!(
        ring.staging_ring_allocs, construction_allocs,
        "steady-state uploads must issue zero extra create_buffer calls"
    );
    assert_eq!(ring.staging_ring_fallbacks, 0);
}

// ---------------------------------------------------------------------------
// Test 3: real-GPU Hi-Z build + section cull, zero false negatives
// ---------------------------------------------------------------------------

fn validate_wgsl(name: &str, source: &str) {
    let module = wgsl::parse_str(source).unwrap_or_else(|err| {
        panic!("{name}: naga WGSL parse failed:\n{err}");
    });
    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
    validator.validate(&module).unwrap_or_else(|err| {
        panic!("{name}: naga validation failed:\n{err}");
    });
}

/// Copy `size` bytes of `src` to a map-read buffer and return the bytes.
fn read_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    src: &wgpu::Buffer,
    size: u64,
) -> Vec<u8> {
    let dst = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-readback"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("test-readback"),
    });
    encoder.copy_buffer_to_buffer(src, 0, &dst, 0, size);
    queue.submit([encoder.finish()]);
    let slice = dst.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let bytes = slice.get_mapped_range().unwrap().to_vec();
    dst.unmap();
    bytes
}

/// Read one pyramid mip level through a row-padded copy buffer.
fn read_mip(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    level: u32,
    width: u32,
    height: u32,
) -> Vec<f32> {
    let padded_row = (width * 4).div_ceil(256) * 256;
    let dst = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-mip-readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("test-mip-readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &dst,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let slice = dst.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let raw = slice.get_mapped_range().unwrap().to_vec();
    dst.unmap();
    let mut out = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        let start = (row * padded_row) as usize;
        let floats: &[f32] = bytemuck::cast_slice(&raw[start..start + (width * 4) as usize]);
        out.extend_from_slice(floats);
    }
    out
}

#[test]
fn v4_gpu_compute_hiz_and_section_cull_execute_on_real_gpu_zero_false_negatives() {
    // Naga must accept all three compute modules before any GPU runs them.
    validate_wgsl("HIZ_BUILD_WGSL", HIZ_BUILD_WGSL);
    validate_wgsl("HIZ_DOWNSAMPLE_WGSL", HIZ_DOWNSAMPLE_WGSL);
    validate_wgsl("SECTION_CULL_WGSL", SECTION_CULL_WGSL);

    let Some(ctx) = try_gpu() else {
        return;
    };
    let (device, queue) = (&ctx.device, &ctx.queue);
    eprintln!("adapter: {}", ctx.report.name);

    // 64x64 synthetic depth: left half (u < 0.5) is the wall depth, right
    // half is clear (far = 1.0).
    const W: u32 = 64;
    const H: u32 = 64;
    const WALL: f32 = 0.9;
    let mut depth_px = vec![0f32; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            depth_px[(y * W + x) as usize] = if x < W / 2 { WALL } else { 1.0 };
        }
    }
    let src = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test-depth-src"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &src,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&depth_px),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(W * 4),
            rows_per_image: Some(H),
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );

    let mut culler = GpuHizCuller::new(device, W, H);
    let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("hiz-build"),
    });
    culler.build_from_r32(device, &mut encoder, &src_view);
    queue.submit([encoder.finish()]);
    assert_eq!(
        culler.hiz_build_dispatches,
        u64::from(culler.mip_count()),
        "mip0 + one downsample dispatch per level"
    );

    // Pyramid sanity on the real GPU: mip 0 reproduces the source, mid
    // levels keep the wall in the left half, the last level is the global
    // max (1.0).
    let mip0 = read_mip(device, queue, culler.pyramid_texture(), 0, W, H);
    assert_eq!(mip0[0], WALL);
    assert_eq!(mip0[(W / 2) as usize], 1.0);
    let mid = read_mip(device, queue, culler.pyramid_texture(), 3, W >> 3, H >> 3);
    assert_eq!(mid[0], WALL, "left half of mip3 must stay the wall depth");
    assert_eq!(mid[((H >> 3) * (W >> 3) - 1) as usize], 1.0);
    let last_level = culler.mip_count() - 1;
    let last = read_mip(device, queue, culler.pyramid_texture(), last_level, 1, 1);
    assert_eq!(last[0], 1.0, "coarsest level holds the global max depth");

    // Camera at x=20 looking -Z: the wall column (x 0..16) projects inside
    // the left half of the screen; x >= 16 columns project at/right of
    // centre into untouched 1.0-depth texels.
    let camera = Camera {
        position: [20.0, 8.0, 48.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        fov_deg: 90.0,
        roll_rad: 0.0,
        near: 0.1,
        far: 512.0,
    };
    let view_proj = camera.view_projection(1.0);
    let uniforms = CullUniforms {
        view_proj,
        planes: frustum_planes_from_view_proj(&view_proj),
        camera_pos: [20.0, 8.0, 48.0, 0.0],
        params: [W as f32, H as f32, culler.mip_count() as f32, 4.0],
    };
    let record = |min: [f32; 3], max: [f32; 3]| SectionGpuRecord {
        aabb_min: [min[0], min[1], min[2], 0.0],
        aabb_max: [max[0], max[1], max[2], 0.0],
        index_count: 6,
        first_index: 0,
        base_vertex: 0,
        first_instance: 0,
        _pad: [0; 4],
    };
    let sections = [
        // S0 front: beside the wall at the wall's depth — must stay
        // visible even where it overlaps wall-depth texels.
        record([16.0, 0.0, 24.0], [32.0, 16.0, 40.0]),
        // S1 occluded: directly behind the wall column — culled.
        record([0.0, 0.0, 0.0], [16.0, 16.0, 16.0]),
        // S2 far-visible: same depth as S1 but not behind the wall —
        // must stay visible (no false negatives).
        record([16.0, 0.0, 0.0], [32.0, 16.0, 16.0]),
        // S3 off-frustum: far left of the view — frustum culled.
        record([-64.0, 0.0, 24.0], [-48.0, 16.0, 40.0]),
    ];
    let sections_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cull-sections"),
        size: (sections.len() * std::mem::size_of::<SectionGpuRecord>()) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&sections_buf, 0, bytemuck::cast_slice(&sections));
    let uniforms_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cull-uniforms"),
        size: std::mem::size_of::<CullUniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&uniforms_buf, 0, bytemuck::bytes_of(&uniforms));
    let args_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cull-args"),
        size: (sections.len() * std::mem::size_of::<IndirectArgs>()) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let stats_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cull-stats"),
        size: std::mem::size_of::<CullStats>() as u64,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_SRC
            | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&stats_buf, 0, bytemuck::bytes_of(&CullStats::default()));
    let masks_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cull-masks"),
        size: (sections.len() * 4) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("section-cull"),
    });
    culler.encode_cull(
        device,
        &mut encoder,
        &uniforms_buf,
        &sections_buf,
        &args_buf,
        &stats_buf,
        &masks_buf,
        sections.len() as u32,
    );
    queue.submit([encoder.finish()]);
    assert_eq!(culler.cull_dispatches, 1);

    let args_bytes = read_buffer(
        device,
        queue,
        &args_buf,
        (sections.len() * std::mem::size_of::<IndirectArgs>()) as u64,
    );
    let args: &[IndirectArgs] = bytemuck::cast_slice(&args_bytes);
    let stats_bytes = read_buffer(
        device,
        queue,
        &stats_buf,
        std::mem::size_of::<CullStats>() as u64,
    );
    let stats: &CullStats = &bytemuck::cast_slice::<u8, CullStats>(&stats_bytes)[0];

    assert_eq!(args[0].instance_count, 1, "front section must survive");
    assert_eq!(args[0].index_count, 6);
    assert_eq!(
        args[1].instance_count, 0,
        "walled-off section must be culled"
    );
    assert_eq!(
        args[2].instance_count, 1,
        "far section outside the wall column must survive (no false negatives)"
    );
    assert_eq!(args[3].instance_count, 0, "off-frustum section culled");
    assert_eq!(stats.visible_count, 2);
    assert_eq!(stats.hiz_culled, 1);
    assert_eq!(stats.frustum_culled, 1);

    // Wired-path proof on the same real device: `GpuRenderer`'s own GPU
    // culler builds the pyramid from the rendered frame's depth buffer
    // and culls `live_sections`. Camera at [8,20,24] looks -Z over four
    // solid stone sections stacked along the axis; the section at
    // SectionCoord(0,0,2) sits entirely behind the camera. The GPU may
    // legitimately Hi-Z-cull the far pair (fully occluded by the nearest
    // solid section), but the nearest section (0,0,0) must always
    // survive — zero false negatives.
    let live = session_with_stone_chunks(&[(0, -2), (0, -1), (0, 0), (0, 2)]);
    let mut renderer = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    assert_eq!(renderer.update_live_terrain(&live.world), 4);
    let scene_camera = Camera {
        position: [8.0, 20.0, 24.0],
        yaw_deg: 0.0,
        pitch_deg: -20.0,
        ..Camera::default()
    };
    renderer
        .render_offscreen_rgba(64, 64, &scene_camera, None)
        .expect("offscreen frame feeds the depth buffer");
    assert_eq!(renderer.live_section_count(), 4);
    assert!(renderer.gpu_hiz_build_from_frame_depth());
    let scene_vp = scene_camera.view_projection(1.0);
    let gpu_culled = renderer.gpu_cull_live_sections(&scene_vp, scene_camera.position);
    assert_eq!(renderer.gpu_hiz_cull_calls, 1);
    assert!(
        gpu_culled.contains(&SectionCoord(0, 0, 2)),
        "behind-camera section must be GPU culled: {gpu_culled:?}"
    );
    assert!(
        !gpu_culled.contains(&SectionCoord(0, 0, 0)),
        "nearest visible section must never be culled: {gpu_culled:?}"
    );
    assert!(
        gpu_culled.len() <= 3,
        "at most the behind-camera + two occluded sections: {gpu_culled:?}"
    );
}
