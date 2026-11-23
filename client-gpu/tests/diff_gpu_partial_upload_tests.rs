// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane DIFF-GPU-PARTIAL-UPLOAD proof on the real adapter:
//!
//! `encode_gpu_indirect_cull` packs `SectionGpuRecord`s against a CPU
//! shadow of the GPU buffer and uploads only the coalesced dirty spans:
//! a camera-only frame uploads **zero** record bytes (zero
//! `write_buffer` calls), a freshly allocated/grown buffer uploads the
//! full pack once, and one arriving section uploads exactly its own
//! 64-byte record — never the whole `N * 64` pack.

use megu_client::protocol::{decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync};
use megu_client_gpu::gpu_hiz::{IndirectArgs, SectionGpuRecord};
use megu_client_gpu::live_session::LiveSession;
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, GpuDevice, GpuRenderer, OFFSCREEN_FORMAT,
};
use megu_client_gpu::Camera;
use megu_client_mesh::{RenderBackendPreference, SectionCoord};

// ---------------------------------------------------------------------------
// Shared fixtures (mirrors gpu_indirect_and_opt_wire_tests.rs)
// ---------------------------------------------------------------------------

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
/// section surrounded by air.
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

/// Ingest one stone-section chunk at `(cx, cz)` into an existing
/// session and rebuild the pending meshes immediately.
fn push_stone_chunk(live: &mut LiveSession, cx: i32, cz: i32) {
    let chunk = decode_chunk_packet(&chunk_body(cx, cz, 4), 24, 64).expect("decoded real chunk");
    live.apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(chunk))))
        .unwrap();
    for _ in 0..16 {
        if live.world.pending_mesh_sections() == 0 {
            break;
        }
        live.world
            .rebuild_dirty_bounded(256, [8.0, 20.0, 24.0])
            .expect("mesh rebuild");
    }
    assert_eq!(live.world.pending_mesh_sections(), 0);
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
        push_stone_chunk(&mut live, cx, cz);
    }
    live
}

/// A headless GPU device, or `None` when the host has no adapter (the
/// same graceful skip the other GPU tests use).
fn try_gpu() -> Option<GpuDevice> {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP diff-upload test: probe_adapters found 0 adapters");
        return None;
    }
    Some(
        request_gpu_device(RenderBackendPreference::Auto, None, None)
            .expect("probe found adapters but request_gpu_device failed"),
    )
}

/// Copy `size` bytes of `src` to a map-read buffer and return the bytes —
/// test verification only; the production indirect path never reads back.
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

// ---------------------------------------------------------------------------
// Differential SectionGpuRecord uploads on the real adapter
// ---------------------------------------------------------------------------

#[test]
fn rx7800_gpu_indirect_diff_uploads_zero_bytes_steady_and_partial_span_on_single_edit() {
    let Some(ctx) = try_gpu() else {
        return;
    };
    let info = ctx.adapter.get_info();
    eprintln!(
        "adapter: {} ({:?}) — INDIRECT_EXECUTION gate decides this test",
        info.name, info.backend
    );
    let mut indirect = GpuRenderer::new(ctx, OFFSCREEN_FORMAT);
    if !indirect.indirect_execution_supported() {
        eprintln!("SKIP: adapter lacks DownlevelFlags::INDIRECT_EXECUTION");
        return;
    }
    indirect.set_gpu_indirect_enabled(true);

    // Scene: four opaque arena sections around the camera — a wall pair
    // ahead (-z), one beside, one underfoot-adjacent. Every one commits
    // to a region arena so the indirect path packs its record.
    let mut live = session_with_stone_chunks(&[(0, 0), (0, -1), (1, 0), (0, 1)]);
    assert_eq!(indirect.update_live_terrain(&live.world), 4);

    let camera = Camera {
        position: [8.0, 10.0, 20.0],
        yaw_deg: 0.0,
        pitch_deg: 0.0,
        ..Camera::default()
    };
    let record_bytes = std::mem::size_of::<SectionGpuRecord>() as u64;

    // Frame 1: the empty shadow diffs fully dirty — one full-pack
    // `write_buffer` of `4 * 64` bytes seeds the GPU buffer.
    let px1 = indirect
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("indirect frame 1");
    assert!(
        px1.as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[0] != 0 || px[1] != 0 || px[2] != 0),
        "frame 1 must be non-blank"
    );
    assert!(
        indirect.gpu_indirect_record_uploads >= 1,
        "frame 1 must upload the initial record pack, got {} calls",
        indirect.gpu_indirect_record_uploads
    );
    assert_eq!(
        indirect.gpu_indirect_record_bytes_uploaded,
        4 * record_bytes,
        "frame 1 uploads the whole 4-record pack, got {} bytes",
        indirect.gpu_indirect_record_bytes_uploaded
    );
    eprintln!(
        "frame 1: {} record write_buffer calls, {} bytes",
        indirect.gpu_indirect_record_uploads, indirect.gpu_indirect_record_bytes_uploaded
    );

    // Frames 2..=6: camera rotates and slides ±0.4 blocks — every
    // section stays inside its quantized 16-block distance ring (all
    // centres sit >3 blocks from a ring boundary), so the
    // (ring, morton) order and every packed record stay byte-identical.
    // Zero residency change ⇒ zero record-buffer writes.
    let bytes_after_1 = indirect.gpu_indirect_record_bytes_uploaded;
    let uploads_after_1 = indirect.gpu_indirect_record_uploads;
    let steady_cameras = [
        Camera {
            position: [8.2, 10.0, 20.1],
            yaw_deg: 12.0,
            pitch_deg: -4.0,
            ..Camera::default()
        },
        Camera {
            position: [7.8, 10.2, 20.0],
            yaw_deg: -15.0,
            pitch_deg: 3.0,
            ..Camera::default()
        },
        Camera {
            position: [8.1, 9.8, 20.3],
            yaw_deg: 30.0,
            pitch_deg: -8.0,
            ..Camera::default()
        },
        Camera {
            position: [7.9, 10.1, 19.7],
            yaw_deg: -28.0,
            pitch_deg: 6.0,
            ..Camera::default()
        },
        Camera {
            position: [8.0, 10.0, 20.0],
            yaw_deg: 45.0,
            pitch_deg: -12.0,
            ..Camera::default()
        },
    ];
    for (i, steady) in steady_cameras.iter().enumerate() {
        let px = indirect
            .render_offscreen_rgba(128, 128, steady, None)
            .unwrap_or_else(|e| panic!("indirect frame {}: {e}", i + 2));
        assert!(
            px.as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[0] != 0 || px[1] != 0 || px[2] != 0),
            "steady frame {} must be non-blank",
            i + 2
        );
    }
    assert_eq!(
        indirect.gpu_indirect_record_bytes_uploaded, bytes_after_1,
        "steady frames must upload 0 record bytes, got +{} bytes",
        indirect.gpu_indirect_record_bytes_uploaded - bytes_after_1
    );
    assert_eq!(
        indirect.gpu_indirect_record_uploads, uploads_after_1,
        "steady frames must issue 0 record write_buffer calls, got +{}",
        indirect.gpu_indirect_record_uploads - uploads_after_1
    );
    assert!(
        indirect.gpu_indirect_record_uploads_skipped >= 5,
        "all 5 steady frames must skip the record upload, got {} skips",
        indirect.gpu_indirect_record_uploads_skipped
    );
    eprintln!(
        "frames 2-6: +0 record bytes, {} skipped uploads",
        indirect.gpu_indirect_record_uploads_skipped
    );

    // Frame 7: one new chunk arrives — section (-1, 0, -4) lands in the
    // far region (-8, 0, -8), which sorts behind both resident regions,
    // so its record appends at index 4. The diff must upload exactly
    // that one 64-byte record — strictly less than the 5*64 full pack.
    push_stone_chunk(&mut live, -1, -4);
    assert_eq!(indirect.update_live_terrain(&live.world), 1);
    let px7 = indirect
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("indirect frame 7");
    assert_eq!(
        indirect.gpu_indirect_record_coords().len(),
        5,
        "the arriving section must be a committed record on frame 7, got {:?}",
        indirect.gpu_indirect_record_coords()
    );
    assert!(
        indirect
            .gpu_indirect_record_coords()
            .contains(&SectionCoord(-1, 0, -4)),
        "the new section must appear in the record order, got {:?}",
        indirect.gpu_indirect_record_coords()
    );
    let delta_uploads = indirect.gpu_indirect_record_uploads - uploads_after_1;
    let delta_bytes = indirect.gpu_indirect_record_bytes_uploaded - bytes_after_1;
    assert!(
        delta_bytes < 5 * record_bytes,
        "one arriving section must upload strictly less than the full \
         {full}-byte pack, got {delta_bytes} bytes",
        full = 5 * record_bytes
    );
    assert_eq!(
        delta_bytes, record_bytes,
        "one appended record must upload exactly one {record_bytes}-byte span, \
         got {delta_bytes} bytes over {delta_uploads} calls"
    );
    eprintln!(
        "frame 7: +{delta_bytes} record bytes over +{delta_uploads} calls \
         (partial span, one 64-byte record of a 320-byte pack)"
    );

    // The rendered frame must carry the new section's geometry exactly:
    // the same camera's frame-1 pixels change only where the arriving
    // section projects, and a second indirect renderer full-uploading
    // the same five-section world from an empty shadow rasterizes
    // pixel-identical output — the 64-byte partial span produced the
    // exact GPU record contents.
    let mut bbox = [usize::MAX, usize::MAX, 0usize, 0usize];
    let mut diff_vs_px1 = 0usize;
    let (a7, _) = px7.as_chunks::<4>();
    let (a1, _) = px1.as_chunks::<4>();
    for (i, (a, b)) in a7.iter().zip(a1.iter()).enumerate() {
        if a != b {
            diff_vs_px1 += 1;
            let (x, y) = (i % 128, i / 128);
            bbox[0] = bbox[0].min(x);
            bbox[1] = bbox[1].min(y);
            bbox[2] = bbox[2].max(x);
            bbox[3] = bbox[3].max(y);
        }
    }
    assert!(
        diff_vs_px1 > 0,
        "frame 7 must differ from frame 1 — the new section changed the image"
    );
    // The arriving section at (-1, 0, -4) projects left of the wall —
    // its changed pixels live in the left half of the frame.
    assert!(
        bbox[0] < 64 && bbox[2] < 96,
        "the changed region must sit left-of-centre where (-1, 0, -4) \
         projects, got bbox {bbox:?} over {diff_vs_px1} pixels"
    );
    eprintln!("frame7-vs-frame1 diff pixels: {diff_vs_px1} bbox {bbox:?}");

    let Some(ctx2) = try_gpu() else {
        return;
    };
    let mut fresh = GpuRenderer::new(ctx2, OFFSCREEN_FORMAT);
    fresh.set_gpu_indirect_enabled(true);
    assert_eq!(fresh.update_live_terrain(&live.world), 5);
    let fresh_px = fresh
        .render_offscreen_rgba(128, 128, &camera, None)
        .expect("fresh indirect reference frame");
    assert_eq!(
        px7, fresh_px,
        "the partially-uploaded indirect frame must rasterize the same \
         pixels as a fresh full-upload indirect renderer (geometry \
         accurately reflected)"
    );

    // Byte-level proof the 64-byte span landed the right payload: the
    // GPU-written args for the arriving section show it drawn
    // (`instance_count == 1`), and its region vertex buffer is
    // byte-identical to the fresh full-upload renderer's.
    let coords = indirect.gpu_indirect_record_coords();
    let args_buffer = indirect
        .gpu_indirect_args_buffer()
        .expect("indirect buffers live")
        .clone();
    let bytes = read_buffer(
        &indirect.gpu().device,
        &indirect.gpu().queue,
        &args_buffer,
        (coords.len() * std::mem::size_of::<IndirectArgs>()) as u64,
    );
    let args: &[IndirectArgs] = bytemuck::cast_slice(&bytes);
    let new_index = coords
        .iter()
        .position(|&c| c == SectionCoord(-1, 0, -4))
        .expect("new section in record order");
    assert_eq!(
        args[new_index].instance_count, 1,
        "the GPU cull must keep the arriving section — it is inside the frustum"
    );
    let indirect_vb = indirect
        .debug_region_arena_vertex_buffer([-8, 0, -8])
        .expect("new region arena")
        .0
        .clone();
    let fresh_vb = fresh
        .debug_region_arena_vertex_buffer([-8, 0, -8])
        .expect("new region arena")
        .0
        .clone();
    let indirect_vb_bytes = read_buffer(
        &indirect.gpu().device,
        &indirect.gpu().queue,
        &indirect_vb,
        24 * 16,
    );
    let fresh_vb_bytes = read_buffer(
        &fresh.gpu().device,
        &fresh.gpu().queue,
        &fresh_vb,
        24 * 16,
    );
    assert_eq!(
        indirect_vb_bytes, fresh_vb_bytes,
        "the partial upload must leave byte-identical GPU vertex data"
    );
}
