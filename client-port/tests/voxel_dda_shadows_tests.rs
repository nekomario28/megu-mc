// SPDX-License-Identifier: LGPL-3.0-or-later
//! `VoxelOccupancyGrid` bit packing, the CPU voxel DDA shadow reference and
//! the `VOXEL_DDA_SHADOW_WGSL` kernel contract.

use megu_client_mesh::voxel_dda_shadows::{
    voxel_shadow_factor, voxel_shadow_factor_world, VoxelOccupancyGrid, VoxelShadowParams,
    MAX_DDA_STEPS, VOXEL_DDA_SHADOW_WGSL, VOXEL_GRID_BYTES, VOXEL_GRID_CELLS, VOXEL_GRID_DIM,
    VOXEL_GRID_WORDS,
};

// ---------------------------------------------------------------------------
// Occupancy grid packing
// ---------------------------------------------------------------------------

#[test]
fn grid_is_exactly_32_kib_of_u32_words() {
    assert_eq!(VOXEL_GRID_DIM, 64);
    assert_eq!(VOXEL_GRID_CELLS, 64 * 64 * 64);
    assert_eq!(VOXEL_GRID_WORDS, 8_192);
    assert_eq!(VOXEL_GRID_BYTES, 32 * 1024);
    let grid = VoxelOccupancyGrid::new();
    assert_eq!(grid.words().len(), VOXEL_GRID_WORDS);
    assert_eq!(std::mem::size_of_val(grid.words()), VOXEL_GRID_BYTES);
}

#[test]
fn bit_packing_uses_x_fastest_y_major_index() {
    let mut grid = VoxelOccupancyGrid::new();

    // Cell (0,0,0) -> idx 0 -> word 0 bit 0.
    grid.set_opaque(0, 0, 0, true);
    assert_eq!(grid.words()[0] & 1, 1);

    // Cell (1,0,0) -> idx 1 -> word 0 bit 1.
    grid.set_opaque(1, 0, 0, true);
    assert_eq!(grid.words()[0] & 0b11, 0b11);

    // Cell (0,0,1) -> idx 64 -> word 2 bit 0.
    grid.set_opaque(0, 0, 1, true);
    assert_eq!(grid.words()[2] & 1, 1);

    // Cell (0,1,0) -> idx 4096 -> word 128 bit 0.
    grid.set_opaque(0, 1, 0, true);
    assert_eq!(grid.words()[128] & 1, 1);

    // Cell (63,63,63) -> idx 262143 -> word 8191 bit 31.
    grid.set_opaque(63, 63, 63, true);
    assert_eq!(grid.words()[8191] >> 31, 1);
    assert!(grid.is_opaque(63, 63, 63));

    // Clearing a bit leaves neighbours untouched.
    grid.set_opaque(1, 0, 0, false);
    assert!(grid.is_opaque(0, 0, 0));
    assert!(!grid.is_opaque(1, 0, 0));
}

#[test]
fn out_of_bounds_access_never_panics() {
    let mut grid = VoxelOccupancyGrid::new();
    grid.set_opaque(64, 0, 0, true);
    grid.set_opaque(usize::MAX, usize::MAX, usize::MAX, true);
    assert!(!grid.is_opaque(64, 0, 0));
    assert!(!grid.is_opaque(usize::MAX, 0, 0));

    // World-space accessors reject cells outside the anchored volume.
    let mut grid = VoxelOccupancyGrid::with_origin([100, 0, -50]);
    grid.set_opaque_block([99, 0, -50], true); // one block west of origin
    grid.set_opaque_block([132, 32, -18], true); // local (32,32,32)
    assert!(!grid.is_opaque_block([99, 0, -50]));
    assert!(grid.is_opaque(32, 32, 32));
    assert!(grid.is_opaque_block([132, 32, -18]));

    // from_words validates the exact word count.
    assert!(VoxelOccupancyGrid::from_words([0; 3], vec![0; 8191]).is_none());
    assert!(VoxelOccupancyGrid::from_words([0; 3], vec![0; 8193]).is_none());
    assert!(VoxelOccupancyGrid::from_words([0; 3], vec![0; 8192]).is_some());
}

// ---------------------------------------------------------------------------
// CPU DDA reference evaluator
// ---------------------------------------------------------------------------

fn params(radius: f32) -> VoxelShadowParams {
    VoxelShadowParams {
        light_radius: radius,
        origin_bias: 0.0,
        max_steps: MAX_DDA_STEPS,
    }
}

#[test]
fn open_room_ray_is_fully_lit() {
    let grid = VoxelOccupancyGrid::new(); // all air
    let s = voxel_shadow_factor(
        &grid,
        [8.5, 8.5, 8.5],
        [40.5, 50.5, 20.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 1.0);
}

#[test]
fn opaque_pillar_between_point_and_torch_fully_occludes() {
    let mut grid = VoxelOccupancyGrid::new();
    // A 3x3 pillar of opaque blocks dead between the point and the torch.
    for y in 30..35 {
        for z in 30..35 {
            grid.set_opaque(32, y, z, true);
        }
    }
    let s = voxel_shadow_factor(
        &grid,
        [20.5, 32.5, 32.5],
        [40.5, 32.5, 32.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 0.0);
    // Even a soft torch is fully hidden: the 1-voxel chord through the pillar
    // beats the penumbra width w = 2*0.25*11.5/8.5 ~= 0.68.
    let s = voxel_shadow_factor(&grid, [20.5, 32.5, 32.5], [40.5, 32.5, 32.5], &params(0.25));
    assert_eq!(s, 0.0);
    // The same ray missing the pillar stays lit.
    let s = voxel_shadow_factor(
        &grid,
        [20.5, 20.5, 20.5],
        [40.5, 20.5, 20.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 1.0);
}

#[test]
fn diagonal_ray_through_opaque_corner_is_blocked() {
    let mut grid = VoxelOccupancyGrid::new();
    grid.set_opaque(32, 32, 32, true);
    // The exact main diagonal passes through cell (32,32,32) corner-to-corner.
    let s = voxel_shadow_factor(
        &grid,
        [24.0, 24.0, 24.0],
        [40.0, 40.0, 40.0],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 0.0);
    // A parallel diagonal offset one cell aside never enters the voxel.
    let s = voxel_shadow_factor(
        &grid,
        [24.0, 24.0, 25.5],
        [40.0, 40.0, 41.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 1.0);
}

#[test]
fn corner_grazing_ray_gets_partial_soft_shadow() {
    let mut grid = VoxelOccupancyGrid::new();
    grid.set_opaque(32, 32, 32, true);
    // Ray enters cell (32,32,32) through x=32 near y=33 and leaves through
    // the y=33 face ~0.31 voxels later — a short chord, i.e. a graze.
    let from = [31.0, 32.782_03, 32.5];
    let to = [34.0, 33.276_09, 32.5];
    // Hard shadow: any opaque contact is still fully dark.
    let s = voxel_shadow_factor(&grid, from, to, &VoxelShadowParams::HARD);
    assert_eq!(s, 0.0);
    // Soft light radius 0.5: penumbra w ~ 0.5 > chord ~0.31 -> partial.
    let soft = voxel_shadow_factor(&grid, from, to, &params(0.5));
    assert!(
        soft > 0.05 && soft < 0.95,
        "corner graze should be penumbral, got {soft}"
    );
    // Larger light radius widens the penumbra -> strictly lighter.
    let softer = voxel_shadow_factor(&grid, from, to, &params(2.0));
    assert!(softer > soft);
    assert!(softer <= 1.0);
    // Radius -> huge approaches fully lit.
    let huge = voxel_shadow_factor(&grid, from, to, &params(64.0));
    assert!(huge > softer);
}

#[test]
fn doorway_gap_ray_stays_lit_while_frame_blocks() {
    let mut grid = VoxelOccupancyGrid::new();
    // Wall plane x=32 with a 2-cell doorway at z in {32,33}, y in {32,33}.
    for y in 16..48 {
        for z in 16..48 {
            let in_door = (32..34).contains(&z) && (32..34).contains(&y);
            if !in_door {
                grid.set_opaque(32, y, z, true);
            }
        }
    }
    // Straight through the doorway.
    let s = voxel_shadow_factor(
        &grid,
        [28.5, 32.5, 32.5],
        [36.5, 32.5, 32.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 1.0);
    // Into the door frame one cell up.
    let s = voxel_shadow_factor(
        &grid,
        [28.5, 34.5, 32.5],
        [36.5, 34.5, 32.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 0.0);
}

#[test]
fn non_opaque_blocks_like_glass_cast_no_shadow() {
    let mut grid = VoxelOccupancyGrid::new();
    // A full plane of "glass": present in the world, but never marked opaque
    // in the bit grid, so it casts no shadow. Fill z-plane except one cell
    // that stays air, and leave a *glass* cell unset by definition.
    for y in 16..48 {
        for x in 16..48 {
            grid.set_opaque(x, y, 32, true);
        }
    }
    grid.set_opaque(32, 32, 32, false); // glass cell in the wall
    let s = voxel_shadow_factor(
        &grid,
        [32.5, 32.5, 28.5],
        [32.5, 32.5, 36.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 1.0); // through the glass pane
    let s = voxel_shadow_factor(
        &grid,
        [33.5, 32.5, 28.5],
        [33.5, 32.5, 36.5],
        &VoxelShadowParams::HARD,
    );
    assert_eq!(s, 0.0); // into the opaque wall one cell over
}

#[test]
fn degenerate_and_out_of_bounds_rays_never_panic_and_stay_lit() {
    let mut grid = VoxelOccupancyGrid::new();
    grid.set_opaque(32, 32, 32, true);
    let p = VoxelShadowParams::HARD;

    // Coincident origin == light.
    assert_eq!(
        voxel_shadow_factor(&grid, [10.0, 10.0, 10.0], [10.0, 10.0, 10.0], &p),
        1.0
    );
    // NaN / infinite inputs.
    assert_eq!(
        voxel_shadow_factor(&grid, [f32::NAN, 0.0, 0.0], [1.0, 1.0, 1.0], &p),
        1.0
    );
    assert_eq!(
        voxel_shadow_factor(
            &grid,
            [10.0, 10.0, 10.0],
            [f32::INFINITY, 0.0, 0.0],
            &p
        ),
        1.0
    );
    // Far outside the volume, pointing away and pointing through.
    assert_eq!(
        voxel_shadow_factor(&grid, [-500.0, 0.0, 0.0], [-1000.0, 0.0, 0.0], &p),
        1.0
    );
    assert_eq!(
        voxel_shadow_factor(&grid, [-500.0, 0.0, 0.0], [500.0, 0.0, 0.0], &p),
        1.0
    );
    // A ray entering the volume from outside still sees the blocker.
    let s = voxel_shadow_factor(&grid, [10.5, 32.5, 32.5], [32.5, 32.5, 32.5], &p);
    assert_eq!(s, 0.0);
    // Tiny step budget: bounded work, no panic, reports what it found.
    let p3 = VoxelShadowParams {
        light_radius: 0.0,
        origin_bias: 0.0,
        max_steps: 3,
    };
    let s = voxel_shadow_factor(&grid, [10.5, 32.5, 32.5], [32.5, 32.5, 32.5], &p3);
    assert_eq!(s, 1.0); // occluder beyond the budget stays unfound
}

#[test]
fn light_inside_opaque_voxel_is_dark_and_origin_inside_is_dark() {
    let mut grid = VoxelOccupancyGrid::new();
    grid.set_opaque(32, 32, 32, true);
    let p = VoxelShadowParams::HARD;
    // Light embedded in the block.
    assert_eq!(
        voxel_shadow_factor(&grid, [20.5, 32.5, 32.5], [32.5, 32.5, 32.5], &p),
        0.0
    );
    // Shaded point inside the block (bias 0): shadowed by its own cell.
    assert_eq!(
        voxel_shadow_factor(&grid, [32.5, 32.5, 32.5], [20.5, 32.5, 32.5], &p),
        0.0
    );
    // The standard origin bias escapes self-shadowing on the surface.
    let biased = VoxelShadowParams {
        light_radius: 0.0,
        origin_bias: 0.5,
        max_steps: MAX_DDA_STEPS,
    };
    // Point on the west face of the block, light to the west: biased origin
    // leaves the block before it can self-occlude.
    let s = voxel_shadow_factor(&grid, [32.0, 32.5, 32.5], [20.5, 32.5, 32.5], &biased);
    assert_eq!(s, 1.0);
}

#[test]
fn world_space_wrapper_uses_grid_origin() {
    let mut grid = VoxelOccupancyGrid::with_origin([1000, 64, -2000]);
    grid.set_opaque_block([1032, 96, -1968], true); // local (32,32,32)
    let p = VoxelShadowParams::HARD;
    let s = voxel_shadow_factor_world(
        &grid,
        [1020.5, 96.5, -1967.5],
        [1040.5, 96.5, -1967.5],
        &p,
    );
    assert_eq!(s, 0.0);
    let s = voxel_shadow_factor_world(
        &grid,
        [1020.5, 70.5, -1967.5],
        [1040.5, 70.5, -1967.5],
        &p,
    );
    assert_eq!(s, 1.0);
}

#[test]
fn shadow_is_monotone_in_blocker_thickness() {
    // Thicker occluders can only darken, never lighten.
    let mut thin = VoxelOccupancyGrid::new();
    thin.set_opaque(32, 32, 32, true);
    let mut thick = thin.clone();
    for x in 33..38 {
        thick.set_opaque(x, 32, 32, true);
    }
    let p = params(2.0);
    let from = [28.5, 32.5, 32.5];
    let to = [44.5, 32.5, 32.5];
    let s_thin = voxel_shadow_factor(&thin, from, to, &p);
    let s_thick = voxel_shadow_factor(&thick, from, to, &p);
    // Penumbra w = 2*2*3.5/12.5 = 1.12: chord 1 -> partial, chord 5 -> full.
    assert!(s_thin > 0.0 && s_thin < 1.0);
    assert_eq!(s_thick, 0.0);
    assert!(s_thick < s_thin);
}

// ---------------------------------------------------------------------------
// WGSL kernel contract
// ---------------------------------------------------------------------------

fn assert_balanced_wgsl(src: &str) {
    let mut brackets = Vec::new();
    for ch in src.chars() {
        match ch {
            '{' | '(' | '[' => brackets.push(ch),
            '}' => assert_eq!(brackets.pop(), Some('{')),
            ')' => assert_eq!(brackets.pop(), Some('(')),
            ']' => assert_eq!(brackets.pop(), Some('[')),
            _ => {}
        }
    }
    assert!(brackets.is_empty(), "unclosed WGSL delimiter");
}

#[test]
fn wgsl_kernel_is_well_formed_and_has_entrypoint() {
    let src = VOXEL_DDA_SHADOW_WGSL;
    assert_balanced_wgsl(src);
    assert!(src.contains("@compute"));
    assert!(src.contains("@workgroup_size(64)"));
    assert!(src.contains("fn cs_main("));
    assert!(src.contains("@builtin(global_invocation_id)"));
    assert!(src.contains("fn voxel_dda_shadow("));
}

#[test]
fn wgsl_bitgrid_binding_matches_cpu_layout() {
    let src = VOXEL_DDA_SHADOW_WGSL;
    // storage<read> u32 word buffer.
    assert!(src.contains("var<storage, read> voxel_bits: array<u32>;"));
    // Identical bit-index math as VoxelOccupancyGrid: idx = x + 64*(z + 64*y).
    assert!(src.contains("v.x + VOXEL_DIM * (v.z + VOXEL_DIM * v.y)"));
    assert!(src.contains("const VOXEL_DIM: i32 = 64;"));
    assert!(src.contains("const VOXEL_WORDS: u32 = 8192u;"));
    // word = idx >> 5, bit = idx & 31.
    assert!(src.contains("voxel_bits[idx >> 5u]"));
    assert!(src.contains("idx & 31u"));
    // Same "infinity" constant as the CPU BIG_T.
    assert!(src.contains("3.402823e+38"));
}

#[test]
fn wgsl_uses_consistent_types_and_mirrored_control_flow() {
    let src = VOXEL_DDA_SHADOW_WGSL;
    // Integer cell/step vectors and float parametric vectors, explicitly typed.
    assert!(src.contains("vec3<i32>"));
    assert!(src.contains("vec3<f32>"));
    assert!(src.contains("f32(cell.x)"));
    assert!(src.contains("u32(dda_params.values.y)"));
    // The same similar-triangles penumbra the CPU evaluates.
    assert!(src.contains("2.0 * radius * first_hit"));
    assert!(src.contains("max(travel - first_hit, DDA_EPS)"));
    assert!(src.contains("clamp(1.0 - coverage, 0.0, 1.0)"));
    // No forbidden constructs: no textures, no atomics, no infinite loop.
    assert!(!src.contains("textureSample"));
    assert!(!src.contains("atomic"));
    assert!(src.contains("for (var i = 0u; i < max_steps; i++)"));
}

#[test]
fn cpu_and_wgsl_encode_identical_grid_and_step_constants() {
    // The constants baked into the WGSL must equal the Rust ones; if the grid
    // ever changes size both sides must move together.
    let src = VOXEL_DDA_SHADOW_WGSL;
    let dim_decl = format!("const VOXEL_DIM: i32 = {};", VOXEL_GRID_DIM);
    let words_decl = format!("const VOXEL_WORDS: u32 = {}u;", VOXEL_GRID_WORDS);
    assert!(src.contains(&dim_decl));
    assert!(src.contains(&words_decl));
    // Step cap mirrored: WGSL clamps to 256u just as params clamp to
    // MAX_DDA_STEPS.
    let cap = format!("{}u", MAX_DDA_STEPS);
    assert!(src.contains(&cap));
}

#[test]
fn origin_bias_never_marches_past_the_light() {
    // A shaded point 1.1 voxels from a torch with a wide `origin_bias`: the
    // bias slides the march origin toward the light, and an unclamped nudge
    // walks the segment past the torch into the wall behind it, shading a
    // receiver that the light plainly reaches. The clamp caps the nudge at
    // half the chord, so the torch always stays inside the marched segment.
    let mut grid = VoxelOccupancyGrid::with_origin([0, 0, 0]);
    grid.set_opaque(2, 0, 0, true);
    let from = [0.1, 0.5, 0.5];
    let to = [1.2, 0.5, 0.5];
    let wide = VoxelShadowParams {
        light_radius: 0.25,
        origin_bias: 1.0,
        max_steps: MAX_DDA_STEPS,
    };
    assert_eq!(
        voxel_shadow_factor(&grid, from, to, &wide),
        1.0,
        "geometry behind the emitter must not shadow the receiver"
    );
    // The same wide bias on a ray that really is blocked still occludes, so
    // the clamp cannot be mistaken for a blanket disable.
    let blocked_from = [0.1, 0.5, 0.5];
    let blocked_to = [4.0, 0.5, 0.5];
    assert_eq!(
        voxel_shadow_factor(&grid, blocked_from, blocked_to, &wide),
        0.0,
        "a wall on the path still blocks"
    );
    // The WGSL kernel carries the identical clamp and the reduced chord.
    let src = VOXEL_DDA_SHADOW_WGSL;
    assert!(src.contains("min(clamp(dda_params.values.z, 0.0, 1.0), len * 0.5)"));
    assert!(src.contains("max(len - bias, DDA_EPS)"));
}
