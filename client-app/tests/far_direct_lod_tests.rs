// SPDX-License-Identifier: LGPL-3.0-or-later
//! Ring ownership, deterministic direct sampling, skirts, and bounded work/cache.

use megu_client::far_direct_lod::{
    FarDirectLodConfig, FarDirectLodEngine, FarHeightBiomeSampler, FarHeightSample, FarLodQuad,
    FarTileKey, LodRingLevel, ProceduralFarSampler, FULL_DETAIL_RADIUS_CHUNKS,
    RING_OUTER_RADII_CHUNKS,
};

#[derive(Clone, Copy)]
struct FlatSampler;

impl FarHeightBiomeSampler for FlatSampler {
    fn sample(&self, x: i64, z: i64, _stride: u8) -> FarHeightSample {
        FarHeightSample {
            surface_y: ((x ^ z) & 7) as i32,
            sea_level: 63,
            top_color_rgba: 0x12_34_56_ff,
            water_depth: 0,
        }
    }
}

fn build_first_tile() -> megu_client::far_direct_lod::FarLodTile {
    let mut engine = FarDirectLodEngine::new(FarDirectLodConfig {
        max_tiles_per_frame: 1,
        ..FarDirectLodConfig::default()
    });
    engine.set_camera(0, 0);
    assert_eq!(engine.build_frame(&ProceduralFarSampler::default()), 1);
    let key = FarTileKey {
        level: LodRingLevel::Level0,
        tile_x: -32,
        tile_z: -32,
    };
    engine
        .tile(key)
        .expect("first scheduled level-0 tile")
        .clone()
}

#[test]
fn full_detail_and_five_rings_partition_distances_through_1024_chunks() {
    assert_eq!(
        LodRingLevel::ALL.map(LodRingLevel::stride_blocks),
        [2, 4, 8, 16, 32]
    );
    assert_eq!(RING_OUTER_RADII_CHUNKS, [64, 128, 256, 512, 1024]);
    for distance in 0..=1024 {
        let full_detail = distance <= FULL_DETAIL_RADIUS_CHUNKS;
        let ring_owners = LodRingLevel::ALL
            .into_iter()
            .filter(|level| level.contains_distance(distance))
            .count();
        assert_eq!(
            usize::from(full_detail) + ring_owners,
            1,
            "distance {distance}"
        );
        assert_eq!(
            LodRingLevel::for_distance(distance).is_some(),
            !full_detail,
            "distance {distance}"
        );
    }
    assert_eq!(LodRingLevel::for_distance(1025), None);
}

#[test]
fn procedural_mesh_is_deterministic_and_has_downward_boundary_skirts() {
    let a = build_first_tile();
    let b = build_first_tile();
    assert_eq!(a, b);
    assert_eq!(a.quads.len(), 16 * 16 + 16 * 4);
    let skirts: Vec<&FarLodQuad> = a.quads.iter().filter(|quad| quad.face != 0).collect();
    assert_eq!(skirts.len(), 64);
    assert!(skirts.iter().all(|quad| quad.lower_y < quad.y));
    assert!(skirts.iter().all(|quad| (1..=4).contains(&quad.face)));
    assert!(a
        .quads
        .iter()
        .filter(|quad| quad.face == 0)
        .all(|quad| quad.lower_y == quad.y));
}

#[test]
fn lru_eviction_keeps_memory_at_or_below_the_strict_cap() {
    let tile_bytes = std::mem::size_of::<megu_client::far_direct_lod::FarLodTile>()
        + 320 * std::mem::size_of::<FarLodQuad>();
    let cap = tile_bytes * 2;
    let mut engine = FarDirectLodEngine::new(FarDirectLodConfig {
        max_memory_bytes: cap,
        max_tiles_per_frame: 1,
        ..FarDirectLodConfig::default()
    });
    engine.set_camera(0, 0);
    assert_eq!(engine.build_frame(&FlatSampler), 1);
    assert_eq!(engine.build_frame(&FlatSampler), 1);
    let recently_used = FarTileKey {
        level: LodRingLevel::Level0,
        tile_x: -32,
        tile_z: -32,
    };
    assert!(engine.tile(recently_used).is_some());
    assert_eq!(engine.build_frame(&FlatSampler), 1);
    let stats = engine.stats();
    assert!(stats.evictions > 0);
    assert!(stats.memory_bytes <= cap);
    assert!(
        engine.tile(recently_used).is_some(),
        "recently used tile should outlive older entries"
    );
    assert_eq!(stats.memory_bytes, stats.cached_tiles * tile_bytes);
}

#[test]
fn each_frame_respects_the_incremental_tile_build_budget() {
    let mut engine = FarDirectLodEngine::new(FarDirectLodConfig {
        max_tiles_per_frame: 3,
        ..FarDirectLodConfig::default()
    });
    engine.set_camera(0, 0);
    assert_eq!(engine.build_frame(&FlatSampler), 3);
    assert_eq!(engine.stats().built_last_frame, 3);
    assert!(engine.stats().queued_tiles > 0);
    assert_eq!(engine.build_frame(&FlatSampler), 3);
    assert_eq!(engine.stats().built_last_frame, 3);
}
