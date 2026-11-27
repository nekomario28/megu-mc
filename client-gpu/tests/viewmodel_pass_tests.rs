// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::aligned_copy_bytes_per_row;
use megu_client_gpu::renderer::{
    probe_adapters, request_gpu_device, FirstPersonUseKind, FirstPersonViewmodel,
};
use megu_client_gpu::viewmodel_pass::{
    build_viewmodel_geometry, enchanted_glint_uv_offset, viewmodel_pose, FirstPersonViewmodelPass,
    ViewmodelGeometryInfo, ViewmodelGeometryKind, ViewmodelVertex, VIEWMODEL_MAX_INDICES,
    VIEWMODEL_MAX_VERTICES, VIEWMODEL_WGSL,
};
use megu_client_mesh::RenderBackendPreference;
use wgpu::naga::front::wgsl;
use wgpu::naga::valid::{Capabilities, ValidationFlags, Validator};
use wgpu::naga::ShaderStage;

const _: () = {
    assert!(VIEWMODEL_MAX_VERTICES >= 256);
    assert!(VIEWMODEL_MAX_INDICES >= 384);
};

fn compile_and_validate(source: &str) -> wgpu::naga::Module {
    let module = wgsl::parse_str(source)
        .unwrap_or_else(|err| panic!("naga parse failed:\n{}", err.emit_to_string(source)));
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|err| panic!("naga validation failed:\n{}", err.emit_to_string(source)));
    module
}

fn has_entry_point(module: &wgpu::naga::Module, stage: ShaderStage, name: &str) -> bool {
    module
        .entry_points
        .iter()
        .any(|entry| entry.stage == stage && entry.name == name)
}

fn viewmodel(held_item_name: Option<&str>, is_block: bool) -> FirstPersonViewmodel {
    FirstPersonViewmodel {
        visible: true,
        held_item_name: held_item_name.map(str::to_string),
        held_color_rgb: [1.0; 3],
        is_block,
        swing_progress: 0.0,
        use_progress: 0.0,
        using_kind: FirstPersonUseKind::None,
        walk_bob_x: 0.0,
        walk_bob_y: 0.0,
        equip_progress: 1.0,
    }
}

fn build(
    vm: &FirstPersonViewmodel,
) -> (
    ViewmodelGeometryInfo,
    [ViewmodelVertex; VIEWMODEL_MAX_VERTICES],
    [u16; VIEWMODEL_MAX_INDICES],
) {
    let mut vertices = [ViewmodelVertex::default(); VIEWMODEL_MAX_VERTICES];
    let mut indices = [0; VIEWMODEL_MAX_INDICES];
    let info = build_viewmodel_geometry(
        vm,
        16.0 / 9.0,
        Some([0.0, 0.0, 1.0, 1.0]),
        0.0,
        &mut vertices,
        &mut indices,
    )
    .expect("visible viewmodel must emit geometry");
    (info, vertices, indices)
}

fn pose(
    vm: &FirstPersonViewmodel,
    time_secs: f32,
) -> megu_client_gpu::viewmodel_pass::ViewmodelPose {
    viewmodel_pose(vm, 16.0 / 9.0, time_secs)
}

#[test]
fn viewmodel_wgsl_compiles_with_real_naga() {
    let module = compile_and_validate(VIEWMODEL_WGSL);
    assert!(has_entry_point(&module, ShaderStage::Vertex, "vs_main"));
    assert!(has_entry_point(&module, ShaderStage::Fragment, "fs_main"));
    assert!(VIEWMODEL_WGSL.contains("@binding(1) var surface_texture"));
    assert!(VIEWMODEL_WGSL.contains("@binding(2) var surface_sampler"));
    assert!(VIEWMODEL_WGSL.contains("@binding(3) var glint_texture"));
    assert!(wgsl::parse_str("@fragment fn fs_main() -> vec4<f32> { return missing; }").is_err());
}

#[test]
fn bare_arm_block_and_item_geometry_are_finite_indexed_cuboids() {
    for (mut vm, expected_kind) in [
        (viewmodel(None, false), ViewmodelGeometryKind::BareArm),
        (
            viewmodel(Some("minecraft:stone"), true),
            ViewmodelGeometryKind::HeldBlock,
        ),
        (
            viewmodel(Some("minecraft:iron_sword"), false),
            ViewmodelGeometryKind::HeldItem,
        ),
    ] {
        vm.walk_bob_x = 0.01;
        vm.walk_bob_y = -0.01;
        let (info, vertices, indices) = build(&vm);
        assert_eq!(info.kind, expected_kind);
        assert_eq!(info.vertex_count, 24);
        assert_eq!(info.index_count, 36);
        assert_eq!(info.index_count % 3, 0);
        for vertex in &vertices[..info.vertex_count] {
            assert!(vertex.position.iter().all(|v| v.is_finite()));
            assert!(vertex
                .uv
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
            assert!(vertex.color.iter().all(|v| v.is_finite() && *v >= 0.0));
        }
        for index in &indices[..info.index_count] {
            assert!((*index as usize) < info.vertex_count);
        }
        let xs: Vec<f32> = vertices[..info.vertex_count]
            .iter()
            .map(|vertex| vertex.position[0])
            .collect();
        let ys: Vec<f32> = vertices[..info.vertex_count]
            .iter()
            .map(|vertex| vertex.position[1])
            .collect();
        let zs: Vec<f32> = vertices[..info.vertex_count]
            .iter()
            .map(|vertex| vertex.position[2])
            .collect();
        for axis in [xs, ys, zs] {
            let min = axis.iter().copied().fold(f32::INFINITY, f32::min);
            let max = axis.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            assert!(max - min > 0.005, "geometry collapsed on one axis");
        }
    }
}

#[test]
fn bare_arm_uses_neutral_directional_shade_with_right_arm_skin_uvs() {
    let vm = viewmodel(None, false);
    let mut vertices = [ViewmodelVertex::default(); VIEWMODEL_MAX_VERTICES];
    let mut indices = [0; VIEWMODEL_MAX_INDICES];
    let info = build_viewmodel_geometry(&vm, 16.0 / 9.0, None, 0.0, &mut vertices, &mut indices)
        .expect("bare arm geometry");
    let first = vertices[0];
    assert_eq!(info.kind, ViewmodelGeometryKind::BareArm);
    assert!((first.color[0] - first.color[1]).abs() < 1e-6);
    assert!((first.color[1] - first.color[2]).abs() < 1e-6);
    assert!(first.color[0] > 0.45 && first.color[0] <= 1.0);
    assert!((first.color[3] - 1.0).abs() < 1e-6);
    for vertex in &vertices[..info.vertex_count] {
        assert!(
            (40.0 / 64.0..=56.0 / 64.0).contains(&vertex.uv[0]),
            "unexpected bare arm U coordinate: {}",
            vertex.uv[0]
        );
        assert!(
            (16.0 / 64.0..=32.0 / 64.0).contains(&vertex.uv[1]),
            "unexpected bare arm V coordinate: {}",
            vertex.uv[1]
        );
    }
}

#[test]
fn swing_animation_dips_forward_and_turns_inward() {
    let mut vm = viewmodel(Some("minecraft:iron_sword"), false);
    let rest = pose(&vm, 0.0);
    vm.swing_progress = 0.5;
    let swinging = pose(&vm, 0.0);
    assert!(swinging.translation[2] < rest.translation[2] - 0.10);
    assert!(swinging.translation[0] < rest.translation[0]);
    assert!(swinging.rotation[1] < rest.rotation[1]);
    assert!(swinging.rotation[2] < rest.rotation[2]);
    assert!(swinging.translation.iter().all(|v| v.is_finite()));
}

#[test]
fn walk_bob_and_equip_progress_move_the_viewmodel() {
    let mut vm = viewmodel(Some("minecraft:stone"), true);
    let rest = pose(&vm, 0.0);
    vm.walk_bob_x = 0.04;
    vm.walk_bob_y = -0.03;
    let bobbed = pose(&vm, 0.0);
    assert!(bobbed.translation[0] > rest.translation[0]);
    assert!(bobbed.translation[1] < rest.translation[1]);

    let mut lowering = viewmodel(Some("minecraft:stone"), true);
    lowering.equip_progress = 0.0;
    let lowered = pose(&lowering, 0.0);
    assert!(lowered.translation[1] < rest.translation[1] - 0.30);
}

#[test]
fn eat_and_drink_poses_reach_mouth_with_rhythmic_bob() {
    for using_kind in [FirstPersonUseKind::Eat, FirstPersonUseKind::Drink] {
        let mut vm = viewmodel(Some("minecraft:bread"), false);
        let rest = pose(&vm, 0.0);
        vm.using_kind = using_kind;
        vm.use_progress = 0.5;
        let using = pose(&vm, 0.0);
        assert!(using.translation[0] < rest.translation[0] - 0.08);
        assert!(using.translation[1] > rest.translation[1] + 0.04);
        assert!(using.translation[2] > rest.translation[2] + 0.04);

        vm.use_progress = 0.125;
        let munch_a = pose(&vm, 0.0).translation;
        vm.use_progress = 0.375;
        let munch_b = pose(&vm, 0.0).translation;
        assert_ne!(munch_a, munch_b);
    }
}

#[test]
fn bow_and_crossbow_pull_back_with_charge_tremor() {
    for using_kind in [
        FirstPersonUseKind::BowDraw,
        FirstPersonUseKind::CrossbowCharge,
    ] {
        let mut vm = viewmodel(Some("minecraft:bow"), false);
        let rest = pose(&vm, 0.0);
        vm.using_kind = using_kind;
        vm.use_progress = 1.0;
        let drawn = pose(&vm, 0.0);
        assert!(drawn.translation[2] > rest.translation[2] + 0.10);
        assert!(drawn.translation[0] < rest.translation[0]);
        let tremor_a = pose(&vm, 0.13).translation;
        let tremor_b = pose(&vm, 0.47).translation;
        assert_ne!(tremor_a, tremor_b);
    }
}

#[test]
fn shield_and_trident_use_poses_move_in_guard_and_throw_directions() {
    let mut shield = viewmodel(Some("minecraft:shield"), false);
    let rest = pose(&shield, 0.0);
    shield.using_kind = FirstPersonUseKind::ShieldBlock;
    shield.use_progress = 1.0;
    let guard = pose(&shield, 0.0);
    assert!(guard.translation[0] < rest.translation[0] - 0.25);
    assert!(guard.rotation[1] > rest.rotation[1] + 0.5);

    let mut trident = viewmodel(Some("minecraft:trident"), false);
    let trident_rest = pose(&trident, 0.0);
    trident.using_kind = FirstPersonUseKind::TridentThrow;
    trident.use_progress = 1.0;
    let throw = pose(&trident, 0.0);
    assert!(throw.translation[2] > trident_rest.translation[2] + 0.10);
    assert!(throw.rotation[0] < trident_rest.rotation[0] - 0.4);
}

#[test]
fn enchanted_glint_scroll_is_time_animated() {
    let early = enchanted_glint_uv_offset(0.25);
    let late = enchanted_glint_uv_offset(1.75);
    assert_ne!(early, late);
    for component in early.into_iter().chain(late) {
        assert!((0.0..1.0).contains(&component));
    }
    assert!(VIEWMODEL_WGSL.contains("glint_strength > 0.0"));
    assert!(VIEWMODEL_WGSL.contains("fract(in.uv"));
    assert!(VIEWMODEL_WGSL.contains("time_secs"));
}

#[test]
fn invisible_viewmodel_emits_no_geometry() {
    let mut vm = viewmodel(Some("minecraft:stone"), true);
    vm.visible = false;
    let mut vertices = [ViewmodelVertex::default(); VIEWMODEL_MAX_VERTICES];
    let mut indices = [0; VIEWMODEL_MAX_INDICES];
    assert!(
        build_viewmodel_geometry(&vm, 16.0 / 9.0, None, 0.0, &mut vertices, &mut indices,)
            .is_none()
    );
}

#[test]
fn pass_encodes_visible_pixels_on_real_headless_gpu_when_available() {
    if probe_adapters(RenderBackendPreference::Auto).is_empty() {
        eprintln!("SKIP viewmodel GPU smoke: no wgpu adapter available");
        return;
    }
    let ctx = request_gpu_device(RenderBackendPreference::Auto, None, None)
        .expect("probe found adapters but request_gpu_device failed");
    let width = 64;
    let height = 64;
    let mut pass = FirstPersonViewmodelPass::new(
        &ctx.device,
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Depth32Float,
    );
    assert_eq!(pass.vertex_capacity(), VIEWMODEL_MAX_VERTICES);
    assert_eq!(pass.index_capacity(), VIEWMODEL_MAX_INDICES);
    let surface_rgba = [220, 80, 40, 255].repeat(4);
    assert!(pass.upload_surface_texture(&ctx.device, &ctx.queue, 2, 2, &surface_rgba));
    assert_eq!(pass.surface_texture_size(), (2, 2));
    let glint_rgba = [180, 80, 255, 255].repeat(4);
    assert!(pass.upload_glint_texture(&ctx.device, &ctx.queue, 2, 2, &glint_rgba));
    assert_eq!(pass.glint_texture_size(), (2, 2));
    assert!(!pass.upload_surface_texture(&ctx.device, &ctx.queue, 3, 3, &[255; 4]));

    let target = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewmodel-test-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("viewmodel-test-depth"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("viewmodel-test-encoder"),
        });
    let vm = viewmodel(Some("minecraft:iron_sword"), false);
    let error_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    pass.encode(
        &ctx.queue,
        &mut encoder,
        &target_view,
        &depth_view,
        &vm,
        width as f32 / height as f32,
        [1.0; 3],
        Some([0.0, 0.0, 1.0, 1.0]),
        true,
        0.25,
    );
    let padded_bpr = aligned_copy_bytes_per_row(width);
    let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewmodel-test-readback"),
        size: u64::from(padded_bpr) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bpr),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue.submit([encoder.finish()]);
    if let Some(error) = pollster::block_on(error_scope.pop()) {
        panic!("viewmodel pass produced a wgpu validation error: {error}");
    }
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap();
    rx.recv().unwrap().unwrap();
    let mapped = slice.get_mapped_range().unwrap();
    let mut lit_pixels = 0usize;
    for row in 0..height as usize {
        let start = row * padded_bpr as usize;
        for pixel in mapped[start..start + width as usize * 4].as_chunks::<4>().0 {
            if pixel[3] != 0 && pixel[..3].iter().any(|channel| *channel != 0) {
                lit_pixels += 1;
            }
        }
    }
    drop(mapped);
    readback.unmap();
    assert!(
        lit_pixels > 16,
        "viewmodel pass must rasterize visible pixels, got {lit_pixels}"
    );
}

#[test]
fn fixed_capacity_and_encode_shape_satisfy_zero_allocation_contract() {
    let source = include_str!("../src/viewmodel_pass.rs");
    let encode_start = source
        .find("pub fn encode(")
        .expect("encode method must exist");
    let encode_tail = &source[encode_start..];
    let encode_end = encode_tail
        .find("\n    fn rebuild_bind_group")
        .expect("encode method end must be locatable");
    let encode_body = &encode_tail[..encode_end];
    assert!(!encode_body.contains("Vec"));
    assert!(!encode_body.contains("create_buffer"));
    assert!(!encode_body.contains("create_texture"));
    assert!(!encode_body.contains("create_render_pipeline"));
}

#[test]
fn offhand_geometry_reflects_the_held_item_and_preserves_winding() {
    use megu_client_gpu::viewmodel_pass::build_offhand_viewmodel_geometry;

    let mut vm = FirstPersonViewmodel {
        visible: true,
        held_item_name: Some("minecraft:shield".into()),
        equip_progress: 1.0,
        ..FirstPersonViewmodel::default()
    };
    let mut right_vertices = [ViewmodelVertex::default(); VIEWMODEL_MAX_VERTICES];
    let mut left_vertices = right_vertices;
    let mut right_indices = [0; VIEWMODEL_MAX_INDICES];
    let mut left_indices = right_indices;
    let aspect = 16.0 / 9.0;
    let right = build_viewmodel_geometry(
        &vm,
        aspect,
        None,
        0.0,
        &mut right_vertices,
        &mut right_indices,
    )
    .unwrap();
    let left = build_offhand_viewmodel_geometry(
        &vm,
        aspect,
        0.0,
        &mut left_vertices,
        &mut left_indices,
    )
    .unwrap();
    assert_eq!(left.vertex_count, right.vertex_count);
    assert_eq!(left.index_count, right.index_count);
    assert!(left.pose.translation[0] < 0.0);
    for (left, right) in left_vertices[..left.vertex_count]
        .iter()
        .zip(&right_vertices[..right.vertex_count])
    {
        assert!(left.position.iter().all(|v| v.is_finite()));
        assert!(left.position[0] < 0.0);
        let [x, y, z] = right.position;
        assert_eq!(left.position, [-x, y, z]);
        assert_eq!(left.uv, right.uv);
    }
    for (left, right) in left_indices[..left.index_count]
        .as_chunks::<3>()
        .0
        .iter()
        .zip(right_indices[..right.index_count].as_chunks::<3>().0)
    {
        assert_eq!(left, &[right[0], right[2], right[1]]);
    }

    vm.using_kind = FirstPersonUseKind::ShieldBlock;
    vm.use_progress = 1.0;
    let raised = build_offhand_viewmodel_geometry(
        &vm,
        aspect,
        0.0,
        &mut left_vertices,
        &mut left_indices,
    )
    .unwrap();
    assert!(raised.pose.translation[0] > left.pose.translation[0]);
    assert!(raised.pose.translation[1] > left.pose.translation[1]);
}
