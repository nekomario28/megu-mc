// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::BTreeSet;
use std::mem::size_of;

use bytemuck::{Pod, Zeroable};
use megu_client_gpu::planar_quad_pulling::{
    build_planar_quad_pulling_wgsl, vram_footprint_report, GpuPlanarQuad, PlanarQuadPullingPipeline,
};
use megu_client_gpu::renderer::OFFSCREEN_FORMAT;
use megu_client_gpu::{aligned_copy_bytes_per_row, mat4_mul_vec4, Camera, Mat4};
use megu_client_mesh::compact_vertex::{
    emit_wgsl_quad_pull_unpack, ExpandedQuadCorner, PlanarQuadRecord,
};
use megu_client_mesh::DIR_BASIS;
use wgpu::util::DeviceExt;

// Odd width exercises the padded GPU copy and tightly packed RGBA readback.
const WIDTH: u32 = 127;
const HEIGHT: u32 = 109;
const CUBE_MIN_FIXED: [u32; 3] = [4096, 4352, 5120];
const CUBE_SIDE_FIXED: u32 = 3 * 1024;

#[test]
fn layout_and_vram_footprint_match_the_quad_contract() {
    assert_eq!(size_of::<GpuPlanarQuad>(), 24);
    let records = merged_cube();
    let gpu_records = GpuPlanarQuad::from_records(&records);
    assert!(std::ptr::eq(gpu_records.as_ptr().cast(), records.as_ptr()));
    assert_eq!(gpu_records[5].state_id, records[5].state_id);
    assert_eq!(bytemuck::cast_slice::<_, u8>(gpu_records).len(), 6 * 24);

    for count in [0, 1, records.len(), usize::MAX] {
        let report = vram_footprint_report(count);
        assert_eq!(report.legacy_vertex_bytes, count as u128 * 48);
        assert_eq!(report.legacy_index_bytes, count as u128 * 24);
        assert_eq!(report.pulling_storage_bytes, count as u128 * 24);
        assert_eq!(report.pulling_index_bytes, 0);
        assert_eq!(report.legacy_vertex_bytes, report.pulling_storage_bytes * 2);
        assert_eq!(
            report.legacy_vertex_bytes + report.legacy_index_bytes,
            report.pulling_storage_bytes * 3,
        );
    }
}

#[test]
fn shader_embeds_the_canonical_unpacker_and_has_a_24_byte_storage_stride() {
    let source = build_planar_quad_pulling_wgsl();
    assert!(source.starts_with(&emit_wgsl_quad_pull_unpack()));
    let module = wgpu::naga::front::wgsl::parse_str(&source)
        .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&source)));
    wgpu::naga::valid::Validator::new(
        wgpu::naga::valid::ValidationFlags::all(),
        wgpu::naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
    let quads = module
        .global_variables
        .iter()
        .map(|(_, global)| global)
        .find(|global| global.name.as_deref() == Some("quads"))
        .unwrap();
    assert!(matches!(
        module.types[quads.ty].inner,
        wgpu::naga::TypeInner::Array { stride: 24, .. }
    ));
}

fn merged_cube() -> [PlanarQuadRecord; 6] {
    const AO: [[u8; 4]; 6] = [
        [0, 1, 3, 2],
        [1, 0, 3, 2],
        [3, 0, 2, 1],
        [0, 3, 1, 2],
        [2, 0, 1, 3],
        [3, 1, 2, 0],
    ];
    const TINTS: [u32; 6] = [0, 0x24_b9_ed, 0, 0xed_b9_24, 0, 0xb9_24_ed];
    std::array::from_fn(|face| {
        let origin = DIR_BASIS[face].origin.map(u32::from);
        let origin =
            std::array::from_fn(|axis| CUBE_MIN_FIXED[axis] + origin[axis] * CUBE_SIDE_FIXED);
        let ao = AO[face];
        PlanarQuadRecord::pack(
            0x8f12_3400 + face as u32 * 0x0100_0001,
            origin,
            [CUBE_SIDE_FIXED; 2],
            face as u8,
            ao[0] + ao[2] > ao[1] + ao[3],
            ao,
            [1, 10, 5, 9].map(|level| level + face as u8),
            [15, 7, 12, 6].map(|level| level - face as u8),
            TINTS[face],
            0xa5,
        )
        .unwrap()
    })
}

// Opposing orthographic views along the cube's body diagonal. The matrix
// entries and fixture positions are exact binary fractions, so projection
// agrees bitwise between CPU and GPU even when the driver contracts mul/add.
fn cube_view_projection(side: f32, center: [f32; 3]) -> Mat4 {
    let mut matrix = [
        [side / 8.0, -1.0 / 16.0, -side / 32.0, 0.0],
        [0.0, 1.0 / 8.0, -side / 32.0, 0.0],
        [-side / 8.0, -1.0 / 16.0, -side / 32.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let [x, y, z, _] = project(&matrix, center);
    matrix[3] = [-x, -y, 0.5 - z, 1.0];
    matrix
}

fn project(matrix: &Mat4, [x, y, z]: [f32; 3]) -> [f32; 4] {
    mat4_mul_vec4(matrix, [x, y, z, 1.0])
}

fn reference_color(corner: ExpandedQuadCorner, face: usize) -> [f32; 3] {
    let palette_rgb = corner.state_id.wrapping_mul(0x9e37_79b9).to_be_bytes();
    let albedo = if corner.tint == 0 {
        [palette_rgb[0], palette_rgb[1], palette_rgb[2]].map(|v| 0.5 + 0.5 * (f32::from(v) / 255.0))
    } else {
        let [_, r, g, b] = corner.tint.to_be_bytes();
        [r, g, b].map(|v| f32::from(v) / 255.0)
    };
    let illumination = 0.25 + 0.75 * (f32::from(corner.block_light.max(corner.sky_light)) / 15.0);
    let face_shade = [0.45, 1.0, 0.75, 0.85, 0.7, 0.95][face];
    let ao = [0.4, 0.6, 0.8, 1.0][usize::from(corner.ao)];
    let shade = face_shade * ao * illumination;
    albedo.map(|channel| channel * shade)
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ReferenceVertex {
    clip: [f32; 4],
    color: [f32; 3],
}

// Use the same Vulkan rasterizer for both frames so that subpixel coverage,
// perspective interpolation and sRGB quantization can be compared bytewise.
// The reference vertex shader only forwards the CPU-expanded and projected
// corners, with an ordinary vertex buffer and explicit CPU index buffer.
fn render_reference(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    matrix: &Mat4,
    records: &[PlanarQuadRecord; 6],
) -> Vec<u8> {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (face, record) in records.iter().enumerate() {
        let base = vertices.len() as u16;
        vertices.extend(record.expand_corners().map(|corner| ReferenceVertex {
            clip: project(matrix, corner.position),
            color: reference_color(corner, face),
        }));
        indices.extend(record.indices().map(|index| base + u16::from(index)));
    }
    let vertices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("reference vertices"),
        contents: bytemuck::cast_slice(&vertices),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("reference indices"),
        contents: bytemuck::cast_slice(&indices),
        usage: wgpu::BufferUsages::INDEX,
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("CPU-expanded reference"),
        source: wgpu::ShaderSource::Wgsl(
            r#"
struct Vertex {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec3<f32>,
};
@vertex fn vs(@location(0) clip: vec4<f32>, @location(1) color: vec3<f32>) -> Vertex {
    return Vertex(clip, color);
}
@fragment fn fs(in: Vertex) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
"#
            .into(),
        ),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("CPU-expanded reference"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<ReferenceVertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x3],
            })],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState {
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(OFFSCREEN_FORMAT.into())],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    });
    let size = wgpu::Extent3d {
        width: WIDTH,
        height: HEIGHT,
        depth_or_array_layers: 1,
    };
    let color_desc = wgpu::TextureDescriptor {
        label: Some("reference color"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OFFSCREEN_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    };
    let color = device.create_texture(&color_desc);
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("reference depth"),
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        ..color_desc
    });
    let padded_row = aligned_copy_bytes_per_row(WIDTH);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("reference readback"),
        size: u64::from(padded_row) * u64::from(HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color.create_view(&Default::default()),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth.create_view(&Default::default()),
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_vertex_buffer(0, vertices.slice(..));
        pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
    }
    encoder.copy_texture_to_buffer(
        color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: None,
            },
        },
        size,
    );
    let submission_index = queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    readback.map_async(wgpu::MapMode::Read, .., move |result| {
        tx.send(result).unwrap();
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission_index),
            timeout: None,
        })
        .unwrap();
    rx.recv().unwrap().unwrap();
    let mapped = readback.get_mapped_range(..).unwrap();
    mapped
        .chunks_exact(padded_row as usize)
        .flat_map(|row| row[..WIDTH as usize * 4].iter().copied())
        .collect()
}

fn assert_frame_eq(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), (WIDTH * HEIGHT * 4) as usize);
    assert_eq!(actual.len(), expected.len());
    let mut differences = actual
        .as_chunks::<4>()
        .0
        .iter()
        .zip(expected.as_chunks::<4>().0)
        .enumerate()
        .filter(|(_, (a, b))| a != b);
    if let Some((pixel, (a, b))) = differences.next() {
        panic!(
            "{} pixels differ, first at ({}, {}): pulling {a:?}, reference {b:?}",
            differences.count() + 1,
            pixel % WIDTH as usize,
            pixel / WIDTH as usize,
        );
    }
}

#[test]
fn vulkan_pulling_matches_cpu_expanded_quads() {
    let backends = wgpu::Backends::VULKAN & wgpu::Instance::enabled_backend_features();
    if backends.is_empty() {
        eprintln!("SKIP: this build has no Vulkan backend");
        return;
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(backends));
    let Some(adapter) =
        adapters
            .into_iter()
            .min_by_key(|adapter| match adapter.get_info().device_type {
                wgpu::DeviceType::DiscreteGpu => 0,
                wgpu::DeviceType::IntegratedGpu => 1,
                wgpu::DeviceType::Cpu => 3,
                _ => 2,
            })
    else {
        eprintln!("SKIP: no Vulkan adapter");
        return;
    };
    let info = adapter.get_info();
    assert_eq!(info.backend, wgpu::Backend::Vulkan);
    eprintln!("Vulkan adapter: {} ({:?})", info.name, info.device_type);
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let pipeline = PlanarQuadPullingPipeline::new(&device);
    let records = merged_cube();
    assert!(records.iter().any(|r| r.indices() == [0, 1, 3, 1, 2, 3]));
    assert!(records.iter().any(|r| r.indices() == [0, 1, 2, 0, 2, 3]));

    let mut face_colors = BTreeSet::new();
    let half_side = CUBE_SIDE_FIXED as f32 / (2.0 * 1024.0);
    let center = CUBE_MIN_FIXED.map(|v| v as f32 / 1024.0 + half_side);
    for (side, visible_faces) in [(1.0, [1, 3, 5]), (-1.0, [0, 2, 4])] {
        let matrix = cube_view_projection(side, center);
        let frame = pipeline
            .render_offscreen_rgba_with_view_projection(
                &device, &queue, &records, &matrix, WIDTH, HEIGHT,
            )
            .unwrap();
        let reference = render_reference(&device, &queue, &matrix, &records);
        assert_frame_eq(&frame, &reference);
        let pixels = frame.as_chunks::<4>().0;
        assert!(pixels.contains(&[0, 0, 0, 0]));
        assert!(
            pixels
                .iter()
                .filter(|pixel| pixel[3] == 255 && pixel[..3] != [0, 0, 0])
                .count()
                > 500
        );
        for face in visible_faces {
            let point = std::array::from_fn(|axis| {
                center[axis] + f32::from(DIR_BASIS[face].n[axis]) * half_side
            });
            let [x, y, _, w] = project(&matrix, point);
            let col = ((0.5 + 0.5 * x / w) * WIDTH as f32) as usize;
            let row = ((0.5 - 0.5 * y / w) * HEIGHT as f32) as usize;
            let offset = (row * WIDTH as usize + col) * 4;
            let pixel = &frame[offset..offset + 4];
            assert_eq!(pixel[3], 255, "face {face} center must be opaque");
            face_colors.insert([pixel[0], pixel[1], pixel[2]]);
        }
    }
    assert_eq!(
        face_colors.len(),
        6,
        "every face must have distinct shading"
    );

    let camera = Camera {
        position: [5.5, 5.75, 14.0],
        pitch_deg: 0.0,
        ..Camera::default()
    };
    let render = |quads: &[PlanarQuadRecord]| {
        pipeline
            .render_offscreen_rgba(&device, &queue, quads, &camera, WIDTH, HEIGHT)
            .unwrap()
    };
    assert!(render(&[]).iter().all(|&byte| byte == 0));
    assert!(pipeline
        .render_offscreen_rgba(&device, &queue, &records, &camera, 0, HEIGHT)
        .is_err());
    let near = records[3];
    let far = PlanarQuadRecord::pack(
        17,
        CUBE_MIN_FIXED,
        [CUBE_SIDE_FIXED; 2],
        3,
        false,
        [3; 4],
        [15; 4],
        [15; 4],
        0xff_00_00,
        0,
    )
    .unwrap();
    let near_frame = render(&[near]);
    assert_ne!(near_frame, render(&[far]));
    assert_frame_eq(&render(&[near, far]), &near_frame);
    assert_frame_eq(&render(&[far, near]), &near_frame);
}
