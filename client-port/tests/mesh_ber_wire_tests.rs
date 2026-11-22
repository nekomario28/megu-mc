// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//
// MESH-BER-CUTOUT-1 wiring proofs: the mesher's BER instance collector must
// classify every vanilla block-entity type into its `VanillaBerKind`, emit
// one instanced batch per (kind, state) with real world positions, tag
// cutout-material quads into the cutout side-list, and extrude the six 3D
// skin overlay parts — and `gpu_pipeline::upload_ber_instances` must pack
// those batches into the instanced-part record stream.

use mc_spine_client_mesh::ber_instancing::{build_ber_mesh, VanillaBerFacing, VanillaBerKind, VanillaBerState};
use mc_spine_client_mesh::gpu_pipeline::upload_ber_instances;
use mc_spine_client_mesh::mesher::{
    block_render_layer, classify_vanilla_ber, humanoid_skin_layer_parts, mesh_section_with_cutout,
    BerInstanceCollector, SectionBerInstance, VANILLA_BER_KINDS,
};
use mc_spine_client_mesh::vanilla_plus_visuals::{
    VanillaPlusVisualsConfig, SKIN_RGBA_BYTES,
};
use mc_spine_client_mesh::{Block, Layer, Light, Section, CUTOUT, OPAQUE};

/// A non-degenerate [`VanillaBerState`] per family — every variant must
/// produce a non-empty `build_ber_mesh`.
fn representative_state(kind: VanillaBerKind) -> VanillaBerState {
    match kind {
        VanillaBerKind::Chest => VanillaBerState::Chest {
            lid_open: 0.4,
            double: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::Sign => VanillaBerState::Sign {
            wall: false,
            hanging: false,
            glowing: true,
            facing_yaw: 0.3,
        },
        VanillaBerKind::Banner => VanillaBerState::Banner {
            wall: false,
            sway_rad: 0.05,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Beacon => VanillaBerState::Beacon {
            beam_height: 64.0,
            active: true,
            rgb: [1.0, 0.2, 0.1],
        },
        VanillaBerKind::Bed => VanillaBerState::Bed {
            is_head: true,
            facing: VanillaBerFacing::North,
        },
        VanillaBerKind::Skull => VanillaBerState::Skull {
            wall: false,
            yaw_deg: 90.0,
        },
        VanillaBerKind::ItemFrame => VanillaBerState::ItemFrame {
            facing: VanillaBerFacing::East,
            glow: false,
            rotation: 2,
        },
        VanillaBerKind::Campfire => VanillaBerState::Campfire {
            lit: true,
            item_count: 2,
        },
        VanillaBerKind::BrewingStand => VanillaBerState::BrewingStand { bottle_mask: 0b101 },
        VanillaBerKind::Lectern => VanillaBerState::Lectern {
            has_book: true,
            facing: VanillaBerFacing::West,
        },
        VanillaBerKind::DecoratedPot => VanillaBerState::DecoratedPot {
            facing: VanillaBerFacing::South,
            wobble: 0.02,
        },
        VanillaBerKind::ShulkerBox => VanillaBerState::ShulkerBox {
            open_progress: 0.5,
            facing: VanillaBerFacing::Up,
        },
    }
}

#[test]
fn all_twelve_ber_kinds_classify_and_mesh() {
    // Every vanilla block-entity type alias maps to its renderer family.
    let cases: &[(&str, VanillaBerKind)] = &[
        ("minecraft:chest", VanillaBerKind::Chest),
        ("minecraft:trapped_chest", VanillaBerKind::Chest),
        ("minecraft:ender_chest", VanillaBerKind::Chest),
        ("minecraft:sign", VanillaBerKind::Sign),
        ("minecraft:wall_sign", VanillaBerKind::Sign),
        ("minecraft:hanging_sign", VanillaBerKind::Sign),
        ("minecraft:wall_hanging_sign", VanillaBerKind::Sign),
        ("minecraft:banner", VanillaBerKind::Banner),
        ("minecraft:standing_banner", VanillaBerKind::Banner),
        ("minecraft:wall_banner", VanillaBerKind::Banner),
        ("minecraft:beacon", VanillaBerKind::Beacon),
        ("minecraft:bed", VanillaBerKind::Bed),
        ("minecraft:skull", VanillaBerKind::Skull),
        ("minecraft:wall_skull", VanillaBerKind::Skull),
        ("minecraft:item_frame", VanillaBerKind::ItemFrame),
        ("minecraft:glow_item_frame", VanillaBerKind::ItemFrame),
        ("minecraft:campfire", VanillaBerKind::Campfire),
        ("minecraft:soul_campfire", VanillaBerKind::Campfire),
        ("minecraft:brewing_stand", VanillaBerKind::BrewingStand),
        ("minecraft:lectern", VanillaBerKind::Lectern),
        ("minecraft:decorated_pot", VanillaBerKind::DecoratedPot),
        ("minecraft:shulker_box", VanillaBerKind::ShulkerBox),
    ];
    let mut covered = [false; 12];
    for &(name, kind) in cases {
        assert_eq!(classify_vanilla_ber(name), Some(kind), "{name}");
        covered[VANILLA_BER_KINDS.iter().position(|k| *k == kind).unwrap()] = true;
    }
    assert!(covered.iter().all(|c| *c), "every VanillaBerKind needs a BE name");

    // Unknown / modded block-entity types fail closed.
    assert_eq!(classify_vanilla_ber("minecraft:furnace"), None);
    assert_eq!(classify_vanilla_ber("mymod:widget"), None);
    assert_eq!(classify_vanilla_ber(""), None);

    // Each family builds real geometry.
    for kind in VANILLA_BER_KINDS {
        let state = representative_state(kind);
        assert_eq!(state.kind(), kind);
        let mesh = build_ber_mesh(kind, state);
        assert!(!mesh.quads.is_empty(), "{kind:?} produced an empty mesh");
    }
}

#[test]
fn section_bers_emit_per_kind_batches_with_world_positions() {
    let chest_state = VanillaBerState::Chest {
        lid_open: 0.5,
        double: false,
        facing: VanillaBerFacing::South,
    };
    let sign_state = VanillaBerState::Sign {
        wall: true,
        hanging: false,
        glowing: false,
        facing_yaw: 1.1,
    };
    let instances = [
        SectionBerInstance {
            entity_id: 1,
            pos: [3, 4, 5],
            state: chest_state,
            packed_light: 0x00f0,
            tint_rgba: 0xffff_ffff,
        },
        SectionBerInstance {
            entity_id: 2,
            pos: [9, 10, 11],
            state: sign_state,
            packed_light: 0x1234,
            tint_rgba: 0xffff_ffff,
        },
        // Same kind, different state -> its own batch.
        SectionBerInstance {
            entity_id: 3,
            pos: [3, 4, 6],
            state: VanillaBerState::Chest {
                lid_open: 1.0,
                double: true,
                facing: VanillaBerFacing::North,
            },
            packed_light: 0x00f0,
            tint_rgba: 0xffff_ffff,
        },
        // Outside the 16^3 section at origin -> skipped by collect_section.
        SectionBerInstance {
            entity_id: 4,
            pos: [40, 4, 5],
            state: chest_state,
            packed_light: 0x00f0,
            tint_rgba: 0xffff_ffff,
        },
    ];

    let mut collector = BerInstanceCollector::new();
    collector.collect_section([0, 0, 0], &instances);
    let batches = collector.into_batches();

    // Chest (slot 0) batches precede Sign (slot 1); two chest state groups.
    assert_eq!(batches.len(), 3);
    assert_eq!(batches[0].kind, VanillaBerKind::Chest);
    assert_eq!(batches[1].kind, VanillaBerKind::Chest);
    assert_eq!(batches[2].kind, VanillaBerKind::Sign);
    assert_ne!(batches[0].state, batches[1].state);

    // World positions land on the instance records unchanged.
    assert_eq!(batches[0].instances[0].pos, [3.0, 4.0, 5.0]);
    assert_eq!(batches[0].instances[0].packed_light, 0x00f0);
    assert_eq!(batches[1].instances[0].pos, [3.0, 4.0, 6.0]);
    assert_eq!(batches[2].instances[0].pos, [9.0, 10.0, 11.0]);
    assert_eq!(batches[2].instances[0].packed_light, 0x1234);

    // Every batch carries a real, non-empty template mesh.
    for batch in &batches {
        assert!(!batch.template.quads.is_empty());
    }
}

#[test]
fn identical_states_share_one_instanced_batch() {
    let state = VanillaBerState::Lectern {
        has_book: false,
        facing: VanillaBerFacing::East,
    };
    let instances = [
        SectionBerInstance {
            entity_id: 10,
            pos: [1, 2, 3],
            state,
            packed_light: 0x00f0,
            tint_rgba: 0xffff_ffff,
        },
        SectionBerInstance {
            entity_id: 11,
            pos: [5, 6, 7],
            state,
            packed_light: 0x00f0,
            tint_rgba: 0xffff_ffff,
        },
    ];
    let mut collector = BerInstanceCollector::new();
    collector.collect(&instances);
    let batches = collector.into_batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].kind, VanillaBerKind::Lectern);
    assert_eq!(batches[0].instances.len(), 2);
    assert_eq!(batches[0].instances[1].pos, [5.0, 6.0, 7.0]);
}

#[test]
fn cutout_material_quads_land_in_cutout_list() {
    let mut section = Section::filled(Block::AIR, Light::new(0, 15));
    // Foliage-style cutout cube at the section corner.
    section.set(
        0,
        0,
        0,
        Block {
            state_id: 7,
            flags: CUTOUT,
            occlusion: 0,
            ..Block::AIR
        },
        Light::new(0, 15),
    );
    // Ordinary opaque cube mid-section.
    section.set(8, 8, 8, Block::opaque_cube(3), Light::new(0, 15));

    let out = mesh_section_with_cutout(&section.view());
    assert!(!out.quads.is_empty());
    assert!(!out.cutout_indices.is_empty());

    // The side-list is exactly the set of cutout-layer quads.
    let mut expected = Vec::new();
    for (index, quad) in out.quads.iter().enumerate() {
        if quad.layer == Layer::Cutout {
            expected.push(index as u32);
        }
    }
    assert_eq!(out.cutout_indices, expected);
    for &index in &out.cutout_indices {
        assert_eq!(out.quads[index as usize].layer, Layer::Cutout);
    }
    // A solid quad exists and is not in the cutout list.
    assert!(out
        .quads
        .iter()
        .any(|quad| quad.layer != Layer::Cutout));
    for (index, quad) in out.quads.iter().enumerate() {
        if quad.layer != Layer::Cutout {
            assert!(!out.cutout_indices.contains(&(index as u32)));
        }
    }

    // The block->layer mapping mirrors mesher precedence.
    let cutout_block = Block {
        state_id: 7,
        flags: CUTOUT,
        occlusion: 0,
        ..Block::AIR
    };
    let opaque_block = Block {
        flags: OPAQUE | CUTOUT,
        ..Block::opaque_cube(1)
    };
    assert_eq!(block_render_layer(&cutout_block), Layer::Cutout);
    assert_eq!(block_render_layer(&opaque_block), Layer::Opaque);
    assert_eq!(block_render_layer(&Block::AIR), Layer::Translucent);
}

#[test]
fn humanoid_skin_layer_parts_returns_overlay_geometry() {
    let skin = vec![255u8; SKIN_RGBA_BYTES];
    let config = VanillaPlusVisualsConfig {
        skin_3d_layers: true,
        ..VanillaPlusVisualsConfig::default()
    };
    let parts = humanoid_skin_layer_parts(&config, &skin, false, 0.0);
    assert_eq!(parts.len(), 6);
    for (expected, (part, mesh)) in mc_spine_client_mesh::mesher::HUMANOID_SKIN_OVERLAY_PARTS
        .iter()
        .zip(parts.iter())
    {
        assert_eq!(part, expected);
        assert!(!mesh.quads.is_empty(), "{part:?} produced no geometry");
    }
    // Slim arms still produce all six parts.
    assert_eq!(humanoid_skin_layer_parts(&config, &skin, true, 0.0).len(), 6);
    // Feature off -> empty (caller keeps the flat overlay).
    assert!(humanoid_skin_layer_parts(
        &VanillaPlusVisualsConfig::default(),
        &skin,
        false,
        0.0
    )
    .is_empty());
    // Beyond the LOD cutoff -> empty.
    assert!(humanoid_skin_layer_parts(&config, &skin, false, 10_000.0).is_empty());
    // Malformed skin buffer -> empty, never a panic.
    assert!(humanoid_skin_layer_parts(&config, &[0u8; 16], false, 0.0).is_empty());
}

#[test]
fn upload_ber_instances_packs_kind_batches_into_record_stream() {
    let mut collector = BerInstanceCollector::new();
    collector.collect(&[
        SectionBerInstance {
            entity_id: 1,
            pos: [3, 4, 5],
            state: VanillaBerState::Chest {
                lid_open: 0.5,
                double: false,
                facing: VanillaBerFacing::South,
            },
            packed_light: 0x00f0,
            tint_rgba: 0xffff_ffff,
        },
        SectionBerInstance {
            entity_id: 2,
            pos: [9, 10, 11],
            state: VanillaBerState::Sign {
                wall: false,
                hanging: false,
                glowing: true,
                facing_yaw: 0.0,
            },
            packed_light: 0x1234,
            tint_rgba: 0xffff_ffff,
        },
    ]);
    let batches = collector.into_batches();
    let upload = upload_ber_instances(&batches);

    assert_eq!(upload.batches.len(), 2);
    assert_eq!(upload.runs.len(), 2);
    assert_eq!(upload.instances.len(), 2);

    // Chest = kind base 0 -> mesh_kind 0; Sign = base 1 -> 256.
    assert_eq!(upload.batches[0].mesh_kind, 0);
    assert_eq!(upload.batches[1].mesh_kind, 256);
    assert!(!upload.batches[0].template.quads.is_empty());
    assert!(upload.batches[0].geometry_dirty && upload.batches[0].instances_dirty);

    // Runs index the flat stream; records carry world pos + light.
    assert_eq!(upload.runs[0].first_instance, 0);
    assert_eq!(upload.runs[0].instance_count, 1);
    assert_eq!(upload.runs[1].first_instance, 1);
    let chest = &upload.instances[0];
    assert_eq!(chest.pos_scale, [3.0, 4.0, 5.0, 1.0]);
    assert_eq!(chest.packed_light, 0x00f0);
    assert_eq!(chest.mesh_kind, 0);
    let sign = &upload.instances[1];
    assert_eq!(sign.pos_scale, [9.0, 10.0, 11.0, 1.0]);
    assert_eq!(sign.packed_light, 0x1234);
    assert_eq!(sign.mesh_kind, 256);
}
