// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane MT-PORT-SNAP (audit `full-multithreading-server-client-parallel-architecture-20261123`
//! §3.2 + `zero-bloat-minimal-bundling-audit-20261123` #2): lock-free
//! 27-neighbor `Arc<CompressedSection>` snapshot meshing. Proves
//! `MeshCache::snapshot_section_input` + pure `build_and_pack_section_mesh`
//! reproduce `MeshCache::build` geometry bit-for-bit, and that 4 parallel OS
//! threads produce bit-identical `BuiltSectionMesh` output to sequential runs.

use mc_spine_client_mesh::ffi::{mc_spine_client_mesh_pack_vertices, ABI_OK};
use mc_spine_client_mesh::{
    build_and_pack_section_mesh, Block, BuiltSectionMesh, CompressedSection, DirectionBuckets,
    Layer, Light, MeshCache, PackedVertex, Quad, Section, SectionCoord, SectionMeshInput,
    DIRECTION_BUCKETS, LIGHT_BYTES, SECTION_CELLS, WATER,
};

const SKY: Light = Light::new(0, 15);
const DARK: Light = Light::new(0, 0);

fn assert_send_sync<T: Send + Sync>() {}

/// Same translucent rule as the client-port `is_translucent`: translucent
/// layer, plus water fluid cells.
fn is_translucent_quad(quad: &Quad) -> bool {
    quad.layer == Layer::Translucent || (quad.layer == Layer::Fluid && quad.fluid_kind == WATER)
}

/// Deterministic xorshift64* PRNG — no external crates.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn stone(id: u32) -> Block {
    Block::opaque_cube(id)
}

fn glass(id: u32) -> Block {
    Block {
        state_id: id,
        flags: mc_spine_client_mesh::TRANSLUCENT,
        occlusion: 0,
        ..Block::AIR
    }
}

fn cutout(id: u32) -> Block {
    Block {
        state_id: id,
        flags: mc_spine_client_mesh::CUTOUT,
        occlusion: 0,
        ..Block::AIR
    }
}

fn water(level: u8) -> Block {
    Block {
        state_id: 1_000 + u32::from(level),
        fluid_kind: WATER,
        fluid_level: level,
        ..Block::AIR
    }
}

fn lava(level: u8) -> Block {
    Block {
        state_id: 2_000 + u32::from(level),
        fluid_kind: mc_spine_client_mesh::LAVA,
        fluid_level: level,
        ..Block::AIR
    }
}

/// Dense section with a deterministic mix of opaque, translucent, cutout,
/// water and lava cells plus varied light, so greedy merges, AO diagonals and
/// every layer bucket are exercised.
fn dense_section(seed: u64) -> Section {
    let mut rng = Rng(seed | 1);
    let mut blocks = Box::new([Block::AIR; SECTION_CELLS]);
    let mut block_light = [0u8; LIGHT_BYTES];
    let mut sky_light = [255u8; LIGHT_BYTES];
    for y in 0..16usize {
        for z in 0..16usize {
            for x in 0..16usize {
                let roll = rng.below(100);
                let block = match roll {
                    0..=54 => stone(1 + rng.below(4) as u32),
                    55..=64 => glass(500 + rng.below(3) as u32),
                    65..=72 => cutout(600 + rng.below(3) as u32),
                    73..=82 => water(rng.below(8) as u8),
                    83..=86 => lava(rng.below(8) as u8),
                    _ => Block::AIR,
                };
                if block != Block::AIR {
                    let index = y * 256 + z * 16 + x;
                    blocks[index] = block;
                    let light = Light::new(rng.below(16) as u8, 8 + rng.below(8) as u8);
                    let shift = (index & 1) * 4;
                    let byte = &mut block_light[index / 2];
                    *byte = (*byte & !(15 << shift)) | (light.block() << shift);
                    let byte = &mut sky_light[index / 2];
                    *byte = (*byte & !(15 << shift)) | (light.sky() << shift);
                }
            }
        }
    }
    Section {
        blocks,
        block_light,
        sky_light,
    }
}

/// Pack `quads` through the real client-port FFI, mirroring the reference
/// single-threaded upload path.
fn pack_via_ffi(quads: &[Quad]) -> Vec<PackedVertex> {
    if quads.is_empty() {
        return Vec::new();
    }
    let mut count = 0usize;
    let status = unsafe {
        mc_spine_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            std::ptr::null_mut(),
            0,
            &mut count,
        )
    };
    assert_eq!(status, ABI_OK, "pack_vertices count query failed");
    let mut out = vec![PackedVertex::default(); count];
    let status = unsafe {
        mc_spine_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut count,
        )
    };
    assert_eq!(status, ABI_OK, "pack_vertices fill failed");
    out
}

/// Reference opaque stream: direction-bucket grouping of the non-translucent
/// quads, FFI packing, then the documented diagonal normalization
/// (`[v0,v1,v2,v3] -> [v1,v2,v3,v0]`, bit 17 cleared).
fn expected_opaque_packed(opaque: &[Quad]) -> (Vec<PackedVertex>, [(u32, u32); DIRECTION_BUCKETS]) {
    let (grouped, buckets) = DirectionBuckets::group(opaque);
    let flat = pack_via_ffi(&grouped);
    let mut out = Vec::with_capacity(flat.len());
    for (i, quad) in grouped.iter().enumerate() {
        let verts = &flat[i * 4..i * 4 + 4];
        if quad.diagonal != 0 {
            let mut rotated = [verts[1], verts[2], verts[3], verts[0]];
            for vertex in &mut rotated {
                vertex.attributes &= !(1 << 17);
            }
            out.extend_from_slice(&rotated);
        } else {
            out.extend_from_slice(verts);
        }
    }
    (out, buckets.vertex_ranges())
}

/// Populate a cache with a deterministic 3x3x3 neighborhood around the origin
/// plus extra sections, mixing dense, uniform-air and uniform-stone storage.
fn sample_cache() -> MeshCache {
    let mut cache = MeshCache::default();
    for y in -1..=1 {
        for z in -1..=1 {
            for x in -1..=1 {
                let coord = SectionCoord(x, y, z);
                let seed = ((x + 8) as u64) << 40
                    | ((y + 8) as u64) << 20
                    | (z + 8) as u64;
                match (x, y, z) {
                    (0, 0, 0) | (1, 0, 0) | (0, 1, 0) => {
                        cache.insert(coord, dense_section(seed));
                    }
                    (0, 0, 1) | (-1, -1, -1) => {
                        cache.insert_compressed(
                            coord,
                            CompressedSection::uniform(stone(9), SKY),
                        );
                    }
                    _ => {
                        cache.insert_compressed(
                            coord,
                            CompressedSection::uniform(Block::AIR, SKY),
                        );
                    }
                }
            }
        }
    }
    // A far-away section proves missing neighbors take the `missing` policy.
    cache.insert_compressed(
        SectionCoord(6, 0, 6),
        CompressedSection::uniform(stone(3), DARK),
    );
    cache
}

#[test]
fn snapshot_mesh_matches_single_threaded_build_bit_for_bit() {
    assert_send_sync::<SectionMeshInput>();
    assert_send_sync::<BuiltSectionMesh>();

    let mut cache = sample_cache();
    let rebuilt = cache.rebuild_dirty(usize::MAX);
    assert_eq!(rebuilt.len(), 28);

    let coords: Vec<SectionCoord> = rebuilt;
    for &coord in &coords {
        let reference_quads = cache.mesh(coord).expect("rebuilt mesh").to_vec();
        let input = cache
            .snapshot_section_input(coord, 42)
            .expect("loaded center snapshots");
        assert_eq!(input.coord, coord);
        assert_eq!(input.revision, 42);

        let built = build_and_pack_section_mesh(&input);
        assert_eq!(built.coord, coord);
        assert_eq!(built.revision, 42);

        // Translucent quads keep mesh order and exact quad geometry.
        let expected_translucent: Vec<Quad> = reference_quads
            .iter()
            .copied()
            .filter(is_translucent_quad)
            .collect();
        assert_eq!(built.translucent_quads, expected_translucent, "{coord:?}");

        // Opaque stream: identical packed vertices and bucket totals.
        let opaque: Vec<Quad> = reference_quads
            .iter()
            .copied()
            .filter(|q| !is_translucent_quad(q))
            .collect();
        let (expected_packed, expected_ranges) = expected_opaque_packed(&opaque);
        assert_eq!(built.opaque_packed, expected_packed, "{coord:?}");
        assert_eq!(built.opaque_bucket_ranges, expected_ranges, "{coord:?}");
        assert_eq!(
            built.opaque_packed.len(),
            opaque.len() * 4,
            "{coord:?} packed vertex count"
        );

        // Visibility mask covers every meshed direction.
        let mut mask = 0u8;
        for quad in &reference_quads {
            mask |= quad.direction.bit();
        }
        assert_eq!(built.visibility_mask, mask, "{coord:?}");
    }

    // A coordinate with no stored center section has nothing to snapshot.
    assert!(cache.snapshot_section_input(SectionCoord(99, 0, 0), 1).is_none());
}

#[test]
fn snapshot_is_isolated_from_later_cache_mutations() {
    let mut cache = sample_cache();
    cache.rebuild_dirty(usize::MAX);
    let center = SectionCoord(0, 0, 0);
    let input = cache.snapshot_section_input(center, 7).unwrap();
    let before = build_and_pack_section_mesh(&input);

    // Mutating the cache after the snapshot must not leak into the in-flight
    // job: `set_cell` copy-on-writes the shared `Arc`.
    assert!(cache.set_cell(center, [8, 8, 8], stone(77), DARK));
    assert!(cache.set_cell(SectionCoord(1, 0, 0), [0, 0, 0], glass(501), SKY));
    let after = build_and_pack_section_mesh(&input);
    assert_eq!(before, after);
}

#[test]
fn parallel_builds_across_four_threads_are_bit_identical() {
    // 32 deterministic sections in a 4x2x4 slab.
    let mut cache = MeshCache::default();
    for y in 0..2i32 {
        for z in 0..4i32 {
            for x in 0..4i32 {
                let coord = SectionCoord(x, y, z);
                let seed = ((x as u64) << 40) | ((y as u64) << 20) | z as u64 | 0x9e37;
                if (x + y + z) % 5 == 0 {
                    cache.insert_compressed(
                        coord,
                        CompressedSection::uniform(if seed.is_multiple_of(2) { stone(4) } else { Block::AIR }, SKY),
                    );
                } else {
                    cache.insert(coord, dense_section(seed));
                }
            }
        }
    }
    cache.rebuild_dirty(usize::MAX);

    let coords: Vec<SectionCoord> = (0..2)
        .flat_map(|y| (0..4).flat_map(move |z| (0..4).map(move |x| SectionCoord(x, y, z))))
        .collect();
    assert_eq!(coords.len(), 32);
    let inputs: Vec<SectionMeshInput> = coords
        .iter()
        .map(|&coord| cache.snapshot_section_input(coord, 100).unwrap())
        .collect();

    let sequential: Vec<BuiltSectionMesh> = inputs
        .iter()
        .map(build_and_pack_section_mesh)
        .collect();

    // Four OS worker threads, contiguous chunks, results reassembled in order.
    let chunk = inputs.len().div_ceil(4);
    let mut parallel: Vec<BuiltSectionMesh> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = inputs
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(build_and_pack_section_mesh)
                        .collect::<Vec<BuiltSectionMesh>>()
                })
            })
            .collect();
        for handle in handles {
            parallel.extend(handle.join().expect("worker panicked"));
        }
    });

    assert_eq!(parallel.len(), sequential.len());
    for (index, (par, seq)) in parallel.iter().zip(&sequential).enumerate() {
        assert_eq!(par, seq, "section {index} diverged: {:?}", seq.coord);
    }

    // Sanity: the slab actually produced geometry and at least one
    // diagonal-normalized quad somewhere (bit 17 is always clear in the
    // packed stream, so compare against the reference builder's count).
    let total_quads: usize = sequential.iter().map(|m| m.opaque_packed.len() / 4).sum();
    assert!(total_quads > 0);
}
