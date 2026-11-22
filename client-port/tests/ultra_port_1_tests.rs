// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane ULTRA-PORT-1 (audit `ultra-50k-fps-1k-low-500mib-plan-20261122`):
//! `[FPS-2]` O(1) dirty mesh coord queue + Priority-0 near-player rebuild,
//! `[FPS-5]` `HardFloor1000Fps` upload budget profile,
//! `[MEM-1]` `Uniform(Block)` section compression + distance eviction +
//! post-upload opaque CPU-mesh release.

use mc_spine_client_mesh::{
    mesh_section, Block, ChunkUploadBudget, CompressedSection, Light, MeshCache, Quad, Section,
    SectionBlocks, SectionCoord, UploadBudget, UploadBudgetProfile, WorldState,
    HARD_FLOOR_1000FPS_MAX_UPLOADS_PER_FRAME, HARD_FLOOR_1000FPS_MAX_UPLOAD_BYTES_PER_FRAME,
    HARD_FLOOR_1000FPS_MAX_UPLOAD_WORK_US_PER_FRAME, PRIORITY_ZERO_NEAR_RADIUS_BLOCKS,
    SECTION_CELLS, TRANSLUCENT,
};

const SKY_LIT: Light = Light::new(0, 15);
const UNLIT: Light = Light::new(0, 0);

fn stone() -> Block {
    Block::opaque_cube(7)
}

fn glass() -> Block {
    Block {
        state_id: 500,
        flags: TRANSLUCENT,
        ..Block::AIR
    }
}

/// MEM-1 §3.2: uniform sections hold zero block-array heap bytes, promote on
/// the first non-uniform write, collapse back when uniform again, and keep
/// 100% identical get/set/mesh semantics.
#[test]
fn uniform_section_compression_uses_zero_heap_blocks_and_promotes_losslessly() {
    // One hundred uniform air/stone sections: zero block heap bytes each.
    let mut world = WorldState::new();
    for i in 0..50 {
        assert!(world.insert_compressed(
            SectionCoord(i, 0, 0),
            CompressedSection::uniform(Block::AIR, SKY_LIT),
        ));
    }
    for i in 50..100 {
        assert!(world.insert_compressed(
            SectionCoord(i, 0, 0),
            CompressedSection::uniform(stone(), UNLIT),
        ));
    }
    for i in 0..100 {
        let coord = SectionCoord(i, 0, 0);
        assert_eq!(world.is_uniform_section(coord), Some(true), "{coord:?}");
        assert_eq!(world.blocks_heap_bytes(coord), Some(0), "{coord:?}");
    }

    // A plain `Section::filled` insert lands in the same compressed form.
    let mut cache = MeshCache::default();
    let coord = SectionCoord(0, 0, 0);
    cache.insert(coord, Section::filled(stone(), UNLIT));
    assert_eq!(cache.is_uniform_section(coord), Some(true));
    assert_eq!(cache.blocks_heap_bytes(coord), Some(0));

    // Mutating one cell promotes to Dense and preserves the exact state.
    let target = [3, 9, 5];
    assert!(cache.set_cell(coord, target, Block::opaque_cube(42), UNLIT));
    assert_eq!(cache.is_uniform_section(coord), Some(false));
    assert_eq!(
        cache.blocks_heap_bytes(coord),
        Some(SECTION_CELLS * std::mem::size_of::<Block>())
    );
    let stored = cache.section(coord).expect("stored section");
    assert_eq!(
        stored.get(target[0], target[1], target[2]).block,
        Block::opaque_cube(42)
    );
    assert_eq!(stored.get(0, 0, 0).block, stone());

    // Losslessness: the expanded snapshot equals a dense Section built with
    // the identical writes.
    let mut reference = Section::filled(stone(), UNLIT);
    assert!(reference.set(
        target[0],
        target[1],
        target[2],
        Block::opaque_cube(42),
        UNLIT
    ));
    assert_eq!(stored.expand(), reference);
    let SectionBlocks::Dense(_) = &stored.blocks else {
        panic!("promoted storage must be Dense");
    };

    // Meshing parity: the isolated section's rebuilt mesh equals the
    // canonical `mesh_section` output of the identical dense snapshot.
    assert_eq!(cache.rebuild_dirty(usize::MAX), vec![coord]);
    let expected = mesh_section(&reference.view());
    assert_eq!(cache.mesh(coord), Some(expected.as_slice()));

    // Restoring the cell collapses the storage back to zero-heap Uniform.
    assert!(cache.set_cell(coord, target, stone(), UNLIT));
    assert_eq!(cache.is_uniform_section(coord), Some(true));
    assert_eq!(cache.blocks_heap_bytes(coord), Some(0));
    assert_eq!(
        cache.section(coord).unwrap().expand(),
        Section::filled(stone(), UNLIT)
    );
}

/// FPS-2: the dirty-mesh-coord queue reports exactly the rebuilt sections in
/// O(1) each (no world scan, no vertex-slice compare), and the Priority-0
/// lane rebuilds only the near-player section before any distant work.
#[test]
fn dirty_mesh_coords_queue_is_o1_and_priority_zero_rebuilds_near_player_first() {
    let mut world = WorldState::new();
    let camera = [8.0, 8.0, 8.0]; // inside section (0, 0, 0)
    let near = SectionCoord(0, 0, 0);
    // 50 distant dirty sections (x = 160.. blocks away) + 1 near edit.
    let far: Vec<SectionCoord> = (10..60).map(|x| SectionCoord(x, 0, 0)).collect();
    for &coord in &far {
        world.insert_compressed(coord, CompressedSection::uniform(stone(), UNLIT));
    }
    world.insert_compressed(near, CompressedSection::uniform(stone(), UNLIT));
    assert_eq!(world.pending(), 51);

    // Priority 0 rebuilds ONLY the near section; the 50 far ones stay dirty.
    let rebuilt = world.rebuild_dirty_priority_zero(camera, PRIORITY_ZERO_NEAR_RADIUS_BLOCKS, 8);
    assert_eq!(rebuilt, vec![near]);
    assert_eq!(world.pending(), 50);
    assert!(world.mesh(near).is_some());
    for &coord in &far {
        assert!(world.mesh(coord).is_none(), "{coord:?} must stay dirty");
    }

    // The queue yields exactly the rebuilt coord — no untouched sections —
    // then drains empty.
    assert_eq!(world.take_dirty_mesh_coords(), vec![near]);
    assert!(world.take_dirty_mesh_coords().is_empty());

    // Rebuilding the same section twice queues it once (deduplicated).
    world.set_cell(near, [0, 0, 0], Block::opaque_cube(9), UNLIT);
    let again = world.rebuild_dirty_priority_zero(camera, 16.0, 8);
    assert_eq!(again, vec![near]);
    world.set_cell(near, [1, 0, 0], Block::opaque_cube(11), UNLIT);
    let third = world.rebuild_dirty_priority_zero(camera, 16.0, 8);
    assert_eq!(third, vec![near]);
    assert_eq!(world.take_dirty_mesh_coords(), vec![near]);

    // The remaining far sections drain through the normal bounded path and
    // are each reported exactly once.
    let rest = world.rebuild_dirty(usize::MAX);
    assert_eq!(rest.len(), 50);
    let mut queued = world.take_dirty_mesh_coords();
    queued.sort_unstable();
    let mut expected = far.clone();
    expected.sort_unstable();
    assert_eq!(queued, expected);
    assert!(world.take_dirty_mesh_coords().is_empty());
}

/// FPS-5 + MEM-1 §3.1/§4-② + §3.2-evict: the hard floor caps a 200-section
/// burst at 2 sections / 128 KiB / 150 µs per frame without dropping any,
/// the opaque CPU drop frees quad bytes while keeping metadata and
/// translucent sort data, and the radius governor reclaims far chunks.
#[test]
fn hard_floor_1000fps_budget_caps_burst_and_cpu_drop_frees_opaque_bytes() {
    // FPS-5 profile constants.
    let mut budget = UploadBudget::hard_floor_1000fps();
    assert_eq!(
        budget.max_uploads_per_frame,
        HARD_FLOOR_1000FPS_MAX_UPLOADS_PER_FRAME
    );
    assert_eq!(budget.max_uploads_per_frame, 2);
    assert_eq!(
        budget.max_upload_bytes_per_frame,
        HARD_FLOOR_1000FPS_MAX_UPLOAD_BYTES_PER_FRAME
    );
    assert_eq!(budget.max_upload_bytes_per_frame, 128 * 1024);
    assert_eq!(
        budget.max_upload_work_us_per_frame,
        HARD_FLOOR_1000FPS_MAX_UPLOAD_WORK_US_PER_FRAME
    );
    assert_eq!(budget.max_upload_work_us_per_frame, 150);
    assert_eq!(budget.target_frame_us, 1_000);
    // Plan-named accessors mirror the same three caps.
    assert_eq!(budget.max_bytes_per_frame(), 128 * 1024);
    assert_eq!(budget.max_sections_per_frame(), 2);
    assert_eq!(budget.max_cost_us(), 150);
    let via_profile = ChunkUploadBudget::with_profile(UploadBudgetProfile::HardFloor1000Fps);
    assert_eq!(via_profile.max_upload_work_us_per_frame, 150);

    // Byte and µs allowances gate independently of the count cap.
    budget.begin_frame();
    assert!(budget.try_schedule(96 * 1024, 60));
    assert!(!budget.try_schedule(48 * 1024, 60)); // 144 KiB > 128 KiB
    assert!(budget.try_schedule(32 * 1024, 60)); // 128 KiB total, exactly
    budget.begin_frame();
    assert!(budget.try_schedule(1024, 100));
    assert!(!budget.try_schedule(1024, 60)); // 160 µs > 150 µs
    assert!(budget.try_schedule(1024, 50)); // 150 µs exactly

    // A 200-section burst drains at <=2 sections / <=128 KiB / <=150 µs per
    // frame; nothing is ever dropped, it just defers to the next frame.
    let mut admitted = 0usize;
    let mut frames = 0usize;
    while admitted < 200 {
        budget.begin_frame();
        frames += 1;
        assert!(frames <= 200, "queue must keep draining");
        while budget.try_schedule(64 * 1024, 60) {
            admitted += 1;
        }
        assert!(budget.used_uploads() <= 2);
        assert!(budget.used_bytes() <= 128 * 1024);
        assert!(budget.used_work_us() <= 150);
    }
    assert_eq!(admitted, 200);
    assert!(frames >= 100, "2/frame needs at least 100 frames");

    // MEM-1 §3.1 + §4-②: releasing an uploaded opaque mesh frees its CPU
    // quad bytes and keeps full metadata.
    let mut world = WorldState::new();
    let opaque = SectionCoord(0, 0, 0);
    world.insert_section(opaque, Section::filled(stone(), SKY_LIT));
    world.rebuild_dirty(usize::MAX);
    let opaque_quads = world.mesh(opaque).unwrap().len();
    let opaque_mesh_bytes = std::mem::size_of_val(world.mesh(opaque).unwrap());
    assert!(opaque_quads > 0);
    let freed = world.release_uploaded_opaque_cpu_mesh(opaque);
    assert_eq!(freed, opaque_mesh_bytes);
    assert_eq!(world.mesh(opaque), Some([].as_slice()));
    let meta = world.released_mesh_meta(opaque).copied().unwrap();
    assert_eq!(meta.quad_count, opaque_quads);
    assert_eq!(meta.vertex_count, opaque_quads * 4);
    assert_eq!(meta.index_count, opaque_quads * 6);
    assert_eq!(meta.kept_translucent_quads, 0);
    assert_ne!(meta.direction_mask, 0);
    for axis in 0..3 {
        assert!(meta.aabb_min[axis] >= 0.0 && meta.aabb_max[axis] <= 16.0);
    }
    // Releasing again frees nothing more.
    assert_eq!(world.release_uploaded_opaque_cpu_mesh(opaque), 0);
    // A dirty section (fresh edit) cannot release: VRAM is stale.
    world.set_cell(opaque, [0, 0, 0], Block::opaque_cube(9), SKY_LIT);
    assert_eq!(world.release_uploaded_opaque_cpu_mesh(opaque), 0);

    // Translucent sections keep their re-sort quads; opaque-only sections in
    // a mixed mesh drop just the opaque share.
    let translucent = SectionCoord(4, 0, 0);
    world.insert_section(translucent, Section::filled(glass(), SKY_LIT));
    world.rebuild_dirty(usize::MAX);
    let t_quads = world.mesh(translucent).unwrap().len();
    assert!(t_quads > 0);
    assert_eq!(world.release_uploaded_opaque_cpu_mesh(translucent), 0);
    let meta = world.released_mesh_meta(translucent).copied().unwrap();
    assert_eq!(meta.quad_count, t_quads);
    assert_eq!(meta.kept_translucent_quads, t_quads);
    assert_eq!(world.mesh(translucent).unwrap().len(), t_quads);

    let mut mixed_section = Section::filled(stone(), SKY_LIT);
    for x in 0..4 {
        for z in 0..4 {
            for y in 0..16 {
                mixed_section.set(x, y, z, glass(), SKY_LIT);
            }
        }
    }
    let mixed = SectionCoord(8, 0, 0);
    world.insert_section(mixed, mixed_section);
    world.rebuild_dirty(usize::MAX);
    let total = world.mesh(mixed).unwrap().len();
    let freed = world.release_uploaded_opaque_cpu_mesh(mixed);
    let meta = world.released_mesh_meta(mixed).copied().unwrap();
    assert!(meta.kept_translucent_quads > 0);
    assert_eq!(
        freed,
        (total - meta.kept_translucent_quads) * std::mem::size_of::<Quad>()
    );
    assert_eq!(
        world.mesh(mixed).unwrap().len(),
        meta.kept_translucent_quads
    );

    // MEM-1 §3.2 distance governor: a 7x7 column grid, evict to radius 2.
    let mut far_world = WorldState::new();
    for cx in -3..=3 {
        for cz in -3..=3 {
            far_world.insert_compressed(
                SectionCoord(cx, 0, cz),
                CompressedSection::uniform(stone(), UNLIT),
            );
        }
    }
    assert_eq!(far_world.sections_loaded(), 49);
    far_world.rebuild_dirty(usize::MAX);
    let evicted = far_world.evict_chunks_outside_radius(0, 0, 2);
    assert_eq!(evicted, 24); // 49 - 5x5 kept
    assert_eq!(far_world.sections_loaded(), 25);
    assert!(far_world.section(SectionCoord(3, 0, 3)).is_none());
    assert!(far_world.mesh(SectionCoord(3, 0, 3)).is_none());
    assert!(far_world.section(SectionCoord(0, 0, 0)).is_some());
}
