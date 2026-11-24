// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::BTreeMap;
use std::mem::size_of;
use std::path::Path;

use megu_kernel::occlusion_table::{
    BlockLightState, BlockShapeKind, CoalescedLightQueue, Direction, HorizontalFacing,
    PackedLightProps, PrebakedLightTable, QueuedLightEdit, BLOCKS_SKYLIGHT, DOWN, EAST,
    IS_OPAQUE_FULL_CUBE, UP, WEST,
};
use megu_kernel::{Error, Registry};

fn states() -> Vec<BlockLightState> {
    use BlockShapeKind as Shape;
    use HorizontalFacing as Facing;

    let mut states = vec![
        BlockLightState::new(Shape::Air, 0, 0),
        BlockLightState::new(Shape::FullOpaque, 0, 15),
        BlockLightState::new(Shape::Transparent, 0, 0),
        BlockLightState::new(Shape::BottomSlab, 0, 0),
        BlockLightState::new(Shape::TopSlab, 0, 0),
    ];
    for facing in [Facing::North, Facing::East, Facing::South, Facing::West] {
        for half_top in [false, true] {
            states.push(BlockLightState::new(
                Shape::Stair { facing, half_top },
                0,
                0,
            ));
        }
    }
    states.extend([
        BlockLightState::new(Shape::Leaves, 0, 1),
        BlockLightState::new(Shape::WaterloggedOpaque, 0, 15),
    ]);
    states
}

// Independent scalar geometry evaluation in world-space half-block cells.
// Unlike the implementation, this does not project or pack octant bitmasks.
fn occupied(shape: BlockShapeKind, x: usize, y: usize, z: usize) -> bool {
    match shape {
        BlockShapeKind::Air
        | BlockShapeKind::Transparent
        | BlockShapeKind::Leaves
        | BlockShapeKind::Partial => false,
        BlockShapeKind::FullOpaque | BlockShapeKind::WaterloggedOpaque => true,
        BlockShapeKind::BottomSlab => y == 0,
        BlockShapeKind::TopSlab => y == 1,
        BlockShapeKind::Stair { facing, half_top } => {
            (y == usize::from(half_top))
                || ((y != usize::from(half_top))
                    && match facing {
                        HorizontalFacing::North => z == 0,
                        HorizontalFacing::South => z == 1,
                        HorizontalFacing::West => x == 0,
                        HorizontalFacing::East => x == 1,
                    })
        }
    }
}

fn scalar_face(shape: BlockShapeKind, dir: Direction) -> u8 {
    let mut quadrants = 0u8;
    for first in 0..2 {
        for second in 0..2 {
            // Face quadrants use the same world axes on opposing faces.
            let (x, y, z) = match dir {
                Direction::Down => (first, 0, second),
                Direction::Up => (first, 1, second),
                Direction::North => (first, second, 0),
                Direction::South => (first, second, 1),
                Direction::West => (0, second, first),
                Direction::East => (1, second, first),
            };
            if occupied(shape, x, y, z) {
                quadrants |= 1 << (second * 2 + first);
            }
        }
    }
    quadrants
}

fn scalar_passes(from: BlockShapeKind, to: BlockShapeKind, dir: Direction) -> bool {
    scalar_face(from, dir) | scalar_face(to, dir.opposite()) != 0x0f
}

#[test]
fn every_shape_pair_matches_independent_scalar_face_union() {
    let states = states();
    // The requested named set contains 15 shapes (7 non-stairs + 8 stairs),
    // so exhaustive coverage is 15 * 15 * 6 = 1,350 directional pairs.
    assert_eq!(states.len(), 15);
    let table = PrebakedLightTable::from_shapes(&states);
    assert_eq!(table.len(), 15);
    let mut comparisons = 0;
    for (from, from_state) in states.iter().enumerate() {
        for (to, to_state) in states.iter().enumerate() {
            for dir in Direction::ALL {
                assert_eq!(
                    table.light_passes_between(from as u32, to as u32, dir),
                    scalar_passes(from_state.shape, to_state.shape, dir),
                    "from {from_state:?} to {to_state:?} through {dir:?}"
                );
                comparisons += 1;
            }
        }
    }
    assert_eq!(comparisons, 1_350);
}

#[test]
fn face_union_requires_quadrant_cache_beyond_six_full_face_bits() {
    let table = PrebakedLightTable::from_shapes(&states());
    assert_eq!(table.get(3).face_mask() & WEST, 0);
    assert_eq!(table.get(4).face_mask() & EAST, 0);
    // Two complementary partial faces seal completely in the scalar engine.
    assert!(!table.light_passes_between(3, 4, Direction::West));
    assert!(!table.light_passes_between(4, 3, Direction::East));
    assert!(table.light_passes_between(3, 3, Direction::West));
}

#[test]
fn compact_properties_flags_nibbles_and_unknown_states() {
    assert_eq!(size_of::<PackedLightProps>(), 2);
    let props = PackedLightProps::new(DOWN | UP, true, true, 11, 9);
    assert_eq!(
        props.as_bytes(),
        [DOWN | UP | IS_OPAQUE_FULL_CUBE | BLOCKS_SKYLIGHT, 0x9b]
    );
    assert_eq!(props.emission(), 11);
    assert_eq!(props.opacity(), 9);
    assert!(props.is_opaque_full_cube());
    assert!(props.blocks_skylight());
    assert_eq!(
        PackedLightProps::new(0, false, false, 255, 255).as_bytes(),
        [0, 0xff]
    );

    let table = PrebakedLightTable::from_shapes(&states());
    assert_eq!(table.get(u32::MAX), PackedLightProps::AIR);
    assert_eq!(table.get(0), PackedLightProps::AIR);
    assert!(table.get(1).is_opaque_full_cube());
    assert_eq!(table.get(1).face_mask(), 0x3f);
    assert!(table.get(13).blocks_skylight());
    for dir in Direction::ALL {
        assert!(table.light_passes_between(u32::MAX, u32::MAX, dir));
        assert!(table.light_passes_between(u32::MAX, 0, dir));
        assert!(!table.light_passes_between(u32::MAX, 1, dir));
    }
}

/// The pinned pack, or `None` on a strict-public export where the
/// generated `assets/*_1211*` files are absent — a skip, never a silent
/// pass: any error other than the missing pack still panics.
fn pinned_registry() -> Option<Registry> {
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets"));
    if !dir.join("blocks_1211.json").is_file() {
        eprintln!("pinned assets absent (strict-public export); skipped");
        return None;
    }
    Some(Registry::from_version_dir(dir).unwrap())
}

#[test]
fn registry_table_matches_existing_light_fact_masks() {
    let Some(registry) = pinned_registry() else {
        return;
    };
    let table = PrebakedLightTable::from_registry(&registry);
    assert_eq!(table.len(), registry.state_count());

    let blocks = [
        "minecraft:air",
        "minecraft:stone",
        "minecraft:glass",
        "minecraft:oak_leaves",
        "minecraft:oak_slab",
        "minecraft:oak_stairs",
        "minecraft:water",
        "minecraft:glowstone",
    ];
    let ids: Vec<u32> = blocks
        .iter()
        .map(|name| registry.default_state(name).unwrap())
        .collect();
    let legacy_face_index = |dir: Direction| match dir {
        Direction::Down => 4,
        Direction::Up => 5,
        Direction::North => 2,
        Direction::South => 3,
        Direction::West => 0,
        Direction::East => 1,
    };

    let mut comparisons = 0;
    for &from in &ids {
        for &to in &ids {
            let source = registry.facts(from).unwrap();
            let dest = registry.facts(to).unwrap();
            for dir in Direction::ALL {
                let blocked = (source.face_occlusion[legacy_face_index(dir)]
                    | dest.face_occlusion[legacy_face_index(dir.opposite())])
                    == 0x0f;
                assert_eq!(table.light_passes_between(from, to, dir), !blocked);
                comparisons += 1;
            }
        }
    }
    assert_eq!(comparisons, ids.len() * ids.len() * 6);
}

#[test]
fn leaf_decay_burst_is_last_write_wins_and_one_batch_per_chunk() {
    let mut queue = CoalescedLightQueue::new();
    type CellMap = BTreeMap<(i32, i32, i32), u32>;
    let mut expected: BTreeMap<(i32, i32), CellMap> = BTreeMap::new();

    for event in 0..2_300 {
        let cell = event % 617;
        let edit = QueuedLightEdit {
            x: cell % 49 - 24,
            y: 65 + cell / 49,
            z: (cell * 7) % 61 - 30,
            state_id: if event % 3 == 0 { 0 } else { 17 + event as u32 },
        };
        let chunk = (edit.x.div_euclid(16), edit.z.div_euclid(16));
        expected
            .entry(chunk)
            .or_default()
            .insert((edit.x, edit.y, edit.z), edit.state_id);
        queue.push(edit);
    }

    assert_eq!(queue.len(), 617);
    assert_eq!(queue.chunk_count(), expected.len());
    let batches = queue.drain();
    assert_eq!(batches.len(), expected.len());
    assert_eq!(
        batches.iter().map(|batch| batch.edits.len()).sum::<usize>(),
        617
    );
    for batch in batches {
        let actual: BTreeMap<_, _> = batch
            .edits
            .into_iter()
            .map(|edit| ((edit.x, edit.y, edit.z), edit.state_id))
            .collect();
        assert_eq!(actual, expected.remove(&batch.chunk).unwrap());
    }
    assert!(expected.is_empty());
    assert!(queue.is_empty());
    assert_eq!(queue.chunk_count(), 0);
    assert!(queue.drain().is_empty());

    queue.push(QueuedLightEdit {
        x: -1,
        y: 0,
        z: -1,
        state_id: 1,
    });
    assert_eq!(queue.drain()[0].chunk, (-1, -1));
}

#[test]
fn coalesced_edits_convert_to_existing_light_engine_facts() {
    let Some(registry) = pinned_registry() else {
        return;
    };
    let mut queue = CoalescedLightQueue::new();
    let slab = registry.default_state("minecraft:oak_slab").unwrap();
    let stone = registry.default_state("minecraft:stone").unwrap();
    queue.push(QueuedLightEdit {
        x: 1,
        y: 42,
        z: 1,
        state_id: slab,
    });
    queue.push(QueuedLightEdit {
        x: 1,
        y: 42,
        z: 1,
        state_id: stone,
    });
    let mut batches = queue.drain();
    let batch = batches.pop().unwrap();
    assert_eq!(batch.chunk, (0, 0));
    assert_eq!(batch.edits.len(), 1);
    let converted = batch.to_light_edits(&registry).unwrap();
    assert_eq!(converted.len(), 1);
    assert_eq!(
        converted[0].fact.opacity,
        registry.facts(stone).unwrap().opacity
    );
    assert_eq!(
        converted[0].fact.occlusion,
        registry.facts(stone).unwrap().face_occlusion
    );

    let mut invalid = batch;
    invalid.edits[0].state_id = u32::MAX;
    assert_eq!(invalid.to_light_edits(&registry), Err(Error::InvalidState));
}
