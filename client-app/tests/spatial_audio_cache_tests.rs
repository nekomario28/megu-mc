use std::cell::Cell;

use megu_client::spatial_audio::{
    AcousticMaterial, AcousticTracer, ListenerPose, SoundSource, SpatialAudioEngine,
    SpatialAudioPreset, VoxelAcousticWorld,
};

struct UniformAirWorld {
    reads: Cell<u64>,
    revision: Cell<u64>,
    epoch: Cell<u64>,
}

impl UniformAirWorld {
    fn new() -> Self {
        Self {
            reads: Cell::new(0),
            revision: Cell::new(1),
            epoch: Cell::new(1),
        }
    }
}

impl VoxelAcousticWorld for UniformAirWorld {
    fn acoustic_material(&self, _x: i32, _y: i32, _z: i32) -> AcousticMaterial {
        self.reads.set(self.reads.get() + 1);
        AcousticMaterial::AIR
    }
    fn world_mutation_epoch(&self) -> Option<[u64; 4]> {
        Some([self.epoch.get(), 0, 0, 0])
    }
    fn section_revision(&self, _cx: i32, _sy: i32, _cz: i32) -> Option<u64> {
        Some(self.revision.get())
    }
    fn uniform_section_material(&self, _cx: i32, _sy: i32, _cz: i32) -> Option<AcousticMaterial> {
        Some(AcousticMaterial::AIR)
    }
}

#[test]
fn audio_scene_cache_hit_uses_zero_voxel_reads_and_revision_bump_misses() {
    let world = UniformAirWorld::new();
    let mut engine = SpatialAudioEngine::new(48_000, SpatialAudioPreset::Performance);
    let source = SoundSource {
        position: [12.0, 8.0, 2.0],
        velocity: [0.0; 3],
    };
    let listener = ListenerPose {
        position: [0.0, 8.0, 2.0],
        ..ListenerPose::default()
    };

    engine.analyze(&world, &source, &listener);
    let after_miss = world.reads.get();
    assert!(
        after_miss > 0,
        "first analysis must inspect acoustic voxels"
    );
    engine.analyze(&world, &source, &listener);
    assert_eq!(
        world.reads.get(),
        after_miss,
        "cache hit must perform zero voxel lookups"
    );

    world.revision.set(2);
    engine.analyze(&world, &source, &listener);
    assert!(
        world.reads.get() > after_miss,
        "a section revision bump must retrace"
    );
    let after_revision_bump = world.reads.get();
    world.epoch.set(2);
    engine.analyze(&world, &source, &listener);
    assert!(
        world.reads.get() > after_revision_bump,
        "a world edit epoch bump must retrace"
    );
}

#[test]
fn uniform_air_section_warp_matches_per_voxel_occlusion() {
    let world = UniformAirWorld::new();
    let optimized = AcousticTracer::new(&world, SpatialAudioPreset::Performance);
    let fast = optimized.occlusion_between([1.25, 8.5, 2.5], [31.75, 8.5, 2.5]);
    assert!(
        optimized.macro_dda_skips() > 0,
        "uniform sections should be warped over"
    );

    struct PerVoxelAir<'a>(&'a UniformAirWorld);
    impl VoxelAcousticWorld for PerVoxelAir<'_> {
        fn acoustic_material(&self, x: i32, y: i32, z: i32) -> AcousticMaterial {
            self.0.acoustic_material(x, y, z)
        }
    }
    let reference = AcousticTracer::new(&PerVoxelAir(&world), SpatialAudioPreset::Performance)
        .occlusion_between([1.25, 8.5, 2.5], [31.75, 8.5, 2.5]);
    assert_eq!(fast, reference);
}
