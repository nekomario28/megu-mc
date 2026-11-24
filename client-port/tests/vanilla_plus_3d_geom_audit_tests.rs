// SPDX-License-Identifier: LGPL-3.0-or-later

//! Geometry audit pins: rim UV sampling, HD sprite extrusion, model-projected
//! fixture UVs, coplanar z-fighting freedom, and ladder facing orientation.

use megu_client_mesh::vanilla_plus_3d::*;
use megu_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn assert_finite(mesh: &ExtrudedMesh) {
    for (i, q) in mesh.quads.iter().enumerate() {
        for p in &q.positions {
            for c in p {
                assert!(c.is_finite(), "quad {i} has non-finite position {p:?}");
            }
        }
    }
}

fn uv_midpoint(q: &ExtrudedQuad) -> [f32; 2] {
    let mut m = [0.0f32; 2];
    for uv in &q.uv {
        m[0] += uv[0] / 4.0;
        m[1] += uv[1] / 4.0;
    }
    m
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm3(a: [f32; 3]) -> f32 {
    dot3(a, a).sqrt()
}

/// Projects `p` onto the plane through the origin spanned by `t1`/`t2`.
fn project2(p: [f32; 3], t1: [f32; 3], t2: [f32; 3]) -> [f32; 2] {
    [dot3(p, t1), dot3(p, t2)]
}

/// Whether two convex planar quads intersect with positive area, judged by
/// the separating-axis test on their edge normals. Touching along an edge
/// or at a point returns `false` — only real overlapping surface counts.
fn positive_area_overlap(a: &[[f32; 2]; 4], b: &[[f32; 2]; 4]) -> bool {
    const EPS: f32 = 1e-5;
    let mut axes: Vec<[f32; 2]> = Vec::with_capacity(8);
    for poly in [a, b] {
        for e in 0..4 {
            let p0 = poly[e];
            let p1 = poly[(e + 1) % 4];
            let ex = p1[0] - p0[0];
            let ey = p1[1] - p0[1];
            let len = (ex * ex + ey * ey).sqrt();
            if len < 1e-9 {
                continue;
            }
            axes.push([-ey / len, ex / len]);
        }
    }
    for ax in &axes {
        let (mut a0, mut a1) = (f32::MAX, f32::MIN);
        for p in a.iter() {
            let s = p[0] * ax[0] + p[1] * ax[1];
            a0 = a0.min(s);
            a1 = a1.max(s);
        }
        let (mut b0, mut b1) = (f32::MAX, f32::MIN);
        for p in b.iter() {
            let s = p[0] * ax[0] + p[1] * ax[1];
            b0 = b0.min(s);
            b1 = b1.max(s);
        }
        if a1.min(b1) - a0.max(b0) <= EPS {
            return false;
        }
    }
    true
}

/// Counts quad pairs that are coplanar (parallel normals, same plane offset)
/// AND overlap with positive area — the exact z-fighting configuration.
fn coplanar_overlap_pairs(mesh: &ExtrudedMesh) -> Vec<(usize, usize)> {
    const NORMAL_EPS: f32 = 1e-4;
    const OFFSET_EPS: f32 = 1e-4;
    let mut pairs = Vec::new();
    for i in 0..mesh.quads.len() {
        for j in (i + 1)..mesh.quads.len() {
            let (a, b) = (&mesh.quads[i], &mesh.quads[j]);
            let na = a.normal;
            let parallel = dot3(na, b.normal).abs();
            if (parallel - 1.0).abs() > NORMAL_EPS {
                continue;
            }
            // Plane offsets along `na`: `dot(na, p)` describes the same
            // geometric plane whether `b`'s normal is parallel or
            // antiparallel to `na`, so facing-each-other coplanar pairs
            // (the classic z-fight) are caught too.
            let off_a = dot3(na, a.positions[0]);
            let off_b = dot3(na, b.positions[0]);
            if (off_a - off_b).abs() > OFFSET_EPS {
                continue;
            }
            // In-plane basis for the separating-axis projection.
            let seed = if na[0].abs() < 0.9 {
                [1.0, 0.0, 0.0]
            } else {
                [0.0, 1.0, 0.0]
            };
            let t1 = {
                let c = cross3(na, seed);
                let l = norm3(c);
                [c[0] / l, c[1] / l, c[2] / l]
            };
            let t2 = cross3(na, t1);
            let pa: [[f32; 2]; 4] = std::array::from_fn(|k| project2(a.positions[k], t1, t2));
            let pb: [[f32; 2]; 4] = std::array::from_fn(|k| project2(b.positions[k], t1, t2));
            if positive_area_overlap(&pa, &pb) {
                pairs.push((i, j));
            }
        }
    }
    pairs
}

fn assert_no_coplanar_z_fighting(mesh: &ExtrudedMesh, what: &str) {
    let pairs = coplanar_overlap_pairs(mesh);
    assert!(
        pairs.is_empty(),
        "{what}: {} coplanar overlapping quad pairs, first {:?}",
        pairs.len(),
        pairs.first()
    );
}

// ---------------------------------------------------------------------------
// Rim UVs sample the texels they bound
// ---------------------------------------------------------------------------

#[test]
fn rim_uvs_land_on_opaque_texels() {
    // Centred 2x2 opaque block: cells tx in {7,8}, ty in {7,8}.
    let mut mask = [0u8; 256];
    for y in 7..9 {
        for x in 7..9 {
            mask[y * 16 + x] = 255;
        }
    }
    let mesh = extrude_alpha_mask_16x16(&mask, TEXEL);
    assert_eq!(mesh.quad_count(), 6, "2x2 block: 2 faces + 4 rims");

    let mut rims = 0;
    for q in &mesh.quads {
        // Front/back faces have |normal.z| == 1; rims face sideways.
        if q.normal[2].abs() > 0.5 {
            continue;
        }
        rims += 1;
        let m = uv_midpoint(q);
        let cx = (m[0] * 16.0).floor() as usize;
        let cy = (m[1] * 16.0).floor() as usize;
        assert!(
            cx < 16 && cy < 16 && mask[cy * 16 + cx] >= ALPHA_CUTOFF,
            "rim quad uv midpoint {m:?} lands on transparent texel ({cx}, {cy})"
        );
    }
    assert_eq!(rims, 4, "expected exactly four rim quads");
}

// ---------------------------------------------------------------------------
// HD alpha-mask extrusion
// ---------------------------------------------------------------------------

fn hd_mask(w: usize, h: usize) -> Vec<u8> {
    // Centred opaque rectangle covering the middle half of the sprite.
    let mut m = vec![0u8; w * h];
    for y in h / 4..3 * h / 4 {
        for x in w / 4..3 * w / 4 {
            m[y * w + x] = 255;
        }
    }
    m
}

#[test]
fn extrude_alpha_mask_supports_hd_dimensions() {
    for &(w, h) in &[(16usize, 16usize), (32, 32), (64, 64), (32, 16), (8, 128)] {
        let mask = hd_mask(w, h);
        let mesh = extrude_alpha_mask(w, h, &mask, TEXEL);
        assert!(mesh.quad_count() > 0, "{w}x{h} produced no quads");
        assert!(mesh.quad_count() <= MAX_UPGRADE_QUADS);
        assert_finite(&mesh);
        for q in &mesh.quads {
            for uv in &q.uv {
                assert!((-1e-6..=1.0 + 1e-6).contains(&uv[0]));
                assert!((-1e-6..=1.0 + 1e-6).contains(&uv[1]));
            }
        }
    }
    // A solid HD sprite still merges to exactly six quads.
    let solid = vec![255u8; 64 * 64];
    assert_eq!(extrude_alpha_mask(64, 64, &solid, TEXEL).quad_count(), 6);
    // The 16x16 entry point is exactly the generalized call.
    let m16 = hd_mask(16, 16);
    let arr: [u8; 256] = m16.clone().try_into().unwrap();
    assert_eq!(
        extrude_alpha_mask(16, 16, &m16, TEXEL),
        extrude_alpha_mask_16x16(&arr, TEXEL)
    );
}

#[test]
fn extrude_alpha_mask_fails_closed_on_invalid_input() {
    let good = vec![255u8; 32 * 32];
    for (w, h, len) in [
        (0usize, 16usize, 0usize),
        (16, 0, 0),
        (129, 16, 129 * 16),
        (16, 129, 16 * 129),
        (32, 32, 32 * 32 - 1), // length mismatch
        (32, 32, 32 * 32 + 1),
    ] {
        let buf = vec![255u8; len];
        assert_eq!(
            extrude_alpha_mask(w, h, &buf, TEXEL).quad_count(),
            0,
            "{w}x{h} len {len} must fail closed"
        );
    }
    // Zero/negative thickness and empty masks also fail closed.
    assert_eq!(extrude_alpha_mask(32, 32, &good, 0.0).quad_count(), 0);
    assert_eq!(extrude_alpha_mask(32, 32, &good, -1.0).quad_count(), 0);
    assert_eq!(extrude_alpha_mask(32, 32, &vec![0u8; 1024], TEXEL).quad_count(), 0);
}

// ---------------------------------------------------------------------------
// Model-projected fixture UVs
// ---------------------------------------------------------------------------

#[test]
fn fixture_side_uvs_are_model_projected_near_center() {
    for kind in [FixtureKind::Torch, FixtureKind::Lantern, FixtureKind::Chain] {
        let mesh = build_3d_fixture_mesh(kind);
        let mut side_quads = 0;
        for q in &mesh.quads {
            // Only side faces (horizontal normals) carry the projected
            // `(horizontal, 1 - y)` UVs this audit pins.
            if q.normal[1].abs() > 0.5 {
                continue;
            }
            side_quads += 1;
            let m = uv_midpoint(q);
            assert!(
                (0.3..=0.7).contains(&m[0]),
                "{kind:?} side-face uv u={} outside centred band (mid {m:?})",
                m[0]
            );
            // Faces whose geometry sits in the central height band must also
            // project v near the middle — never the old `(0, 0)` corner.
            let y_mid = q.positions.iter().map(|p| p[1]).sum::<f32>() / 4.0;
            if (0.3..=0.7).contains(&y_mid) {
                assert!(
                    (0.3..=0.7).contains(&m[1]),
                    "{kind:?} central side-face uv v={} outside band (mid {m:?})",
                    m[1]
                );
            }
        }
        assert!(side_quads > 0, "{kind:?} produced no side faces");
    }
    // The fully centred torch samples the middle of the texture on every
    // single face — the exact defect the audit flagged.
    let torch = build_3d_fixture_mesh(FixtureKind::Torch);
    for q in &torch.quads {
        let m = uv_midpoint(q);
        assert!(
            (0.3..=0.7).contains(&m[0]) && (0.3..=0.7).contains(&m[1]),
            "torch quad uv midpoint {m:?} not centred"
        );
    }
}

// ---------------------------------------------------------------------------
// Coplanar z-fighting freedom
// ---------------------------------------------------------------------------

#[test]
fn diagonal_cross_has_no_coplanar_overlaps() {
    for block in [[0, 0, 0], [5, 70, -3]] {
        let mesh = build_3d_cross_mesh(&[255u8; 256], CrossLayout::Diagonal, block);
        assert!(!mesh.quads.is_empty());
        assert_no_coplanar_z_fighting(&mesh, "diagonal cross");
    }
}

#[test]
fn curved_rail_has_no_coplanar_overlaps() {
    for powered in [false, true] {
        let mesh = build_3d_rail_mesh(RailShape::CurveSouthEast, powered);
        assert!(!mesh.quads.is_empty());
        assert_no_coplanar_z_fighting(&mesh, "CurveSouthEast rail");
    }
}

#[test]
fn chain_and_iron_bars_have_no_coplanar_overlaps() {
    for kind in [FixtureKind::Chain, FixtureKind::IronBars] {
        let mesh = build_3d_fixture_mesh(kind);
        assert!(!mesh.quads.is_empty());
        assert_no_coplanar_z_fighting(&mesh, "fixture");
    }
}

// ---------------------------------------------------------------------------
// Ladder facing orientation
// ---------------------------------------------------------------------------

#[test]
fn ladder_north_facing_attaches_to_south_wall() {
    // Vanilla `facing=north`: the ladder fronts north (-z) and is attached
    // to the wall behind it — the south wall at `z = 1 - depth .. 1`.
    let mesh = build_3d_ladder_mesh(Direction::North);
    let min_z = mesh
        .quads
        .iter()
        .flat_map(|q| q.positions)
        .fold(f32::MAX, |m, p| m.min(p[2]));
    let max_z = mesh
        .quads
        .iter()
        .flat_map(|q| q.positions)
        .fold(f32::MIN, |m, p| m.max(p[2]));
    assert!(
        min_z > 0.8,
        "north-facing ladder must hug the south wall, min z = {min_z}"
    );
    assert!((max_z - 1.0).abs() < 1e-6, "plate must reach z = 1, got {max_z}");

    // South facing mirrors onto the north wall (z ~ 0).
    let south = build_3d_ladder_mesh(Direction::South);
    let south_min_z = south
        .quads
        .iter()
        .flat_map(|q| q.positions)
        .fold(f32::MAX, |m, p| m.min(p[2]));
    assert!(
        south_min_z < 0.2,
        "south-facing ladder must hug the north wall, min z = {south_min_z}"
    );

    // East/West attach to the x walls.
    let east = build_3d_ladder_mesh(Direction::East);
    let east_min_x = east
        .quads
        .iter()
        .flat_map(|q| q.positions)
        .fold(f32::MAX, |m, p| m.min(p[0]));
    assert!(east_min_x < 0.2, "east-facing ladder on wrong wall");
    let west = build_3d_ladder_mesh(Direction::West);
    let west_max_x = west
        .quads
        .iter()
        .flat_map(|q| q.positions)
        .fold(f32::MIN, |m, p| m.max(p[0]));
    assert!(west_max_x > 0.8, "west-facing ladder on wrong wall");
}
