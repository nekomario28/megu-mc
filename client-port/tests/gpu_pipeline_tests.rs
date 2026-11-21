// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::mem::{align_of, offset_of, size_of, size_of_val};

use mc_spine_client_mesh::gpu_pipeline::*;
use mc_spine_client_mesh::{mesh_section, Block, Direction, Light, Section, ALL_FACES, EDGE};

const DAY: Light = Light::new(0, 15);

// ---------------------------------------------------------------------------
// Hardware capability profiles (matches what the Vulkan layer would report).
// ---------------------------------------------------------------------------

fn nvidia_rtx_4090() -> GpuCapabilities {
    GpuCapabilities {
        vendor: GpuVendor::Nvidia,
        supports_ext_mesh_shader: true,
        max_mesh_output_vertices: 256,
        max_mesh_output_primitives: 512,
        supports_multi_draw_indirect_count: true,
        supports_compute_shaders: true,
    }
}

fn amd_rx_7900_xtx() -> GpuCapabilities {
    GpuCapabilities {
        vendor: GpuVendor::Amd,
        supports_ext_mesh_shader: true,
        max_mesh_output_vertices: 256,
        max_mesh_output_primitives: 256,
        supports_multi_draw_indirect_count: true,
        supports_compute_shaders: true,
    }
}

fn intel_arc_a770() -> GpuCapabilities {
    GpuCapabilities {
        vendor: GpuVendor::Intel,
        supports_ext_mesh_shader: true,
        max_mesh_output_vertices: 256,
        max_mesh_output_primitives: 256,
        supports_multi_draw_indirect_count: true,
        supports_compute_shaders: true,
    }
}

/// Apple silicon through Metal/MoltenVK: no VK_EXT_mesh_shader, but compute
/// and `vkCmdDrawIndexedIndirectCount` are available.
fn apple_m3() -> GpuCapabilities {
    GpuCapabilities {
        vendor: GpuVendor::Apple,
        supports_ext_mesh_shader: false,
        max_mesh_output_vertices: 0,
        max_mesh_output_primitives: 0,
        supports_multi_draw_indirect_count: true,
        supports_compute_shaders: true,
    }
}

/// Software rasterizer: no GPU culling features at all.
fn software_rasterizer() -> GpuCapabilities {
    GpuCapabilities {
        vendor: GpuVendor::Other,
        supports_ext_mesh_shader: false,
        max_mesh_output_vertices: 0,
        max_mesh_output_primitives: 0,
        supports_multi_draw_indirect_count: false,
        supports_compute_shaders: false,
    }
}

// ---------------------------------------------------------------------------
// Backend selection matrix
// ---------------------------------------------------------------------------

#[test]
fn auto_prefers_mesh_shader_on_nvidia_amd_and_intel() {
    for caps in [nvidia_rtx_4090(), amd_rx_7900_xtx(), intel_arc_a770()] {
        let resolution = resolve_render_backend(RenderBackendPreference::Auto, &caps);
        assert_eq!(
            resolution.backend,
            ActiveRenderBackend::MeshShader,
            "{caps:?}"
        );
        assert_eq!(resolution.fallback_reason, None, "{caps:?}");
    }
}

#[test]
fn auto_uses_compute_indirect_on_apple_silicon() {
    let resolution = resolve_render_backend(RenderBackendPreference::Auto, &apple_m3());
    assert_eq!(resolution.backend, ActiveRenderBackend::ComputeIndirect);
    assert_eq!(resolution.fallback_reason, None);
}

#[test]
fn auto_falls_back_to_classic_on_software() {
    let resolution = resolve_render_backend(RenderBackendPreference::Auto, &software_rasterizer());
    assert_eq!(resolution.backend, ActiveRenderBackend::Classic);
    assert!(resolution.fallback_reason.is_some());
}

#[test]
fn explicit_mesh_shader_request_falls_back_cleanly() {
    // Apple: ext missing -> ComputeIndirect (best remaining GPU path).
    let resolution = resolve_render_backend(RenderBackendPreference::MeshShader, &apple_m3());
    assert_eq!(resolution.backend, ActiveRenderBackend::ComputeIndirect);
    let reason = resolution.fallback_reason.unwrap();
    assert!(reason.contains("VK_EXT_mesh_shader"), "{reason}");

    // Software: nothing usable -> Classic, still no panic.
    let resolution =
        resolve_render_backend(RenderBackendPreference::MeshShader, &software_rasterizer());
    assert_eq!(resolution.backend, ActiveRenderBackend::Classic);
    assert!(resolution.fallback_reason.is_some());

    // Honored hardware resolves with no reason.
    let resolution =
        resolve_render_backend(RenderBackendPreference::MeshShader, &nvidia_rtx_4090());
    assert_eq!(resolution.backend, ActiveRenderBackend::MeshShader);
    assert_eq!(resolution.fallback_reason, None);
}

#[test]
fn mesh_shader_extension_with_low_limits_is_not_adequate() {
    // Extension present but a 128-vertex/64-primitive budget cannot emit a
    // 64-quad meshlet, so both Auto and the explicit request must degrade.
    let mut caps = nvidia_rtx_4090();
    caps.max_mesh_output_vertices = 128;
    caps.max_mesh_output_primitives = 64;
    assert!(!caps.mesh_shader_adequate());
    assert_eq!(
        resolve_render_backend(RenderBackendPreference::Auto, &caps).backend,
        ActiveRenderBackend::ComputeIndirect
    );
    let resolution = resolve_render_backend(RenderBackendPreference::MeshShader, &caps);
    assert_eq!(resolution.backend, ActiveRenderBackend::ComputeIndirect);
    assert!(resolution.fallback_reason.is_some());
}

#[test]
fn explicit_compute_indirect_request_falls_back_cleanly() {
    let resolution = resolve_render_backend(
        RenderBackendPreference::ComputeIndirect,
        &software_rasterizer(),
    );
    assert_eq!(resolution.backend, ActiveRenderBackend::Classic);
    assert!(resolution.fallback_reason.unwrap().contains("indirect"));

    // A mesh-shader-capable device missing indirect-count still gets a GPU path.
    let mut caps = nvidia_rtx_4090();
    caps.supports_multi_draw_indirect_count = false;
    let resolution = resolve_render_backend(RenderBackendPreference::ComputeIndirect, &caps);
    assert_eq!(resolution.backend, ActiveRenderBackend::MeshShader);
    assert!(resolution.fallback_reason.is_some());

    // Honored hardware resolves with no reason.
    let resolution =
        resolve_render_backend(RenderBackendPreference::ComputeIndirect, &nvidia_rtx_4090());
    assert_eq!(resolution.backend, ActiveRenderBackend::ComputeIndirect);
    assert_eq!(resolution.fallback_reason, None);
}

#[test]
fn explicit_classic_is_always_honored() {
    for caps in [nvidia_rtx_4090(), apple_m3(), software_rasterizer()] {
        let resolution = resolve_render_backend(RenderBackendPreference::Classic, &caps);
        assert_eq!(resolution.backend, ActiveRenderBackend::Classic);
        assert_eq!(resolution.fallback_reason, None);
    }
}

// ---------------------------------------------------------------------------
// 64-quad meshlet packer
// ---------------------------------------------------------------------------

/// A one-block slab of alternating-state cubes at each of y=0 and y=15 so the
/// mesher emits >64 unmergeable quads per face direction.
fn checkerboard_slabs() -> Section {
    let mut section = Section::filled(Block::AIR, DAY);
    for y in [0, EDGE - 1] {
        for x in 0..EDGE {
            for z in 0..EDGE {
                let state_id = 1 + ((x + z) % 2) as u32;
                section.set(x, y, z, Block::opaque_cube(state_id), DAY);
            }
        }
    }
    section
}

#[test]
fn meshlet_packing_conserves_every_quad_and_caps_at_64() {
    let quads = mesh_section(&checkerboard_slabs().view());
    // Both slabs expose two 16x16 checkerboard faces plus side strips: far
    // more than one meshlet's budget per direction.
    assert!(quads.len() > 2 * MAX_QUADS_PER_MESHLET);

    let packed = pack_section_meshlets(&quads);
    let repacked = pack_section_meshlets(&quads);
    assert_eq!(packed, repacked, "packing must be deterministic");

    assert_eq!(packed.quads.len(), quads.len());
    let conserved: u32 = packed.meshlets.iter().map(|m| m.quad_count).sum();
    assert_eq!(conserved as usize, quads.len());

    // Ranges partition the grouped buffer with no gaps or overlaps.
    let mut next = 0u32;
    for meshlet in &packed.meshlets {
        assert!(meshlet.quad_count >= 1);
        assert!(meshlet.quad_count as usize <= MAX_QUADS_PER_MESHLET);
        assert_eq!(meshlet.quad_offset, next);
        next += meshlet.quad_count;
    }
    assert_eq!(next as usize, quads.len());
}

#[test]
fn meshlets_are_direction_pure_with_matching_facing_mask() {
    let quads = mesh_section(&checkerboard_slabs().view());
    let packed = pack_section_meshlets(&quads);
    for meshlet in &packed.meshlets {
        let range =
            meshlet.quad_offset as usize..(meshlet.quad_offset + meshlet.quad_count) as usize;
        let direction = packed.quads[range.start].direction;
        assert!(packed.quads[range]
            .iter()
            .all(|quad| quad.direction == direction));
        assert_eq!(meshlet.facing_mask, direction.bit());
    }
}

#[test]
fn meshlet_aabb_tightly_covers_its_quads_quantized() {
    let quads = mesh_section(&checkerboard_slabs().view());
    let packed = pack_section_meshlets(&quads);
    for meshlet in &packed.meshlets {
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for quad in &packed.quads
            [meshlet.quad_offset as usize..(meshlet.quad_offset + meshlet.quad_count) as usize]
        {
            for position in &quad.positions {
                for axis in 0..3 {
                    min[axis] = min[axis].min(position[axis]);
                    max[axis] = max[axis].max(position[axis]);
                }
            }
        }
        for axis in 0..3 {
            assert_eq!(meshlet.aabb_min[axis] as f32, min[axis].floor());
            assert_eq!(meshlet.aabb_max[axis] as f32, max[axis].ceil());
        }
    }
}

#[test]
fn empty_section_meshes_to_zero_meshlets() {
    let quads = mesh_section(&Section::filled(Block::AIR, DAY).view());
    assert!(quads.is_empty());
    let packed = pack_section_meshlets(&quads);
    assert!(packed.quads.is_empty());
    assert!(packed.meshlets.is_empty());
    assert!(cull_meshlets_cpu_reference(&packed.meshlets, &[], [0.0; 3]).is_empty());
}

/// Solid section: six greedy-merged faces, one meshlet per direction.
fn solid_section_meshlets() -> SectionMeshlets {
    pack_section_meshlets(&mesh_section(
        &Section::filled(Block::opaque_cube(1), DAY).view(),
    ))
}

#[test]
fn cull_reference_drops_backfacing_meshlets() {
    let packed = solid_section_meshlets();
    assert_eq!(packed.meshlets.len(), 6);

    // Every plane trivially inside: dot(0,p)+0 >= 0 is not what we want, so
    // use six planes forming a huge enclosing box.
    let planes = [
        [1.0, 0.0, 0.0, 100.0],
        [-1.0, 0.0, 0.0, 100.0],
        [0.0, 1.0, 0.0, 100.0],
        [0.0, -1.0, 0.0, 100.0],
        [0.0, 0.0, 1.0, 100.0],
        [0.0, 0.0, -1.0, 100.0],
    ];

    // Camera above: only the Up meshlet's facing bit is lit.
    let visible = cull_meshlets_cpu_reference(&packed.meshlets, &planes, [8.0, 30.0, 8.0]);
    assert_eq!(visible.len(), 1);
    assert_eq!(
        packed.meshlets[visible[0] as usize].facing_mask,
        Direction::Up.bit()
    );

    // Camera on the far +x/+y/+z corner: Up, South, East survive.
    let visible = cull_meshlets_cpu_reference(&packed.meshlets, &planes, [30.0, 30.0, 30.0]);
    let masks: u8 = visible
        .iter()
        .map(|&i| packed.meshlets[i as usize].facing_mask)
        .sum();
    assert_eq!(visible.len(), 3);
    assert_eq!(
        masks,
        Direction::Up.bit() | Direction::South.bit() | Direction::East.bit()
    );

    // Camera inside the solid cube: every outward face is a backface.
    let visible = cull_meshlets_cpu_reference(&packed.meshlets, &planes, [8.0, 8.0, 8.0]);
    assert!(visible.is_empty());
}

#[test]
fn cull_reference_frustum_drops_meshlets_outside_any_plane() {
    let packed = solid_section_meshlets();
    // Half-space y <= 8: everything except the Down face is out.
    let planes = [[0.0, -1.0, 0.0, 8.0]];
    let visible = cull_meshlets_cpu_reference(&packed.meshlets, &planes, [8.0, -10.0, 8.0]);
    assert_eq!(visible.len(), 1);
    assert_eq!(
        packed.meshlets[visible[0] as usize].facing_mask,
        Direction::Down.bit()
    );

    // A plane nothing can satisfy culls the whole set.
    let planes = [[0.0, -1.0, 0.0, -100.0]];
    assert!(cull_meshlets_cpu_reference(&packed.meshlets, &planes, [8.0, 30.0, 8.0]).is_empty());
}

// ---------------------------------------------------------------------------
// KineticInstancePool
// ---------------------------------------------------------------------------

#[test]
fn gpu_records_match_the_declared_layout() {
    assert_eq!(size_of::<KineticInstanceGpu>(), 48);
    assert_eq!(align_of::<KineticInstanceGpu>(), 4);
    assert_eq!(offset_of!(KineticInstanceGpu, pos), 0);
    assert_eq!(offset_of!(KineticInstanceGpu, angular_velocity), 12);
    assert_eq!(offset_of!(KineticInstanceGpu, phase), 16);
    assert_eq!(offset_of!(KineticInstanceGpu, axis_and_flags), 20);
    assert_eq!(offset_of!(KineticInstanceGpu, light_packed), 24);
    assert_eq!(offset_of!(KineticInstanceGpu, tint_rgba), 28);
    assert_eq!(offset_of!(KineticInstanceGpu, scale_xyz), 32);
    assert_eq!(offset_of!(KineticInstanceGpu, mesh_kind), 44);
    assert_eq!(size_of::<DrawIndexedIndirectCommand>(), 20);
    assert_eq!(offset_of!(DrawIndexedIndirectCommand, first_instance), 16);
    assert_eq!(size_of::<SubWorldTransformGpu>(), 64);
    assert_eq!(size_of::<MeshletDescriptor>(), 16);
}

#[test]
fn axis_flags_pack_and_unpack() {
    let flags = pack_axis_flags(2, true, true, 7);
    let mut record = KineticInstanceGpu {
        axis_and_flags: flags,
        ..KineticInstanceGpu::default()
    };
    assert_eq!(record.axis(), 2);
    assert!(record.reversed());
    assert!(record.overstressed());
    assert_eq!(record.subworld_id(), 7);

    // Values above the field width are masked off.
    record.axis_and_flags = pack_axis_flags(5, false, false, KINETIC_SUBWORLD_MAX + 4);
    assert_eq!(record.axis(), 1);
    assert!(!record.reversed());
    assert_eq!(record.subworld_id(), 3);
}

#[test]
fn instance_angle_uses_the_vertex_shader_formula() {
    let record = KineticInstanceGpu {
        phase: 0.5,
        angular_velocity: 2.0,
        ..KineticInstanceGpu::default()
    };
    assert_eq!(KineticInstancePool::eval_instance_angle(&record, 3.0), 6.5);
    assert_eq!(KineticInstancePool::eval_instance_angle(&record, 0.0), 0.5);
}

fn kinetic_instance(seed: u64, mesh_kind: u32) -> KineticInstanceGpu {
    KineticInstanceGpu {
        pos: [seed as f32, 64.0, (seed % 97) as f32],
        angular_velocity: 1.5,
        phase: (seed % 31) as f32 * 0.1,
        axis_and_flags: pack_axis_flags((seed % 3) as u32, seed % 7 == 0, false, 0),
        light_packed: 0x00f0,
        tint_rgba: 0xffff_ffff,
        scale_xyz: [1.0; 3],
        mesh_kind,
    }
}

fn instance_bytes(instances: &[KineticInstanceGpu]) -> &[u8] {
    // SAFETY: KineticInstanceGpu is a plain repr(C) record; reading it as
    // bytes within its own slice is valid.
    unsafe { std::slice::from_raw_parts(instances.as_ptr().cast::<u8>(), size_of_val(instances)) }
}

#[test]
fn pool_emits_one_indirect_command_per_mesh_kind_at_scale() {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(0, 36, 0, 0); // shaft: 12 tris
    pool.set_mesh_template(1, 360, 36, 16); // cogwheel
    pool.set_mesh_template(2, 1200, 396, 160); // belt run

    // Interleave kinds so per-kind instance ids are non-contiguous.
    for id in 0..10_000u64 {
        let mesh_kind = (id % 3) as u32;
        pool.upsert(id, mesh_kind, kinetic_instance(id, mesh_kind));
    }
    assert_eq!(pool.len(), 10_000);

    let upload = pool.build_indirect_batches();
    assert_eq!(upload.commands.len(), 3);
    assert_eq!(upload.instances.len(), 10_000);

    let expected = [(0u32, 3334u32), (1, 3333), (2, 3333)];
    let mut first_instance = 0u32;
    for (command, &(kind, count)) in upload.commands.iter().zip(&expected) {
        let template = pool.mesh_template(kind).unwrap();
        assert_eq!(command.index_count, template.index_count);
        assert_eq!(command.first_index, template.first_index);
        assert_eq!(command.vertex_offset, template.vertex_offset);
        assert_eq!(command.instance_count, count);
        assert_eq!(command.first_instance, first_instance);
        // Every instance in this command's run belongs to the kind and the
        // run is ordered by ascending instance id (id == pos.x here).
        let run = &upload.instances
            [command.first_instance as usize..(command.first_instance + count) as usize];
        assert!(run.iter().all(|r| r.mesh_kind == kind));
        assert!(run.windows(2).all(|w| w[0].pos[0] < w[1].pos[0]));
        first_instance += count;
    }
}

#[test]
fn pool_batches_are_byte_identical_across_runs() {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(0, 36, 0, 0);
    pool.set_mesh_template(1, 360, 36, 16);
    for id in 0..500u64 {
        let mesh_kind = (id % 2) as u32;
        pool.upsert(id, mesh_kind, kinetic_instance(id, mesh_kind));
    }
    let first = pool.build_indirect_batches();
    let second = pool.build_indirect_batches();
    assert_eq!(
        instance_bytes(&first.instances),
        instance_bytes(&second.instances)
    );
    assert_eq!(first.commands, second.commands);

    // A fresh pool populated identically produces the same bytes.
    let mut clone = KineticInstancePool::new();
    clone.set_mesh_template(0, 36, 0, 0);
    clone.set_mesh_template(1, 360, 36, 16);
    for id in 0..500u64 {
        let mesh_kind = (id % 2) as u32;
        clone.upsert(id, mesh_kind, kinetic_instance(id, mesh_kind));
    }
    let rebuilt = clone.build_indirect_batches();
    assert_eq!(
        instance_bytes(&first.instances),
        instance_bytes(&rebuilt.instances)
    );
}

#[test]
fn removed_instances_leave_no_stale_records() {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(0, 36, 0, 0);
    for id in 0..100u64 {
        pool.upsert(id, 0, kinetic_instance(id, 0));
    }
    // Overwriting an id does not duplicate it.
    pool.upsert(7, 0, kinetic_instance(7, 0));
    assert_eq!(pool.len(), 100);

    for id in (0..100u64).step_by(10) {
        assert!(pool.remove(id));
    }
    assert_eq!(pool.len(), 90);
    assert!(pool.get(10).is_none());
    assert!(!pool.remove(10));

    let upload = pool.build_indirect_batches();
    assert_eq!(upload.instances.len(), 90);
    assert_eq!(upload.commands.len(), 1);
    assert_eq!(upload.commands[0].instance_count, 90);
    // pos.x carries the id: no removed id may survive in the SSBO.
    assert!(upload.instances.iter().all(|r| (r.pos[0] as u64) % 10 != 0));
}

#[test]
fn instances_without_a_template_are_not_uploaded() {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(1, 360, 36, 16);
    pool.upsert(1, 1, kinetic_instance(1, 1));
    pool.upsert(2, 9, kinetic_instance(2, 9)); // kind 9 has no template
    let upload = pool.build_indirect_batches();
    assert_eq!(upload.commands.len(), 1);
    assert_eq!(upload.instances.len(), 1);
    assert_eq!(upload.instances[0].mesh_kind, 1);
}

#[test]
fn subworld_table_is_dense_and_identity_padded() {
    let mut pool = KineticInstancePool::new();
    let ship = SubWorldTransformGpu {
        matrix: [
            0.0, -1.0, 0.0, 0.0, //
            1.0, 0.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            40.0, 80.0, -12.0, 1.0,
        ],
    };
    pool.set_subworld_transform(2, ship);
    pool.set_mesh_template(0, 36, 0, 0);
    let mut flying = kinetic_instance(1, 0);
    flying.axis_and_flags = pack_axis_flags(1, false, false, 2);
    pool.upsert(1, 0, flying);
    pool.upsert(2, 0, kinetic_instance(2, 0)); // static grid, subworld 0

    let upload = pool.build_indirect_batches();
    assert_eq!(upload.subworlds.len(), 3);
    assert_eq!(upload.subworlds[0], SubWorldTransformGpu::IDENTITY);
    assert_eq!(upload.subworlds[1], SubWorldTransformGpu::IDENTITY);
    assert_eq!(upload.subworlds[2], ship);
    assert_eq!(upload.instances[0].subworld_id(), 2);
    assert_eq!(upload.instances[1].subworld_id(), 0);

    assert!(pool.remove_subworld_transform(2));
    assert!(!pool.remove_subworld_transform(2));
    assert!(pool.build_indirect_batches().subworlds.is_empty());
}

#[test]
fn all_faces_mask_never_direction_culls_unassigned_meshlets() {
    // Sanity for the unassigned-bucket contract: ALL_FACES intersects every
    // non-empty camera-facing mask, so such meshlets can only be frustum
    // culled, matching `bucket_draw_mask`'s always-draw rule.
    let meshlet = MeshletDescriptor {
        aabb_min: [0, 0, 0],
        aabb_max: [16, 16, 16],
        facing_mask: ALL_FACES,
        flags: 0,
        quad_offset: 0,
        quad_count: 1,
    };
    for camera in [[30.0, 8.0, 8.0], [-4.0, 20.0, 8.0], [8.0, 8.0, 8.0]] {
        assert_eq!(
            cull_meshlets_cpu_reference(&[meshlet], &[], camera),
            vec![0],
            "{camera:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Lane G3: ClassicLowSpec backend, memory profiles & UBO kinetic batching
// ---------------------------------------------------------------------------

#[test]
fn classic_low_spec_is_honored_on_every_profile() {
    for caps in [
        GpuCapabilities::intel_hd_legacy(),
        GpuCapabilities::intel_uhd_630(),
        GpuCapabilities::intel_iris_xe(),
        GpuCapabilities::nvidia_rtx(),
        GpuCapabilities::amd_rdna2(),
        GpuCapabilities::apple_silicon(),
        GpuCapabilities::software(),
    ] {
        let resolution =
            resolve_render_backend(RenderBackendPreference::ClassicLowSpec, &caps);
        assert_eq!(
            resolution.backend,
            ActiveRenderBackend::ClassicLowSpec,
            "{caps:?}"
        );
        assert_eq!(resolution.fallback_reason, None, "{caps:?}");
    }
}

#[test]
fn legacy_intel_profiles_have_the_minimal_feature_floor() {
    for caps in [GpuCapabilities::intel_hd_legacy(), GpuCapabilities::intel_uhd_630()] {
        assert_eq!(caps.vendor, GpuVendor::Intel);
        assert!(!caps.supports_ext_mesh_shader);
        assert!(!caps.supports_multi_draw_indirect_count);
        assert!(!caps.supports_compute_shaders);
        assert!(!caps.mesh_shader_adequate());
        assert!(!caps.compute_indirect_adequate());
        // Auto negotiates down to Classic; ClassicLowSpec is opt-in only so
        // a featureless device never loses the plain-Classic escape hatch.
        let auto = resolve_render_backend(RenderBackendPreference::Auto, &caps);
        assert_eq!(auto.backend, ActiveRenderBackend::Classic);
        assert!(auto.fallback_reason.is_some());
        // Explicit GPU-path requests still degrade cleanly, never panic.
        for pref in [
            RenderBackendPreference::MeshShader,
            RenderBackendPreference::ComputeIndirect,
        ] {
            let resolution = resolve_render_backend(pref, &caps);
            assert_eq!(resolution.backend, ActiveRenderBackend::Classic, "{pref:?}");
            assert!(resolution.fallback_reason.is_some());
        }
    }
    // Iris Xe keeps a full mesh-shader budget unlike the legacy parts.
    let xe = GpuCapabilities::intel_iris_xe();
    assert!(xe.mesh_shader_adequate());
    assert_eq!(
        resolve_render_backend(RenderBackendPreference::Auto, &xe).backend,
        ActiveRenderBackend::MeshShader
    );
}

#[test]
fn backend_preference_tokens_roundtrip_and_fail_closed() {
    for pref in [
        RenderBackendPreference::Auto,
        RenderBackendPreference::MeshShader,
        RenderBackendPreference::ComputeIndirect,
        RenderBackendPreference::Classic,
        RenderBackendPreference::ClassicLowSpec,
    ] {
        assert_eq!(RenderBackendPreference::parse(pref.as_str()), Some(pref));
    }
    // Case-insensitive and `_`-tolerant so TOML files hand-edited with
    // underscores still resolve.
    assert_eq!(
        RenderBackendPreference::parse("Classic_Low_Spec"),
        Some(RenderBackendPreference::ClassicLowSpec)
    );
    assert_eq!(
        RenderBackendPreference::parse("MESH-SHADER"),
        Some(RenderBackendPreference::MeshShader)
    );
    for bad in ["", "vulkan", "low-spec", "classiclowspec", "gpu", "none"] {
        assert_eq!(RenderBackendPreference::parse(bad), None, "{bad}");
    }
}

#[test]
fn ubo_batch_size_fits_the_guaranteed_16kib_limit() {
    assert_eq!(KINETIC_INSTANCE_GPU_BYTES, 48);
    assert_eq!(MAX_INSTANCES_PER_UBO_16KB, 256);
    assert!(
        MAX_INSTANCES_PER_UBO_16KB * KINETIC_INSTANCE_GPU_BYTES as usize
            <= MIN_GUARANTEED_UBO_BYTES as usize
    );
    assert_eq!(
        256 * 48,
        12_288,
        "256 x 48 B records = 12,288 B <= 16,384 B UBO floor"
    );

    // The 16 KiB floor every profile constructor reports yields 256.
    assert_eq!(
        GpuMemoryProfile::integrated_shared(512).max_instances_per_ubo(),
        256
    );
    assert_eq!(GpuMemoryProfile::dedicated(8192).max_instances_per_ubo(), 256);
    // Smaller reported UBO limits shrink the batch; larger ones still cap
    // at 256.
    let mut mem = GpuMemoryProfile::dedicated(1024);
    mem.max_ubo_bytes = 8 * 1024;
    assert_eq!(mem.max_instances_per_ubo(), 8 * 1024 / 48);
    mem.max_ubo_bytes = 64 * 1024;
    assert_eq!(mem.max_instances_per_ubo(), 256);
}

fn pool_with_shafts(count: u64) -> KineticInstancePool {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(0, 36, 0, 0); // shaft: 12 tris
    for id in 0..count {
        pool.upsert(id, 0, kinetic_instance(id, 0));
    }
    pool
}

#[test]
fn ubo_batches_split_600_shafts_into_256_256_88() {
    let pool = pool_with_shafts(600);
    let batches = pool.build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB);
    assert_eq!(batches.len(), 3);
    assert_eq!(
        batches
            .iter()
            .map(|b| b.instances.len())
            .collect::<Vec<_>>(),
        vec![256, 256, 88]
    );

    // Exact conservation and ordered, gapless coverage of the packed stream.
    let conserved: usize = batches.iter().map(|b| b.instances.len()).sum();
    assert_eq!(conserved, pool.len());
    let mut first_instance = 0u32;
    for batch in &batches {
        assert_eq!(batch.mesh_kind, 0);
        assert_eq!(batch.index_count, 36);
        assert_eq!(batch.first_index, 0);
        assert_eq!(batch.vertex_offset, 0);
        assert_eq!(batch.first_instance, first_instance);
        assert!(batch.instances.iter().all(|r| r.mesh_kind == 0));
        assert!(batch
            .instances
            .windows(2)
            .all(|w| w[0].pos[0] < w[1].pos[0]));
        first_instance += batch.instances.len() as u32;
    }
    // Instance ids are still strictly ascending across batch boundaries.
    let ids: Vec<u64> = batches
        .iter()
        .flat_map(|b| b.instances.iter().map(|r| r.pos[0] as u64))
        .collect();
    assert_eq!(ids, (0..600).collect::<Vec<_>>());
}

#[test]
fn ubo_batches_match_the_indirect_stream_byte_for_byte() {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(0, 36, 0, 0);
    pool.set_mesh_template(1, 360, 36, 16);
    pool.set_mesh_template(2, 1200, 396, 160);
    for id in 0..900u64 {
        let mesh_kind = (id % 3) as u32;
        pool.upsert(id, mesh_kind, kinetic_instance(id, mesh_kind));
    }

    let indirect = pool.build_indirect_batches();
    let batches = pool.build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB);
    // 300 instances per kind -> 256 + 44 per kind, kinds ascending.
    assert_eq!(batches.len(), 6);
    assert_eq!(batches[0].mesh_kind, 0);
    assert_eq!(batches[2].mesh_kind, 1);
    assert_eq!(batches[4].mesh_kind, 2);
    assert_eq!(batches[1].instances.len(), 44);

    let ubo_stream: Vec<KineticInstanceGpu> = batches
        .iter()
        .flat_map(|b| b.instances.iter().copied())
        .collect();
    assert_eq!(
        instance_bytes(&ubo_stream),
        instance_bytes(&indirect.instances),
        "UBO batches must conserve the exact indirect instance bytes"
    );
    // Every batch carries its kind's template geometry run.
    for batch in &batches {
        let template = pool.mesh_template(batch.mesh_kind).unwrap();
        assert_eq!(batch.index_count, template.index_count);
        assert_eq!(batch.first_index, template.first_index);
        assert_eq!(batch.vertex_offset, template.vertex_offset);
    }

    // Byte-identical determinism across repeated and fresh builds.
    let again = pool.build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB);
    assert_eq!(batches, again);
    let mut clone = KineticInstancePool::new();
    clone.set_mesh_template(0, 36, 0, 0);
    clone.set_mesh_template(1, 360, 36, 16);
    clone.set_mesh_template(2, 1200, 396, 160);
    for id in 0..900u64 {
        let mesh_kind = (id % 3) as u32;
        clone.upsert(id, mesh_kind, kinetic_instance(id, mesh_kind));
    }
    let rebuilt = clone.build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB);
    assert_eq!(batches, rebuilt);
}

#[test]
fn ubo_batches_skip_untemplated_kinds_and_clamp_zero_budget() {
    let mut pool = KineticInstancePool::new();
    pool.set_mesh_template(1, 360, 36, 16);
    pool.upsert(1, 1, kinetic_instance(1, 1));
    pool.upsert(2, 9, kinetic_instance(2, 9)); // kind 9 has no template
    pool.upsert(3, 1, kinetic_instance(3, 1));

    let batches = pool.build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].instances.len(), 2);
    assert!(batches[0].instances.iter().all(|r| r.mesh_kind == 1));

    // A zero budget is clamped to one instance per UBO, never a panic or
    // an infinite split.
    let singletons = pool.build_ubo_batches(0);
    assert_eq!(singletons.len(), 2);
    assert!(singletons.iter().all(|b| b.instances.len() == 1));

    // A budget larger than the run still emits one batch per kind.
    assert_eq!(pool.build_ubo_batches(usize::MAX).len(), 1);

    // Empty pool -> no batches, matching the empty indirect upload.
    assert!(KineticInstancePool::new()
        .build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB)
        .is_empty());
}

#[test]
fn low_spec_budget_tightens_for_shared_ram_intel() {
    // Intel UHD 630 reporting shared RAM: the low-spec budget caps dirty
    // rebuilds at 2/frame, keeps a short full-detail ring inside 6..=8,
    // pairs it with a far horizon LOD, and requests UBO kinetic batches.
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::integrated_shared(512),
        ActiveRenderBackend::ClassicLowSpec,
    );
    assert_eq!(budget.max_dirty_rebuilds_per_frame, 2);
    assert!((6..=8).contains(&budget.recommended_full_detail_chunks));
    assert!(budget.recommended_lod_chunks >= 24);
    assert!(budget.use_ubo_kinetic_batches);
    assert_eq!(budget.max_mesh_cache_mib, 32);

    // Shared RAM is low-spec even at larger reported sizes; the
    // full-detail ring scales with reported memory inside 6..=8.
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::integrated_shared(0),
        ActiveRenderBackend::Classic,
    );
    assert_eq!(budget.recommended_full_detail_chunks, 6);
    assert_eq!(budget.max_dirty_rebuilds_per_frame, 2);
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::integrated_shared(2048),
        ActiveRenderBackend::ComputeIndirect,
    );
    assert_eq!(budget.recommended_full_detail_chunks, 8);
    assert!(budget.use_ubo_kinetic_batches);

    // A dedicated <=512 MiB card is low-spec without the shared-RAM flag.
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::dedicated(512),
        ActiveRenderBackend::Classic,
    );
    assert_eq!(budget.max_dirty_rebuilds_per_frame, 2);
    assert_eq!(budget.recommended_full_detail_chunks, 8);
    assert_eq!(budget.max_mesh_cache_mib, 64);
    assert!(budget.use_ubo_kinetic_batches);

    // An explicit ClassicLowSpec choice always yields the low-spec budget,
    // even on a card that would not qualify on memory alone.
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::dedicated(8192),
        ActiveRenderBackend::ClassicLowSpec,
    );
    assert_eq!(budget.max_dirty_rebuilds_per_frame, 2);
    assert!(budget.use_ubo_kinetic_batches);
}

#[test]
fn low_spec_budget_stays_generous_on_dedicated_gpus() {
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::dedicated(8192),
        ActiveRenderBackend::MeshShader,
    );
    assert_eq!(budget.max_dirty_rebuilds_per_frame, u32::MAX);
    assert_eq!(budget.max_mesh_cache_mib, 1024);
    assert_eq!(budget.recommended_full_detail_chunks, 12);
    assert_eq!(budget.recommended_lod_chunks, 64);
    assert!(!budget.use_ubo_kinetic_batches);

    // Mesh-cache budget scales with reported VRAM inside 128..=1024 MiB.
    let budget = recommend_low_spec_budget(
        &GpuMemoryProfile::dedicated(1024),
        ActiveRenderBackend::ComputeIndirect,
    );
    assert_eq!(budget.max_mesh_cache_mib, 256);
    assert!(!budget.use_ubo_kinetic_batches);
}

// ---------------------------------------------------------------------------
// Lane PERF-3: BlockEntityInstancePool (storage-room instancing)
// ---------------------------------------------------------------------------

#[test]
fn block_entity_records_match_the_declared_layout() {
    assert_eq!(size_of::<BlockEntityInstanceGpu>(), 48);
    assert_eq!(align_of::<BlockEntityInstanceGpu>(), 4);
    assert_eq!(BLOCK_ENTITY_INSTANCE_GPU_BYTES, 48);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, pos), 0);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, lid_or_anim_rad), 12);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, facing_and_flags), 16);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, light_packed), 20);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, tint_rgba), 24);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, scale_xyz), 28);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, mesh_kind), 40);
    assert_eq!(offset_of!(BlockEntityInstanceGpu, lod_flags), 44);
    // Same stride as the kinetic pool, so both share the 16 KiB UBO floor.
    assert_eq!(BLOCK_ENTITY_INSTANCE_GPU_BYTES, KINETIC_INSTANCE_GPU_BYTES);
    assert!(
        MAX_INSTANCES_PER_UBO_16KB * BLOCK_ENTITY_INSTANCE_GPU_BYTES as usize
            <= MIN_GUARANTEED_UBO_BYTES as usize
    );
}

#[test]
fn block_entity_mesh_kind_ids_are_stable_and_collision_free() {
    let named = [
        BlockEntityMeshKind::ChestSingle,
        BlockEntityMeshKind::ChestDoubleLeft,
        BlockEntityMeshKind::ChestDoubleRight,
        BlockEntityMeshKind::EnderChest,
        BlockEntityMeshKind::ShulkerBox,
        BlockEntityMeshKind::ItemFrame,
        BlockEntityMeshKind::GlowItemFrame,
        BlockEntityMeshKind::StandingSign,
        BlockEntityMeshKind::WallSign,
        BlockEntityMeshKind::HangingSign,
        BlockEntityMeshKind::BedHead,
        BlockEntityMeshKind::BedFoot,
        BlockEntityMeshKind::Banner,
        BlockEntityMeshKind::Bell,
        BlockEntityMeshKind::Hopper,
    ];
    assert_eq!(named.len(), 15);
    for (expected, kind) in named.iter().enumerate() {
        assert_eq!(kind.to_mesh_kind_id(), expected as u32, "{kind:?}");
    }
    // Custom kinds live strictly above the reserved id 15.
    assert_eq!(
        BlockEntityMeshKind::Custom(0).to_mesh_kind_id(),
        BLOCK_ENTITY_MESH_KIND_CUSTOM_BASE
    );
    assert_eq!(
        BlockEntityMeshKind::Custom(41).to_mesh_kind_id(),
        BLOCK_ENTITY_MESH_KIND_CUSTOM_BASE + 41
    );
    assert_eq!(
        BlockEntityMeshKind::Custom(u32::MAX).to_mesh_kind_id(),
        u32::MAX,
        "custom ids saturate instead of wrapping into the built-in range"
    );
    // No named kind can collide with a custom id.
    assert!(named
        .iter()
        .all(|kind| kind.to_mesh_kind_id() < BLOCK_ENTITY_MESH_KIND_CUSTOM_BASE));
}

#[test]
fn block_entity_flags_pack_and_unpack() {
    let flags = pack_facing_and_flags(9, 14, true, false);
    let mut record = BlockEntityInstanceGpu {
        facing_and_flags: flags,
        ..BlockEntityInstanceGpu::default()
    };
    assert_eq!(record.facing_steps(), 9);
    assert_eq!(record.dye_color(), 14);
    assert!(record.glowing());
    assert!(!record.wall_mounted());
    assert!(record.cullable());

    // Values above the field width are masked off.
    record.facing_and_flags = pack_facing_and_flags(37, 19, false, true);
    assert_eq!(record.facing_steps(), 37 & BLOCK_ENTITY_FACING_MASK);
    assert_eq!(record.dye_color(), 19 & BLOCK_ENTITY_DYE_MASK);
    assert!(!record.glowing());
    assert!(record.wall_mounted());

    record.lod_flags = BLOCK_ENTITY_LOD_FLAG_NO_CULL;
    assert!(!record.cullable());
}

fn block_entity_instance(seed: u64) -> BlockEntityInstanceGpu {
    BlockEntityInstanceGpu {
        // A storage-room grid: ~190 x ~100 blocks of block entities.
        pos: [
            (seed % 97) as f32 * 2.0,
            64.0 + (seed % 4) as f32,
            (seed % 53) as f32 * 2.0,
        ],
        lid_or_anim_rad: (seed % 17) as f32 * 0.1,
        facing_and_flags: pack_facing_and_flags(
            (seed % 16) as u32,
            (seed % 16) as u32,
            false,
            false,
        ),
        light_packed: 0x00ff,
        tint_rgba: 0xffff_ffff,
        scale_xyz: [1.0; 3],
        mesh_kind: 0,
        lod_flags: 0,
    }
}

fn block_entity_at(x: f32, y: f32, z: f32) -> BlockEntityInstanceGpu {
    BlockEntityInstanceGpu {
        pos: [x, y, z],
        scale_xyz: [1.0; 3],
        tint_rgba: 0xffff_ffff,
        ..block_entity_instance(0)
    }
}

fn be_instance_bytes(instances: &[BlockEntityInstanceGpu]) -> &[u8] {
    // SAFETY: BlockEntityInstanceGpu is a plain repr(C) record; reading it
    // as bytes within its own slice is valid.
    unsafe { std::slice::from_raw_parts(instances.as_ptr().cast::<u8>(), size_of_val(instances)) }
}

/// The 1,000-chest / 500-frame / 300-sign storage room from the lane spec.
fn storage_room_pool() -> BlockEntityInstancePool {
    let mut pool = BlockEntityInstancePool::new();
    pool.set_mesh_template(BlockEntityMeshKind::ChestSingle, 54, 0, 0);
    pool.set_mesh_template(BlockEntityMeshKind::ItemFrame, 12, 54, 24);
    pool.set_mesh_template(BlockEntityMeshKind::StandingSign, 24, 66, 48);
    for id in 0..1_000u64 {
        pool.upsert(
            id,
            BlockEntityMeshKind::ChestSingle,
            block_entity_instance(id),
        );
    }
    for id in 1_000..1_500u64 {
        pool.upsert(id, BlockEntityMeshKind::ItemFrame, block_entity_instance(id));
    }
    for id in 1_500..1_800u64 {
        pool.upsert(
            id,
            BlockEntityMeshKind::StandingSign,
            block_entity_instance(id),
        );
    }
    pool
}

const STORAGE_ROOM_CAMERA: [f32; 3] = [96.0, 66.0, 52.0];

#[test]
fn storage_room_emits_one_indirect_command_per_mesh_kind() {
    let pool = storage_room_pool();
    assert_eq!(pool.len(), 1_800);

    let upload = pool.build_indirect_batches_culled(
        STORAGE_ROOM_CAMERA,
        f32::INFINITY,
        &[],
    );
    // 1,800 individual block-entity draws collapse to exactly 3 indirect
    // commands — one per active mesh kind, ascending by mesh-kind id
    // (ChestSingle=0, ItemFrame=5, StandingSign=7).
    assert_eq!(upload.commands.len(), 3);
    assert_eq!(upload.instances.len(), 1_800);
    let expected = [
        (BlockEntityMeshKind::ChestSingle, 1_000u32),
        (BlockEntityMeshKind::ItemFrame, 500),
        (BlockEntityMeshKind::StandingSign, 300),
    ];
    let mut first_instance = 0u32;
    for (command, &(kind, count)) in upload.commands.iter().zip(&expected) {
        let template = pool.mesh_template(kind).unwrap();
        assert_eq!(command.index_count, template.index_count);
        assert_eq!(command.first_index, template.first_index);
        assert_eq!(command.vertex_offset, template.vertex_offset);
        assert_eq!(command.instance_count, count);
        assert_eq!(command.first_instance, first_instance);
        // Every instance in the run belongs to the kind and runs fill in
        // ascending instance-id order (id == upsert order == seed order).
        let run = &upload.instances
            [command.first_instance as usize..(command.first_instance + count) as usize];
        assert!(run.iter().all(|r| r.mesh_kind == kind.to_mesh_kind_id()));
        first_instance += count;
    }
    assert_eq!(
        upload.stats,
        BlockEntityBatchStats {
            total_registered: 1_800,
            culled_distance: 0,
            culled_frustum: 0,
            uploaded_instances: 1_800,
            draw_calls: 3,
        }
    );

    // Determinism: the unculled build and a repeat culled build are
    // byte-identical.
    let unculled = pool.build_indirect_batches();
    assert_eq!(unculled, upload);
    let repeat = pool.build_indirect_batches_culled(STORAGE_ROOM_CAMERA, f32::INFINITY, &[]);
    assert_eq!(
        be_instance_bytes(&upload.instances),
        be_instance_bytes(&repeat.instances)
    );
}

#[test]
fn storage_room_ubo_batches_split_1000_chests_into_256_256_256_232() {
    // Chests only: the ClassicLowSpec 16 KiB UBO path must split 1,000
    // 48 B records into 256 + 256 + 256 + 232.
    let mut pool = BlockEntityInstancePool::new();
    pool.set_mesh_template(BlockEntityMeshKind::ChestSingle, 54, 0, 0);
    for id in 0..1_000u64 {
        pool.upsert(
            id,
            BlockEntityMeshKind::ChestSingle,
            block_entity_instance(id),
        );
    }

    let frame = pool.build_ubo_batches_culled(
        STORAGE_ROOM_CAMERA,
        f32::INFINITY,
        &[],
        MAX_INSTANCES_PER_UBO_16KB,
    );
    assert_eq!(frame.batches.len(), 4);
    assert_eq!(
        frame
            .batches
            .iter()
            .map(|b| b.instances.len())
            .collect::<Vec<_>>(),
        vec![256, 256, 256, 232]
    );
    assert_eq!(
        frame.stats,
        BlockEntityBatchStats {
            total_registered: 1_000,
            culled_distance: 0,
            culled_frustum: 0,
            uploaded_instances: 1_000,
            draw_calls: 4,
        }
    );

    // Exact conservation and gapless, ascending coverage of the stream.
    let conserved: usize = frame.batches.iter().map(|b| b.instances.len()).sum();
    assert_eq!(conserved, 1_000);
    let mut first_instance = 0u32;
    for batch in &frame.batches {
        assert_eq!(batch.mesh_kind, BlockEntityMeshKind::ChestSingle.to_mesh_kind_id());
        assert_eq!(batch.index_count, 54);
        assert_eq!(batch.first_index, 0);
        assert_eq!(batch.vertex_offset, 0);
        assert_eq!(batch.first_instance, first_instance);
        first_instance += batch.instances.len() as u32;
    }
    // The unculled shorthand emits the same batch list.
    assert_eq!(
        pool.build_ubo_batches(MAX_INSTANCES_PER_UBO_16KB),
        frame.batches
    );
}

#[test]
fn block_entity_ubo_stream_is_byte_identical_to_indirect_stream() {
    let pool = storage_room_pool();
    let indirect =
        pool.build_indirect_batches_culled(STORAGE_ROOM_CAMERA, f32::INFINITY, &[]);
    let frame = pool.build_ubo_batches_culled(
        STORAGE_ROOM_CAMERA,
        f32::INFINITY,
        &[],
        MAX_INSTANCES_PER_UBO_16KB,
    );

    // 1,000 chests -> 4, 500 frames -> 256+244 = 2, 300 signs -> 256+44 = 2.
    assert_eq!(frame.batches.len(), 8);
    assert_eq!(
        frame
            .batches
            .iter()
            .map(|b| b.mesh_kind)
            .collect::<Vec<_>>(),
        vec![0, 0, 0, 0, 5, 5, 7, 7]
    );
    assert_eq!(frame.batches[5].instances.len(), 244);
    assert_eq!(frame.batches[7].instances.len(), 44);
    assert_eq!(frame.stats.uploaded_instances, 1_800);
    assert_eq!(frame.stats.draw_calls, 8);

    let ubo_stream: Vec<BlockEntityInstanceGpu> = frame
        .batches
        .iter()
        .flat_map(|b| b.instances.iter().copied())
        .collect();
    assert_eq!(
        be_instance_bytes(&ubo_stream),
        be_instance_bytes(&indirect.instances),
        "UBO batches must conserve the exact indirect instance bytes"
    );
    // Every batch carries its kind's template geometry run (the values
    // storage_room_pool registered: chest 54/0/0, frame 12/54/24,
    // sign 24/66/48).
    for batch in &frame.batches {
        let (index_count, first_index, vertex_offset) = match batch.mesh_kind {
            0 => (54, 0, 0),
            5 => (12, 54, 24),
            7 => (24, 66, 48),
            other => panic!("unexpected mesh_kind {other}"),
        };
        assert_eq!(batch.index_count, index_count);
        assert_eq!(batch.first_index, first_index);
        assert_eq!(batch.vertex_offset, vertex_offset);
    }
}

#[test]
fn block_entity_culling_drops_far_and_offscreen_instances() {
    let mut pool = BlockEntityInstancePool::new();
    pool.set_mesh_template(BlockEntityMeshKind::ItemFrame, 12, 0, 0);
    pool.set_mesh_template(BlockEntityMeshKind::ChestSingle, 54, 12, 8);
    // Camera at the origin looking toward +z; the z >= 0 plane is the only
    // frustum boundary.
    let camera = [0.0, 64.0, 0.0];
    let planes = [[0.0, 0.0, 1.0, 0.0]];
    pool.upsert(0, BlockEntityMeshKind::ItemFrame, block_entity_at(4.0, 64.0, 10.0));
    pool.upsert(1, BlockEntityMeshKind::ItemFrame, block_entity_at(200.0, 64.0, 0.0));
    pool.upsert(2, BlockEntityMeshKind::ItemFrame, block_entity_at(4.0, 64.0, -50.0));
    pool.upsert(3, BlockEntityMeshKind::ChestSingle, block_entity_at(2.0, 64.0, 5.0));

    // 64-block entity render distance.
    let upload = pool.build_indirect_batches_culled(camera, 64.0 * 64.0, &planes);
    assert_eq!(
        upload.stats,
        BlockEntityBatchStats {
            total_registered: 4,
            culled_distance: 1, // id 1, 200 blocks out
            culled_frustum: 1,  // id 2, inside range but fully behind z = 0
            uploaded_instances: 2,
            draw_calls: 2,
        }
    );
    assert_eq!(upload.commands.len(), 2);
    // The survivors are exactly the near frame and the near chest, still
    // grouped one command per mesh kind (ChestSingle=0 first).
    assert_eq!(upload.instances.len(), 2);
    assert_eq!(upload.instances[0].pos, [2.0, 64.0, 5.0]);
    assert_eq!(upload.instances[0].mesh_kind, 0);
    assert_eq!(upload.instances[1].pos, [4.0, 64.0, 10.0]);
    assert_eq!(upload.instances[1].mesh_kind, 5);

    // Distance culling alone (no planes) drops only the 200-block frame.
    let upload = pool.build_indirect_batches_culled(camera, 64.0 * 64.0, &[]);
    assert_eq!(upload.stats.culled_distance, 1);
    assert_eq!(upload.stats.culled_frustum, 0);
    assert_eq!(upload.stats.uploaded_instances, 3);
}

#[test]
fn block_entity_no_cull_flag_survives_distance_and_frustum() {
    let mut pool = BlockEntityInstancePool::new();
    pool.set_mesh_template(BlockEntityMeshKind::ItemFrame, 12, 0, 0);
    let mut far = block_entity_at(500.0, 64.0, -500.0);
    far.lod_flags = BLOCK_ENTITY_LOD_FLAG_NO_CULL;
    pool.upsert(0, BlockEntityMeshKind::ItemFrame, far);
    pool.upsert(1, BlockEntityMeshKind::ItemFrame, block_entity_at(4.0, 64.0, 10.0));

    let upload = pool.build_indirect_batches_culled(
        [0.0, 64.0, 0.0],
        64.0 * 64.0,
        &[[0.0, 0.0, 1.0, 0.0]],
    );
    // The flagged frame skips both tests despite being 500+ blocks away
    // and fully behind the camera plane.
    assert_eq!(upload.stats.culled_distance, 0);
    assert_eq!(upload.stats.culled_frustum, 0);
    assert_eq!(upload.stats.uploaded_instances, 2);
    assert_eq!(upload.instances[0].pos, [500.0, 64.0, -500.0]);
}

#[test]
fn chest_lid_angle_updates_in_place_without_rebatching() {
    let mut pool = BlockEntityInstancePool::new();
    pool.set_mesh_template(BlockEntityMeshKind::ChestSingle, 54, 0, 0);
    for id in 0..10u64 {
        pool.upsert(
            id,
            BlockEntityMeshKind::ChestSingle,
            block_entity_instance(id),
        );
    }
    let before = pool.build_indirect_batches();

    // A lid-open tick rewrites the live record — no chunk mesh rebuild,
    // no pool growth, no command-buffer change.
    let mut record = *pool.get(5).unwrap();
    record.lid_or_anim_rad = std::f32::consts::FRAC_PI_2;
    pool.upsert(5, BlockEntityMeshKind::ChestSingle, record);
    assert_eq!(pool.len(), 10);

    let after = pool.build_indirect_batches();
    assert_eq!(after.commands, before.commands);
    assert_eq!(after.instances.len(), before.instances.len());
    // Same-kind instances pack in id order, so slot 5 is id 5: only its
    // lid bytes changed.
    assert_eq!(
        after.instances[5].lid_or_anim_rad,
        std::f32::consts::FRAC_PI_2
    );
    for i in 0..10usize {
        if i != 5 {
            assert_eq!(after.instances[i], before.instances[i], "slot {i}");
        }
    }
}

#[test]
fn untemplated_block_entity_kinds_are_not_uploaded() {
    let mut pool = BlockEntityInstancePool::new();
    pool.set_mesh_template(BlockEntityMeshKind::ChestSingle, 54, 0, 0);
    pool.upsert(0, BlockEntityMeshKind::ChestSingle, block_entity_at(1.0, 64.0, 1.0));
    pool.upsert(1, BlockEntityMeshKind::Custom(3), block_entity_at(2.0, 64.0, 1.0));

    let upload = pool.build_indirect_batches();
    assert_eq!(upload.stats.total_registered, 2);
    assert_eq!(upload.stats.uploaded_instances, 1);
    assert_eq!(upload.commands.len(), 1);
    assert_eq!(upload.instances[0].mesh_kind, 0);

    // Kind mismatch is authoritative: upserting a record built for another
    // kind still lands it in the requested kind's run.
    let mut wrong_kind = block_entity_at(3.0, 64.0, 1.0);
    wrong_kind.mesh_kind = 99;
    pool.upsert(2, BlockEntityMeshKind::ChestSingle, wrong_kind);
    let upload = pool.build_indirect_batches();
    assert_eq!(upload.stats.uploaded_instances, 2);
    assert_eq!(upload.instances[1].mesh_kind, 0);

    assert!(pool.remove(1));
    assert!(!pool.remove(1));
    assert_eq!(pool.build_indirect_batches().stats.total_registered, 2);
}
