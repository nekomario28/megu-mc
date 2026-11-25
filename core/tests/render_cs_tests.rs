// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for `megu_core::render_cs`: cave/portal visibility BFS, six-axis
//! back-face bucket culling, the bit-parallel greedy mesher, the lazy
//! translucent radix sorter, the render-graph compiler, and the
//! fail-closed mod/protrusion safety contracts on top of all of them.

use std::collections::BTreeSet;

use megu_core::bloom::VoxelBitset4096;
use megu_core::coords::{CoordMap, DimIndex, SectionKey};
use megu_core::render_cs::{
    bfs_visible_sections, compute_section_visibility, visible_face_mask,
    visible_face_mask_protrusion, BfsScratch, BinaryGreedyMesher, DrawBucket, FaceDir,
    GreedyQuad, RenderGraph, RenderGraphError, ResourceDesc, ResourceId, SectionRenderMetadata,
    SectionVisBits, SixAxisBuckets, SortOutcome, TranslucentQuadKey, TranslucentSortCache,
    voxel_index,
};

/// Deterministic splitmix64 PRNG — reproducible across machines, no
/// wall-clock entropy.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn usize(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn f32(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        lo + unit * (hi - lo)
    }
}

fn sec(cx: i32, sy: i32, cz: i32) -> SectionKey {
    SectionKey {
        dim: DimIndex::OVERWORLD,
        cx,
        sy,
        cz,
    }
}

/// Build an occluder bitset from a voxel predicate.
fn occluders(pred: impl Fn(u8, u8, u8) -> bool) -> VoxelBitset4096 {
    let mut set = VoxelBitset4096::new();
    for y in 0..16 {
        for z in 0..16 {
            for x in 0..16 {
                if pred(x, y, z) {
                    set.set(voxel_index(x, y, z));
                }
            }
        }
    }
    set
}

/// Look up a metadata map for the BFS `meta` callback.
fn lookup<'m>(
    map: &'m CoordMap<SectionKey, SectionRenderMetadata>,
) -> impl Fn(SectionKey) -> Option<SectionRenderMetadata> + 'm {
    move |key| map.get(&key).copied()
}

// ---------------------------------------------------------------------------
// FaceDir
// ---------------------------------------------------------------------------

#[test]
fn face_dir_opposite_and_delta_are_consistent() {
    for dir in FaceDir::ALL {
        let opp = dir.opposite();
        assert_eq!(opp.opposite(), dir);
        let (dx, dy, dz) = dir.delta();
        let (ox, oy, oz) = opp.delta();
        assert_eq!((dx, dy, dz), (-ox, -oy, -oz));
        assert_eq!(dir.bit().count_ones(), 1);
    }
    assert_eq!(FaceDir::PosX.opposite(), FaceDir::NegX);
    assert_eq!(FaceDir::PosY.opposite(), FaceDir::NegY);
    assert_eq!(FaceDir::PosZ.opposite(), FaceDir::NegZ);
}

// ---------------------------------------------------------------------------
// SectionVisBits / compute_section_visibility
// ---------------------------------------------------------------------------

#[test]
fn visibility_fast_paths_for_air_and_stone() {
    assert_eq!(
        compute_section_visibility(&VoxelBitset4096::new()),
        SectionVisBits::ALL
    );
    let solid = occluders(|_, _, _| true);
    assert_eq!(
        compute_section_visibility(&solid),
        SectionVisBits::EMPTY
    );
    // A self check: every face pair of an all-air section connects.
    for a in FaceDir::ALL {
        for b in FaceDir::ALL {
            assert!(SectionVisBits::ALL.can_see(a, b));
        }
    }
}

#[test]
fn visibility_straight_tunnel_connects_two_faces_only() {
    // 1x1 tunnel along X through an otherwise solid section at y=8, z=8.
    let solid_except_tunnel = occluders(|_x, y, z| !(y == 8 && z == 8));
    let vis = compute_section_visibility(&solid_except_tunnel);
    assert!(vis.can_see(FaceDir::NegX, FaceDir::PosX));
    assert!(vis.can_see(FaceDir::PosX, FaceDir::NegX));
    // The tunnel touches only the two X faces.
    for a in FaceDir::ALL {
        for b in FaceDir::ALL {
            let expected = matches!(
                (a, b),
                (FaceDir::NegX, FaceDir::PosX)
                    | (FaceDir::PosX, FaceDir::NegX)
                    | (FaceDir::NegX, FaceDir::NegX)
                    | (FaceDir::PosX, FaceDir::PosX)
            );
            assert_eq!(vis.can_see(a, b), expected, "{a:?}->{b:?}");
        }
    }
}

#[test]
fn visibility_l_bend_and_isolated_pocket() {
    // L-shaped corridor: enters -X at (y=8, z=8, x<=8), turns up +Y at
    // (x=8, z=8, y>=8) — touches only the NegX and PosY faces.
    let set = occluders(|x, y, z| {
        let tunnel_x = y == 8 && z == 8 && x <= 8;
        let tunnel_y = x == 8 && z == 8 && y >= 8;
        !(tunnel_x || tunnel_y)
    });
    let vis = compute_section_visibility(&set);
    assert!(vis.can_see(FaceDir::NegX, FaceDir::PosY));
    assert!(vis.can_see(FaceDir::PosY, FaceDir::NegX));
    // No path from the X tunnel to +Z.
    assert!(!vis.can_see(FaceDir::NegX, FaceDir::PosZ));
    assert!(!vis.can_see(FaceDir::NegX, FaceDir::PosX));

    // Sealed air pocket in the middle connects no faces at all.
    let pocket = occluders(|x, y, z| !((4..12).contains(&x) && y == 8 && z == 8));
    let vis = compute_section_visibility(&pocket);
    assert_eq!(vis, SectionVisBits::EMPTY);
}

// ---------------------------------------------------------------------------
// bfs_visible_sections — portal culling & no-backtrack
// ---------------------------------------------------------------------------

#[test]
fn bfs_solid_wall_culls_cave_and_tunnel_reveals_it() {
    // A(0,0,0)=camera air, B(1,0,0)=solid wall, C(2,0,0)=air cave.
    let mut meta: CoordMap<SectionKey, SectionRenderMetadata> = CoordMap::default();
    let air = compute_section_visibility(&VoxelBitset4096::new());
    meta.insert(sec(0, 0, 0), SectionRenderMetadata::new(air));
    meta.insert(
        sec(1, 0, 0),
        SectionRenderMetadata::new(SectionVisBits::EMPTY),
    );
    meta.insert(sec(2, 0, 0), SectionRenderMetadata::new(air));

    let mut scratch = BfsScratch::default();
    let mut out = Vec::new();
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 16, &mut scratch, &mut out);

    // The wall's face is drawn; the sealed cave behind it is culled.
    assert!(out.contains(&sec(0, 0, 0)));
    assert!(out.contains(&sec(1, 0, 0)));
    assert!(!out.contains(&sec(2, 0, 0)));

    // Punch a 1x1 tunnel through the wall: the cave lights up.
    let tunnel = occluders(|_x, y, z| !(y == 8 && z == 8));
    meta.insert(
        sec(1, 0, 0),
        SectionRenderMetadata::new(compute_section_visibility(&tunnel)),
    );
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 16, &mut scratch, &mut out);
    assert!(out.contains(&sec(2, 0, 0)));
}

#[test]
fn bfs_portals_route_around_bends_without_duplicates() {
    // Camera A(0,0,0) has a solid roof, so +Y is only reachable through
    // B(1,0,0) -> C(1,1,0) -> D(0,1,0).
    let mut meta: CoordMap<SectionKey, SectionRenderMetadata> = CoordMap::default();
    let air = compute_section_visibility(&VoxelBitset4096::new());
    // A: air everywhere except the top layer (y=15 solid) -> no +Y exit.
    let a = occluders(|_x, y, _z| y == 15);
    meta.insert(sec(0, 0, 0), SectionRenderMetadata::new(compute_section_visibility(&a)));
    for key in [sec(1, 0, 0), sec(1, 1, 0), sec(0, 1, 0), sec(-1, 0, 0)] {
        meta.insert(key, SectionRenderMetadata::new(air));
    }
    let mut scratch = BfsScratch::default();
    let mut out = Vec::new();
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 16, &mut scratch, &mut out);
    assert!(out.contains(&sec(0, 1, 0)), "reachable via the bend");
    let mut dedup = out.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), out.len(), "no section reported twice");
}

// ---------------------------------------------------------------------------
// Protrusion safety — cave-portal BFS
// ---------------------------------------------------------------------------

#[test]
fn bfs_protruding_model_is_never_culled_behind_sealed_portals() {
    // B(1,0,0) is a cave section open toward the camera (-X) and the sky
    // (+Y) only — nothing exits toward C(2,0,0), which holds an
    // oversized Create-style wheel protruding 2 blocks west into B.
    let mut meta: CoordMap<SectionKey, SectionRenderMetadata> = CoordMap::default();
    let air = compute_section_visibility(&VoxelBitset4096::new());
    meta.insert(sec(0, 0, 0), SectionRenderMetadata::new(air));
    let cave = occluders(|x, y, z| {
        // Shaft enters -X at (y=8, z=8, x<=8) and exits +Y at
        // (x=8, z=8, y>=8): reachable onward, but never toward +X.
        let shaft = (y == 8 && z == 8 && x <= 8) || (x == 8 && z == 8 && y >= 8);
        !shaft
    });
    let cave_vis = compute_section_visibility(&cave);
    assert!(cave_vis.can_see(FaceDir::NegX, FaceDir::PosY));
    assert!(!cave_vis.can_see(FaceDir::NegX, FaceDir::PosX));
    meta.insert(sec(1, 0, 0), SectionRenderMetadata::new(cave_vis));
    // C: fully solid portal data — a strictly portal-based walk would
    // cull it — but its wheel protrudes 2 blocks into B.
    let wheel = SectionRenderMetadata::new(SectionVisBits::EMPTY).with_protrusion(2);
    meta.insert(sec(2, 0, 0), wheel);

    let mut scratch = BfsScratch::default();
    let mut out = Vec::new();
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 16, &mut scratch, &mut out);
    assert!(
        out.contains(&sec(2, 0, 0)),
        "protruding wheel must be drawn even though portals are sealed"
    );

    // Without the protrusion flag the same section is culled.
    meta.insert(sec(2, 0, 0), SectionRenderMetadata::new(SectionVisBits::EMPTY));
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 16, &mut scratch, &mut out);
    assert!(!out.contains(&sec(2, 0, 0)));
}

#[test]
fn bfs_deep_protrusion_reaches_two_sections_with_extended_scan() {
    let mut meta: CoordMap<SectionKey, SectionRenderMetadata> = CoordMap::default();
    let air = compute_section_visibility(&VoxelBitset4096::new());
    meta.insert(sec(0, 0, 0), SectionRenderMetadata::new(air));
    // B(1,0,0) solid wall: visible face, but portals dead-end.
    meta.insert(sec(1, 0, 0), SectionRenderMetadata::new(SectionVisBits::EMPTY));
    // D(3,0,0) protrudes 20 blocks west — reaches through B into A.
    meta.insert(
        sec(3, 0, 0),
        SectionRenderMetadata::new(SectionVisBits::EMPTY).with_protrusion(20),
    );

    let mut scratch = BfsScratch::default();
    let mut out = Vec::new();
    // Default 16-block scan cannot reach two sections away.
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 16, &mut scratch, &mut out);
    assert!(!out.contains(&sec(3, 0, 0)));
    // Scanning 32 blocks (2-section reach) reveals it: B is visible and
    // D's protrusion AABB overlaps B.
    bfs_visible_sections(sec(0, 0, 0), lookup(&meta), 32, &mut scratch, &mut out);
    assert!(out.contains(&sec(3, 0, 0)));
}

// ---------------------------------------------------------------------------
// visible_face_mask — mathematical half-space culling
// ---------------------------------------------------------------------------

#[test]
fn face_mask_math_per_axis() {
    let cam = (0, 0, 0);
    assert_eq!(visible_face_mask(cam, cam), 0b11_1111);
    // East target: +X faces point away -> cleared.
    assert_eq!(visible_face_mask(cam, (1, 0, 0)), 0b11_1111 & !FaceDir::PosX.bit());
    assert_eq!(visible_face_mask(cam, (-1, 0, 0)), 0b11_1111 & !FaceDir::NegX.bit());
    assert_eq!(visible_face_mask(cam, (0, 2, 0)), 0b11_1111 & !FaceDir::PosY.bit());
    assert_eq!(visible_face_mask(cam, (0, -3, 0)), 0b11_1111 & !FaceDir::NegY.bit());
    assert_eq!(visible_face_mask(cam, (0, 0, 5)), 0b11_1111 & !FaceDir::PosZ.bit());
    assert_eq!(visible_face_mask(cam, (0, 0, -5)), 0b11_1111 & !FaceDir::NegZ.bit());
    // Diagonal: one bit cleared per axis.
    let m = visible_face_mask(cam, (1, 0, -1));
    assert_eq!(m & FaceDir::PosX.bit(), 0);
    assert_eq!(m & FaceDir::NegZ.bit(), 0);
    assert_eq!(m.count_ones(), 4);
}

#[test]
fn face_mask_nine_cubed_grid_eliminates_half_space() {
    // Every section around the camera: exactly one bit is cleared per
    // non-zero axis offset — the strict mathematical bound.
    let cam = (4, 4, 4);
    let mut kept = 0u32;
    for dx in -4i32..=4 {
        for dy in -4i32..=4 {
            for dz in -4i32..=4 {
                let m = visible_face_mask(cam, (cam.0 + dx, cam.1 + dy, cam.2 + dz));
                let expected =
                    6 - [dx, dy, dz].iter().filter(|&&d| d != 0).count() as u32;
                assert_eq!(m.count_ones(), expected);
                kept += m.count_ones();
            }
        }
    }
    let total = 6 * 9 * 9 * 9;
    let culled = total - kept;
    // 1944/4374 ≈ 44.4% of face buckets eliminated outright.
    assert!(culled * 100 >= total * 44, "culled {culled}/{total}");
}

#[test]
fn face_mask_protrusion_expansion_is_fail_closed() {
    let cam = (0, 0, 0);
    // Adjacent section +X: any protrusion >= 1 can reach into the camera
    // section, so +X faces can no longer be culled.
    assert_eq!(
        visible_face_mask_protrusion(cam, (1, 0, 0), 0) & FaceDir::PosX.bit(),
        0
    );
    assert_eq!(
        visible_face_mask_protrusion(cam, (1, 0, 0), 1),
        0b11_1111
    );
    // Two sections away: 16 blocks of protrusion still cannot bridge the
    // gap; 17 can.
    assert_eq!(
        visible_face_mask_protrusion(cam, (2, 0, 0), 16) & FaceDir::PosX.bit(),
        0
    );
    assert_ne!(
        visible_face_mask_protrusion(cam, (2, 0, 0), 17) & FaceDir::PosX.bit(),
        0
    );
}

// ---------------------------------------------------------------------------
// SixAxisBuckets — never-culled custom bucket
// ---------------------------------------------------------------------------

#[test]
fn buckets_emit_only_visible_faces_but_always_custom() {
    let mut buckets: SixAxisBuckets<u32> = SixAxisBuckets::new();
    for dir in FaceDir::ALL {
        for i in 0..10u32 {
            buckets.push(DrawBucket::Face(dir), dir.index() as u32 * 100 + i);
        }
    }
    buckets.push(DrawBucket::UnculledCustom, 9999);
    buckets.bake();

    let mut ranges = Vec::new();
    // Camera sees only -X/+Y faces; custom still drawn.
    let mask = FaceDir::NegX.bit() | FaceDir::PosY.bit();
    buckets.emit_draw_ranges(mask, &mut ranges);
    assert_eq!(ranges.len(), 3);
    assert_eq!(ranges[0], buckets.range(FaceDir::NegX));
    assert_eq!(ranges[1], buckets.range(FaceDir::PosY));
    assert_eq!(ranges[2], buckets.custom_range());

    // Even a zero mask can never cull the custom bucket.
    ranges.clear();
    buckets.emit_draw_ranges(0, &mut ranges);
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].count, 1);

    // Baked items land in contiguous per-bucket spans.
    assert_eq!(buckets.bucket_items(DrawBucket::UnculledCustom), &[9999]);
    assert_eq!(buckets.bucket_items(DrawBucket::Face(FaceDir::PosX)).len(), 10);
    assert_eq!(buckets.len(), 61);
}

#[test]
fn buckets_diagonal_mod_geometry_survives_every_camera_angle() {
    // A Create-style diagonal belt quad lives in UnculledCustom while
    // ordinary cardinal terrain fills the face buckets; sweeping the
    // camera through all 27 neighbourhood positions must always emit it.
    let mut buckets: SixAxisBuckets<u32> = SixAxisBuckets::new();
    buckets.push(DrawBucket::UnculledCustom, 42);
    for dir in FaceDir::ALL {
        buckets.push(DrawBucket::Face(dir), dir.index() as u32);
    }
    buckets.bake();

    let cam = (4, 4, 4);
    for dx in -1i32..=1 {
        for dy in -1i32..=1 {
            for dz in -1i32..=1 {
                let mask = visible_face_mask(cam, (cam.0 + dx, cam.1 + dy, cam.2 + dz));
                let mut ranges = Vec::new();
                buckets.emit_draw_ranges(mask, &mut ranges);
                let last = ranges.last().expect("custom bucket always emitted");
                assert_eq!(*last, buckets.custom_range());
                assert_eq!(
                    ranges.len() - 1,
                    mask.count_ones() as usize,
                    "exactly the visible face buckets emit"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BinaryGreedyMesher — bit-exact parity with naive face emission
// ---------------------------------------------------------------------------

/// In-plane (u, v) coordinates of `dir`'s face on voxel `(x, y, z)` —
/// mirrors the mesher's axis convention.
fn border_uv(dir: FaceDir, x: u8, y: u8, z: u8) -> (u8, u8) {
    match dir {
        FaceDir::PosX | FaceDir::NegX => (y, z),
        FaceDir::PosY | FaceDir::NegY => (x, z),
        FaceDir::PosZ | FaceDir::NegZ => (x, y),
    }
}

/// Reference emitter: one face per (voxel, dir) pair whose neighbour is
/// non-occluding — the ground truth the mesher must match bit-exactly.
fn naive_faces(
    set: &VoxelBitset4096,
    border: impl Fn(FaceDir, u8, u8) -> bool,
    material_at: impl Fn(u8, u8, u8) -> u32,
) -> BTreeSet<(u16, FaceDir, u32)> {
    let mut faces = BTreeSet::new();
    for idx in 0..4096u16 {
        if !set.get(idx) {
            continue;
        }
        let x = (idx & 15) as u8;
        let y = (idx >> 8) as u8;
        let z = ((idx >> 4) & 15) as u8;
        for dir in FaceDir::ALL {
            let (dx, dy, dz) = dir.delta();
            let (nx, ny, nz) = (x as i32 + dx, y as i32 + dy, z as i32 + dz);
            let covered = if (0..16).contains(&nx) && (0..16).contains(&ny) && (0..16).contains(&nz)
            {
                set.get(voxel_index(nx as u8, ny as u8, nz as u8))
            } else {
                let (u, v) = border_uv(dir, x, y, z);
                border(dir, u, v)
            };
            if !covered {
                faces.insert((idx, dir, material_at(x, y, z)));
            }
        }
    }
    faces
}

/// Expand merged quads back into the unit-face set for comparison.
fn quad_faces(quads: &[GreedyQuad]) -> BTreeSet<(u16, FaceDir, u32)> {
    let mut faces = BTreeSet::new();
    for q in quads {
        for du in 0..q.w {
            for dv in 0..q.h {
                let (u, v) = (q.u + du, q.v + dv);
                let (x, y, z) = match q.dir {
                    FaceDir::PosX | FaceDir::NegX => (q.slice, u, v),
                    FaceDir::PosY | FaceDir::NegY => (u, q.slice, v),
                    FaceDir::PosZ | FaceDir::NegZ => (u, v, q.slice),
                };
                faces.insert((voxel_index(x, y, z), q.dir, q.material));
            }
        }
    }
    faces
}

fn mesher_parity(
    set: &VoxelBitset4096,
    border: impl Fn(FaceDir, u8, u8) -> bool,
    material_at: impl Fn(u8, u8, u8) -> u32,
) -> (usize, usize) {
    let mut quads = Vec::new();
    BinaryGreedyMesher::mesh(set, &border, &material_at, &mut quads);
    let naive = naive_faces(set, border, material_at);
    let merged = quad_faces(&quads);
    assert_eq!(
        merged, naive,
        "greedy mesher must emit bit-exact face coverage"
    );
    (quads.len(), naive.len())
}

#[test]
fn mesher_empty_and_full_sections() {
    let (quads, faces) = mesher_parity(&VoxelBitset4096::new(), |_, _, _| false, |_, _, _| 0);
    assert_eq!((quads, faces), (0, 0));
    // A full section emits only its six outer shells -> 6 merged quads.
    let solid = occluders(|_, _, _| true);
    let (quads, faces) = mesher_parity(&solid, |_, _, _| false, |_, _, _| 7);
    assert_eq!(faces, 6 * 256);
    assert_eq!(quads, 6);
}

#[test]
fn mesher_flat_chunk_compresses_top_to_one_quad() {
    // Flat terrain: bottom half solid. The 256 exposed top faces must
    // collapse into ONE quad (99.6% reduction on that plane alone).
    let flat = occluders(|_x, y, _z| y < 8);
    let mut quads = Vec::new();
    BinaryGreedyMesher::mesh(&flat, |_, _, _| false, |_, _, _| 0, &mut quads);
    let top: Vec<_> = quads.iter().filter(|q| q.dir == FaceDir::PosY).collect();
    assert_eq!(top.len(), 1);
    let top = top[0];
    assert_eq!((top.slice, top.u, top.v, top.w, top.h), (7, 0, 0, 16, 16));
    // Whole chunk: top + bottom + 4 sides.
    assert_eq!(quads.len(), 6);
    let naive = naive_faces(&flat, |_, _, _| false, |_, _, _| 0);
    assert_eq!(naive.len(), 1024);
    assert!(quads.len() * 100 <= naive.len(), ">=99% quad reduction");
}

#[test]
fn mesher_parity_noise_checkerboard_and_materials() {
    let mut rng = Rng::new(0x5EED_5EED);
    // Random noise at several densities.
    for &permille in &[200u32, 500, 800] {
        let mut noise = VoxelBitset4096::new();
        for i in 0..4096u16 {
            if rng.usize(1000) < permille as usize {
                noise.set(i);
            }
        }
        let (quads, faces) = mesher_parity(&noise, |_, _, _| false, |_, _, _| 0);
        assert!(quads <= faces);
    }
    // Checkerboard: maximal fragmentation, still bit-exact.
    let checker = occluders(|x, y, z| (x + y + z) % 2 == 0);
    let (quads, faces) = mesher_parity(&checker, |_, _, _| false, |_, _, _| 0);
    assert_eq!(quads, faces);
    // Multi-material: quads may only merge within one material key.
    let layered = occluders(|_x, y, _z| y < 12);
    mesher_parity(&layered, |_, _, _| false, |x, y, z| {
        ((x as u32) * 7 + (y as u32) * 3 + z as u32) % 5
    });
    // Neighbour border padding: alternating border occluders on -X.
    let walls = occluders(|_x, _y, _z| true);
    mesher_parity(&walls, |dir, u, v| dir == FaceDir::NegX && (u + v) % 2 == 0, |_, _, _| 0);
}

// ---------------------------------------------------------------------------
// TranslucentSortCache — quantized lazy radix sort
// ---------------------------------------------------------------------------

/// Same distance-key quantization the cache applies (1/256-block units).
fn quant_key(center: [f32; 3], cam: [f32; 3]) -> u32 {
    let dx = center[0] - cam[0];
    let dy = center[1] - cam[1];
    let dz = center[2] - cam[2];
    let d2 = dx * dx + dy * dy + dz * dz;
    (d2.sqrt() * 256.0).min(65535.0) as u32
}

#[test]
fn sorter_radix_matches_stable_descending_order() {
    let cam = [32.0, 70.0, -16.0];
    let mut rng = Rng::new(0xCAFE_BABE);
    let quads: Vec<TranslucentQuadKey> = (0..4096u32)
        .map(|quad_idx| TranslucentQuadKey {
            quad_idx,
            center: [
                rng.f32(-200.0, 200.0),
                rng.f32(-200.0, 200.0),
                rng.f32(-200.0, 200.0),
            ],
        })
        .collect();
    let mut cache = TranslucentSortCache::new();
    assert_eq!(cache.sort(cam, 1, &quads), SortOutcome::Resorted);
    let order = cache.order();
    assert_eq!(order.len(), quads.len());
    // Same multiset of indices.
    let mut seen = order.to_vec();
    seen.sort_unstable();
    assert_eq!(seen, (0..4096u32).collect::<Vec<_>>());
    // Back-to-front: quantized keys are non-increasing, and equal keys
    // keep input order (the radix sort is stable).
    for w in order.windows(2) {
        let ka = quant_key(quads[w[0] as usize].center, cam);
        let kb = quant_key(quads[w[1] as usize].center, cam);
        assert!(ka >= kb, "must be back-to-front");
        if ka == kb {
            assert!(w[0] < w[1], "stable tie-break by input order");
        }
    }
    // Where every key is distinct the order equals a stable descending
    // sort on true distance exactly.
    let keys: BTreeSet<u32> = quads
        .iter()
        .map(|q| quant_key(q.center, cam))
        .collect();
    if keys.len() == quads.len() {
        let mut expect: Vec<u32> = (0..quads.len() as u32).collect();
        expect.sort_by(|&a, &b| {
            quant_key(quads[b as usize].center, cam)
                .cmp(&quant_key(quads[a as usize].center, cam))
        });
        assert_eq!(order, expect.as_slice());
    }
}

#[test]
fn sorter_caches_within_half_block_cell() {
    let cam = [8.0, 8.0, 8.0];
    let quads: Vec<TranslucentQuadKey> = (0..64u32)
        .map(|i| TranslucentQuadKey {
            quad_idx: i,
            center: [i as f32, 0.0, 0.0],
        })
        .collect();
    let mut cache = TranslucentSortCache::new();
    assert_eq!(cache.sort(cam, 1, &quads), SortOutcome::Resorted);
    let first = cache.order().to_vec();

    // Same cell + same revision -> zero work.
    assert_eq!(cache.sort(cam, 1, &quads), SortOutcome::Cached);
    assert_eq!(cache.order(), first.as_slice());
    // Sub-half-block camera motion stays cached.
    assert_eq!(cache.sort([8.4, 8.49, 8.0], 1, &quads), SortOutcome::Cached);
    // A new quad revision forces a re-sort.
    assert_eq!(cache.sort([8.4, 8.49, 8.0], 2, &quads), SortOutcome::Resorted);
    // Crossing the half-block cell boundary re-sorts too.
    assert_eq!(cache.sort([8.6, 8.49, 8.0], 2, &quads), SortOutcome::Resorted);
}

// ---------------------------------------------------------------------------
// RenderGraph — dead-pass culling, cycles, transient aliasing
// ---------------------------------------------------------------------------

#[test]
fn render_graph_culls_dead_passes_and_orders_live() {
    const MIB: u64 = 1 << 20;
    let mut g = RenderGraph::new();
    let shadow = g.add_transient_resource(ResourceDesc { bytes: 8 * MIB, kind_tag: 1 });
    let gbuf_a = g.add_transient_resource(ResourceDesc { bytes: 8 * MIB, kind_tag: 1 });
    let gbuf_b = g.add_transient_resource(ResourceDesc { bytes: 8 * MIB, kind_tag: 1 });
    let velocity = g.add_transient_resource(ResourceDesc { bytes: 4 * MIB, kind_tag: 1 });
    // Four sequential 16 MiB A-trous denoise temporaries — the classic
    // aliasing win: neighbours overlap, alternates share a slot.
    let dn_a = g.add_transient_resource(ResourceDesc { bytes: 16 * MIB, kind_tag: 1 });
    let dn_b = g.add_transient_resource(ResourceDesc { bytes: 16 * MIB, kind_tag: 1 });
    let dn_c = g.add_transient_resource(ResourceDesc { bytes: 16 * MIB, kind_tag: 1 });
    let dn_d = g.add_transient_resource(ResourceDesc { bytes: 16 * MIB, kind_tag: 1 });
    let hiz = g.add_transient_resource(ResourceDesc { bytes: 4 * MIB, kind_tag: 1 });
    let ssao = g.add_transient_resource(ResourceDesc { bytes: 4 * MIB, kind_tag: 1 });
    let hdr = g.add_transient_resource(ResourceDesc { bytes: 16 * MIB, kind_tag: 1 });
    let bloom = g.add_transient_resource(ResourceDesc { bytes: 4 * MIB, kind_tag: 1 });
    let ldr = g.add_transient_resource(ResourceDesc { bytes: 8 * MIB, kind_tag: 1 });
    let dead = g.add_transient_resource(ResourceDesc { bytes: MIB, kind_tag: 1 });

    let p_shadow = g.add_pass("Shadow", &[], &[shadow], false);
    let p_gbuf = g.add_pass("GBuffer", &[], &[gbuf_a, gbuf_b, velocity], false);
    let p_dn_a = g.add_pass("AtrousDenoise0", &[velocity], &[dn_a], false);
    let p_dn_b = g.add_pass("AtrousDenoise1", &[dn_a], &[dn_b], false);
    let p_dn_c = g.add_pass("AtrousDenoise2", &[dn_b], &[dn_c], false);
    let p_dn_d = g.add_pass("AtrousDenoise3", &[dn_c], &[dn_d], false);
    let p_hiz = g.add_pass("HiZ", &[gbuf_a], &[hiz], false);
    let p_ssao = g.add_pass("SSAO", &[gbuf_a, hiz, dn_d], &[ssao], false);
    let p_light = g.add_pass("Lighting", &[gbuf_a, gbuf_b, velocity, shadow, ssao], &[hdr], false);
    let p_bloom = g.add_pass("Bloom", &[hdr], &[bloom], false);
    let p_tone = g.add_pass("Tonemap", &[hdr, bloom], &[ldr], true);
    let p_ui = g.add_pass("UI", &[ldr], &[], true);
    let p_debug = g.add_pass("DebugOverlay", &[], &[dead], false);

    let plan = g.compile().expect("valid graph compiles");
    // The unreachable debug pass is culled.
    assert_eq!(plan.culled_passes, vec![p_debug]);
    assert_eq!(plan.order.len(), 12);

    // Topological order: every producer precedes its consumers.
    let pos = |p| plan.order.iter().position(|&x| x == p).unwrap();
    for (producer, consumer) in [
        (p_shadow, p_light),
        (p_gbuf, p_hiz),
        (p_gbuf, p_ssao),
        (p_gbuf, p_light),
        (p_dn_a, p_dn_b),
        (p_dn_b, p_dn_c),
        (p_dn_c, p_dn_d),
        (p_dn_d, p_ssao),
        (p_hiz, p_ssao),
        (p_ssao, p_light),
        (p_light, p_bloom),
        (p_light, p_tone),
        (p_bloom, p_tone),
        (p_tone, p_ui),
    ] {
        assert!(pos(producer) <= pos(consumer));
    }

    // Lifetimes cover every use.
    let (sf, sl) = plan.lifetime(shadow).unwrap();
    assert_eq!(sf as usize, pos(p_shadow));
    assert_eq!(sl as usize, pos(p_light));
    let (f, l) = plan.lifetime(hdr).unwrap();
    assert_eq!(f as usize, pos(p_light));
    assert_eq!(l as usize, pos(p_tone));
    assert!(plan.lifetime(dead).is_none());
    assert!(plan.slot(dead).is_none());

    // Slot sharing only across disjoint lifetimes with matching tags.
    for a in 0..g.resource_count() {
        for b in 0..a {
            if plan.aliased_slots[a] == plan.aliased_slots[b]
                && plan.aliased_slots[a].is_some()
            {
                let (fa, la) = plan.lifetimes[a].unwrap();
                let (fb, lb) = plan.lifetimes[b].unwrap();
                assert!(la < fb || lb < fa, "overlapping lifetimes may not alias");
            }
        }
    }

    // >= 40% VRAM saved by aliasing the realistic pipeline.
    assert_eq!(plan.peak_unaliased_bytes, 128 * MIB);
    assert!(plan.aliased_bytes < plan.peak_unaliased_bytes);
    assert_eq!(
        plan.vram_saved_bytes,
        plan.peak_unaliased_bytes - plan.aliased_bytes
    );
    assert!(
        plan.vram_saved_bytes * 100 >= plan.peak_unaliased_bytes * 40,
        "saved {} of {}",
        plan.vram_saved_bytes,
        plan.peak_unaliased_bytes
    );
}

#[test]
fn render_graph_rejects_cycles_and_unknown_resources() {
    let mut g = RenderGraph::new();
    let r1 = g.add_transient_resource(ResourceDesc { bytes: 16, kind_tag: 1 });
    let r2 = g.add_transient_resource(ResourceDesc { bytes: 16, kind_tag: 1 });
    g.add_pass("A", &[r2], &[r1], false);
    g.add_pass("B", &[r1], &[r2], false);
    g.add_pass("Root", &[r1], &[], true);
    assert!(matches!(g.compile(), Err(RenderGraphError::Cycle)));

    let mut g2 = RenderGraph::new();
    g2.add_pass("Bad", &[ResourceId(99)], &[], true);
    assert!(matches!(
        g2.compile(),
        Err(RenderGraphError::UnknownResource { .. })
    ));
}
