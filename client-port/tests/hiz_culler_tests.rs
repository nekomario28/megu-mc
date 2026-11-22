// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use megu_client_mesh::hiz_culler::{HiZCullStats, HiZError, HiZPyramid};

/// Orthographic `view_proj` for a `side x side` pixel buffer: world x/y in
/// `[0, side]` map 1:1 onto pixels and world z passes through as NDC depth
/// (`w = 1`, so no corner is ever behind the camera).
fn ortho_view_proj(side: f32) -> [[f32; 4]; 4] {
    [
        [2.0 / side, 0.0, 0.0, -1.0],
        [0.0, 2.0 / side, 0.0, -1.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// Perspective `view_proj` looking down +z: `w_clip = z_view`, so world z
/// `<= 0` lands a corner on or behind the near plane. `z_ndc` is standard
/// `[0, 1]` across `[near, far]`.
fn perspective_view_proj(near: f32, far: f32) -> [[f32; 4]; 4] {
    [
        [near, 0.0, 0.0, 0.0],
        [0.0, near, 0.0, 0.0],
        [0.0, 0.0, far / (far - near), -near * far / (far - near)],
        [0.0, 0.0, 1.0, 0.0],
    ]
}

// ---------------------------------------------------------------------------
// Pyramid construction & input validation.
// ---------------------------------------------------------------------------

#[test]
fn depth_buffer_validation_rejects_bad_shapes() {
    assert_eq!(
        HiZPyramid::from_depth_buffer(0, 8, &[]).unwrap_err(),
        HiZError::InvalidDimensions {
            width: 0,
            height: 8
        }
    );
    assert_eq!(
        HiZPyramid::from_depth_buffer(8, 0, &[]).unwrap_err(),
        HiZError::InvalidDimensions {
            width: 8,
            height: 0
        }
    );
    // Dimensions are validated before the texel count.
    assert_eq!(
        HiZPyramid::from_depth_buffer(4097, 1, &[]).unwrap_err(),
        HiZError::InvalidDimensions {
            width: 4097,
            height: 1
        }
    );
    assert_eq!(
        HiZPyramid::from_depth_buffer(8, 8, &[0.5; 63]).unwrap_err(),
        HiZError::DepthLengthMismatch {
            expected: 64,
            actual: 63
        }
    );
    assert!(HiZPyramid::from_depth_buffer(1, 1, &[0.5]).is_ok());
}

#[test]
fn nonfinite_depth_texels_are_scrubbed_to_far() {
    let mut depth = vec![0.1f32; 64];
    depth[0] = f32::NAN;
    depth[3] = f32::INFINITY;
    let pyr = HiZPyramid::from_depth_buffer(8, 8, &depth).unwrap();

    // The scrubbed texels read as farthest depth: a rect covering them can
    // never cull, even for a very deep primitive.
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 1.0, 1.0, 0.9));
    assert!(!pyr.is_screen_rect_occluded(3.0, 0.0, 4.0, 1.0, 0.9));
    // Rects away from the corrupt texels still cull normally.
    assert!(pyr.is_screen_rect_occluded(4.0, 4.0, 6.0, 6.0, 0.9));
}

#[test]
fn pyramid_max_propagates_2x2_and_odd_borders() {
    // 5x3: odd in both dimensions. Hand-computed max-mip values.
    let depth = [
        0.10, 0.90, 0.20, 0.80, 0.30, // row 0
        0.40, 0.50, 0.60, 0.70, 0.15, // row 1
        0.25, 0.35, 0.45, 0.55, 0.65, // row 2
    ];
    let pyr = HiZPyramid::from_depth_buffer(5, 3, &depth).unwrap();
    assert_eq!(pyr.mip_count(), 4);

    let (w, h, texels) = pyr.mip(1).unwrap();
    assert_eq!((w, h), (3, 2));
    // (2,0) folds the odd right column only; (0,1) the odd bottom row;
    // (2,1) the lone corner texel.
    assert_eq!(texels, &[0.9, 0.8, 0.3, 0.35, 0.55, 0.65]);

    let (w, h, texels) = pyr.mip(2).unwrap();
    assert_eq!((w, h), (2, 1));
    assert_eq!(texels, &[0.9, 0.65]);

    let (w, h, texels) = pyr.mip(3).unwrap();
    assert_eq!((w, h), (1, 1));
    assert_eq!(texels, &[0.9]);
}

// ---------------------------------------------------------------------------
// Screen-rect query guards.
// ---------------------------------------------------------------------------

#[test]
fn occlusion_query_never_culls_on_unsafe_inputs() {
    let pyr = HiZPyramid::from_depth_buffer(8, 8, &[0.5f32; 64]).unwrap();

    // Near-plane touch and non-finite depth.
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 4.0, 4.0, 0.0));
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 4.0, 4.0, -0.5));
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 4.0, 4.0, f32::NAN));
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 4.0, 4.0, f32::INFINITY));
    // Non-finite bounds.
    assert!(!pyr.is_screen_rect_occluded(f32::NAN, 0.0, 4.0, 4.0, 0.9));
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, f32::INFINITY, 4.0, 0.9));
    // Fully off-screen and degenerate rects.
    assert!(!pyr.is_screen_rect_occluded(-10.0, -10.0, -1.0, -1.0, 0.9));
    assert!(!pyr.is_screen_rect_occluded(2.0, 2.0, 2.0, 4.0, 0.9));
    // Depth tie: coplanar geometry is never occluded.
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 4.0, 4.0, 0.5));
}

// ---------------------------------------------------------------------------
// Mountain wall scenario (odd-dimension border coverage below).
// ---------------------------------------------------------------------------

/// 64x64 buffer: bottom half (pixel rows 0..32) is a mountain wall at
/// `z = 0.2`, the top half is sky at `1.0`. Because 32 is a multiple of
/// every mip scale below the 1x1 apex, any rect staying in rows `< 32` has
/// its covered texels entirely inside the wall, and any rect crossing row
/// 32 covers a sky-bearing texel.
fn wall_pyramid() -> HiZPyramid {
    let depth: Vec<f32> = (0..64 * 64)
        .map(|i| if i / 64 < 32 { 0.2 } else { 1.0 })
        .collect();
    HiZPyramid::from_depth_buffer(64, 64, &depth).unwrap()
}

#[test]
fn wall_occludes_every_hidden_aabb_and_nothing_else() {
    let pyr = wall_pyramid();
    let vp = ortho_view_proj(64.0);

    // 60 cave/chunk AABBs fully behind the wall (z in 0.6..0.87, screen
    // rect inside pixel rows < 32): 100% must cull.
    for i in 0..60u32 {
        let x = 2.0 + (i % 8) as f32 * 7.0;
        let y = 2.0 + (i % 5) as f32 * 5.0;
        let z = 0.6 + (i % 4) as f32 * 0.09;
        assert!(
            pyr.cull_world_aabb(&vp, [x, y, z], [x + 6.0, (y + 5.0).min(31.0), z + 0.05]),
            "hidden aabb {i} must cull"
        );
    }
    // Same screen rects but in front of the wall (z < 0.2): never cull.
    for i in 0..20u32 {
        let x = 4.0 + (i % 6) as f32 * 9.0;
        let y = 3.0 + (i % 4) as f32 * 6.0;
        let z = 0.05 + (i % 3) as f32 * 0.05;
        assert!(
            !pyr.cull_world_aabb(&vp, [x, y, z], [x + 5.0, y + 4.0, z + 0.03]),
            "in-front aabb {i} must stay visible"
        );
    }
    // Coplanar with the wall surface: never cull.
    assert!(!pyr.cull_world_aabb(&vp, [10.0, 5.0, 0.2], [30.0, 20.0, 0.2]));
    // Straddling the near plane through z <= 0: never cull.
    assert!(!pyr.cull_world_aabb(&vp, [10.0, 5.0, -0.1], [30.0, 20.0, 0.9]));
    // Peeking above the wall: rect crosses pixel row 32 into sky, so the
    // AABB keeps sky texels in its covered set even at z = 0.7.
    for i in 0..20u32 {
        let x = 4.0 + (i % 6) as f32 * 9.0;
        let y = 26.0 + (i % 4) as f32 * 2.0; // crosses y = 32 with +h
        assert!(
            !pyr.cull_world_aabb(&vp, [x, y, 0.7], [x + 5.0, y + 10.0, 0.8]),
            "peeking aabb {i} must stay visible"
        );
    }
    // Fully over sky: visible trivially.
    assert!(!pyr.cull_world_aabb(&vp, [10.0, 40.0, 0.7], [30.0, 60.0, 0.9]));
}

// ---------------------------------------------------------------------------
// Odd dimensions: border texels must reach the parent max.
// ---------------------------------------------------------------------------

#[test]
fn odd_dimensions_never_false_cull_border_texels() {
    let (w, h) = (137usize, 79usize);
    // Interior cliff at z = 0.1; the odd right column and bottom row stay
    // at far depth so only the border texels can veto a cull.
    let mut depth = vec![0.1f32; w * h];
    for y in 0..h {
        depth[y * w + w - 1] = 1.0;
    }
    for x in 0..w {
        depth[(h - 1) * w + x] = 1.0;
    }
    let pyr = HiZPyramid::from_depth_buffer(w, h, &depth).unwrap();

    // Per-pixel rects on the far border: exact level-0 test, never culls.
    for x in 0..w {
        let x = x as f32;
        assert!(!pyr.is_screen_rect_occluded(x, (h - 1) as f32, x + 1.0, h as f32, 0.5));
    }
    for y in 0..h {
        let y = y as f32;
        assert!(!pyr.is_screen_rect_occluded((w - 1) as f32, y, w as f32, y + 1.0, 0.5));
    }
    // Rects large enough to be tested at mip >= 3 whose covered set
    // includes an odd-border far texel: if the border texel were dropped
    // from its parent max this would cull — it must not.
    assert!(!pyr.is_screen_rect_occluded(128.0, 8.0, 137.0, 24.0, 0.9));
    assert!(!pyr.is_screen_rect_occluded(8.0, 72.0, 16.0, 79.0, 0.9));
    assert!(!pyr.is_screen_rect_occluded(124.0, 68.0, 137.0, 79.0, 0.9));
    // Whole screen: the apex texel is 1.0, so nothing ever culls against
    // a sky pixel.
    assert!(!pyr.is_screen_rect_occluded(0.0, 0.0, 137.0, 79.0, 1.0));

    // Converse: a near occluder ON the odd border still culls. A 9-wide
    // near strip at the right edge and a 9-tall one at the bottom fill the
    // border mips with near depth.
    let mut depth = vec![1.0f32; w * h];
    for y in 0..h {
        for x in 128..w {
            depth[y * w + x] = 0.05;
        }
    }
    for y in 70..h {
        for x in 0..w {
            depth[y * w + x] = 0.05;
        }
    }
    let pyr = HiZPyramid::from_depth_buffer(w, h, &depth).unwrap();
    assert!(pyr.is_screen_rect_occluded(130.0, 8.0, 137.0, 24.0, 0.9));
    assert!(pyr.is_screen_rect_occluded(8.0, 72.0, 16.0, 79.0, 0.9));
    assert!(pyr.is_screen_rect_occluded(130.0, 72.0, 137.0, 79.0, 0.9));
}

// ---------------------------------------------------------------------------
// Near-plane straddling through perspective projection.
// ---------------------------------------------------------------------------

#[test]
fn aabb_straddling_near_plane_is_never_culled() {
    // Whole screen is near occluder, so any testable AABB would cull.
    let pyr = HiZPyramid::from_depth_buffer(64, 64, &vec![0.05f32; 64 * 64]).unwrap();
    let vp = perspective_view_proj(0.5, 100.0);

    // Corners on both sides of the camera plane (w < 0 at z = -2).
    assert!(!pyr.cull_world_aabb(&vp, [-1.0, -1.0, -2.0], [1.0, 1.0, 4.0]));
    // Exactly on the plane: all corners have w == 0.
    assert!(!pyr.cull_world_aabb(&vp, [-1.0, -1.0, 0.0], [1.0, 1.0, 0.0]));
    // Fully behind the camera.
    assert!(!pyr.cull_world_aabb(&vp, [-1.0, -1.0, -4.0], [1.0, 1.0, -1.0]));
    // Non-finite AABB input.
    assert!(!pyr.cull_world_aabb(
        &vp,
        [f32::NAN, -1.0, 5.0],
        [1.0, 1.0, 6.0]
    ));
    // Sanity: a small AABB fully in front of the near plane and deep in
    // the buffer still culls through the same code path.
    assert!(pyr.cull_world_aabb(&vp, [-0.1, -0.1, 5.0], [0.1, 0.1, 6.0]));
}

// ---------------------------------------------------------------------------
// Batch cull at scale: >75% occlusion with zero false occlusions.
// ---------------------------------------------------------------------------

#[test]
fn batch_cull_exceeds_75_percent_with_zero_false_occlusions() {
    let (w, h) = (256usize, 256usize);
    // Left half is mountain at z = 0.25 for the full height; right half is
    // sky. Every mip level keeps the x = 128 split aligned, so any covered
    // range staying in pixels < 128 is pure wall and any range crossing
    // 128 sees a sky texel.
    let depth: Vec<f32> = (0..w * h)
        .map(|i| if i % w < 128 { 0.25 } else { 1.0 })
        .collect();
    let pyr = HiZPyramid::from_depth_buffer(w, h, &depth).unwrap();
    let vp = ortho_view_proj(256.0);

    let mut aabbs: Vec<([f32; 3], [f32; 3])> = Vec::new();
    // 200 hidden chunk AABBs: screen rects fully inside the wall half,
    // world z comfortably behind 0.25.
    for i in 0..200u32 {
        let x = 4.0 + (i % 10) as f32 * 10.0;
        let y = 4.0 + ((i / 10) % 20) as f32 * 12.0;
        let z = 0.5 + (i % 5) as f32 * 0.08;
        aabbs.push(([x, y, z], [x + 9.0, y + 10.0, z + 0.05]));
    }
    // 25 in front of the wall (nearer than 0.25): never occluded.
    for i in 0..25u32 {
        let x = 4.0 + (i % 5) as f32 * 20.0;
        let y = 8.0 + (i % 5) as f32 * 40.0;
        let z = 0.05 + (i % 3) as f32 * 0.05;
        aabbs.push(([x, y, z], [x + 9.0, y + 12.0, z + 0.04]));
    }
    // 13 fully over the sky half at wall-behind depths.
    for i in 0..13u32 {
        let x = 136.0 + (i % 8) as f32 * 12.0;
        let y = 8.0 + (i % 5) as f32 * 40.0;
        let z = 0.5 + (i % 5) as f32 * 0.08;
        aabbs.push(([x, y, z], [x + 8.0, y + 12.0, z + 0.05]));
    }
    // 12 straddling the wall/sky boundary: the covered set always contains
    // a sky texel, so these stay visible despite being behind the wall.
    for i in 0..12u32 {
        let x = 118.0 + (i % 3) as f32 * 4.0;
        let y = 8.0 + (i % 5) as f32 * 40.0;
        let z = 0.5 + (i % 4) as f32 * 0.1;
        aabbs.push(([x, y, z], [x + 16.0, y + 12.0, z + 0.05]));
    }

    let stats = pyr.cull_meshlet_batch(&vp, &aabbs);
    assert_eq!(
        stats,
        HiZCullStats {
            tested: 250,
            culled: 200,
            visible: 50,
        },
        "every hidden aabb culled, zero false occlusions"
    );
    assert!(stats.culled as f32 / stats.tested as f32 > 0.75);
}
