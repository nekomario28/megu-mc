// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tier A1 mixed-band sparse mod-registry admission coverage.

use megu_client::mod_assets_meta::{decode_mrs1_frame, ClientModRegistry};
use megu_client::protocol::{DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta};
use megu_client::world::{BlockTable, WorldState};

fn mrs1_v2_frame(base_block_state_id: u32, rows: &[(u32, &str, u8)]) -> Vec<u8> {
    let mut frame = b"MRS1".to_vec();
    frame.extend_from_slice(&2u16.to_be_bytes());
    frame.extend_from_slice(&base_block_state_id.to_be_bytes());
    frame.extend_from_slice(&1_000_000u32.to_be_bytes());
    frame.extend_from_slice(&(rows.len() as u32).to_be_bytes());
    for &(state_id, name, facts_byte) in rows {
        frame.extend_from_slice(&state_id.to_be_bytes());
        frame.extend_from_slice(&(name.len() as u16).to_be_bytes());
        frame.extend_from_slice(name.as_bytes());
        frame.extend_from_slice(&0u16.to_be_bytes());
        frame.push(facts_byte);
    }
    frame.extend_from_slice(&0u32.to_be_bytes());
    let crc = megu_core::hash::crc32_ieee(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

fn decode_rows(base_block_state_id: u32, rows: &[(u32, &str, u8)]) -> ClientModRegistry {
    decode_mrs1_frame(&mrs1_v2_frame(base_block_state_id, rows)).expect("valid MRS1 v2 frame")
}

fn install_registry(world: &mut WorldState, registry: &ClientModRegistry) -> usize {
    let rows: Vec<_> = registry
        .block_states()
        .map(|(id, state)| (id, state.name.clone(), state.facts_byte))
        .collect();
    world.install_dynamic_mod_registry_states(&rows)
}

fn empty_light() -> DecodedLightData {
    DecodedLightData {
        sky_mask: Vec::new(),
        block_mask: Vec::new(),
        empty_sky_mask: Vec::new(),
        empty_block_mask: Vec::new(),
        sky_arrays: Vec::new(),
        block_arrays: Vec::new(),
    }
}

fn apply_single_section(world: &mut WorldState, cells: &[(usize, u32)]) {
    let mut states = Box::new([0u32; 4096]);
    for &(index, state_id) in cells {
        states[index] = state_id;
    }
    world
        .apply_chunk(DecodedChunk {
            cx: 0,
            cz: 0,
            sections: vec![DecodedSection {
                non_air: cells.len() as i16,
                states,
                biomes: [0; 64],
            }],
            light: empty_light(),
            block_entities: Vec::new(),
        })
        .expect("apply dynamic-state chunk");
}

fn assert_registry_resolution(world: &WorldState, state_id: u32, name: &str, facts_byte: u8) {
    assert_eq!(world.blocks.state_name(state_id), Some(name));
    let fact = world
        .blocks
        .dynamic_light_fact(state_id)
        .expect("installed dynamic light fact");
    assert_eq!(fact.emission, facts_byte >> 4);
    assert_eq!(fact.opacity, facts_byte & 0x0f);
    assert_eq!(world.blocks.mesh_block(state_id).state_id, state_id);
}

fn assert_world_resolution(world: &WorldState, cells: &[(usize, u32)]) {
    for &(index, state_id) in cells {
        let x = (index % 16) as i32;
        let z = ((index / 16) % 16) as i32;
        let y = world.dimension.min_y + (index / 256) as i32;
        assert_eq!(world.get_block_state(x, y, z), Some(state_id));
        let section = &world.column_sections(0, 0).expect("loaded column")[0];
        assert_eq!(section.blocks[index].state_id, state_id);
    }
}

#[test]
fn create_only_frame_installs_and_resolves_in_world_state() {
    let create_id = 26_684;
    let rows = [(create_id, "create:shaft", 0x0f)];
    let registry = decode_rows(create_id, &rows);
    assert_eq!(
        registry
            .block_states()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![create_id]
    );

    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    assert_eq!(install_registry(&mut world, &registry), 1);
    assert_registry_resolution(&world, create_id, "create:shaft", 0x0f);

    let cells = [(0usize, create_id)];
    apply_single_section(&mut world, &cells);
    assert_world_resolution(&world, &cells);
}

#[test]
fn generic_only_frame_installs_and_resolves_in_world_state() {
    let generic_id = 1_000_000;
    let rows = [(generic_id, "examplemod:machine", 0x01)];
    let registry = decode_rows(generic_id, &rows);
    assert_eq!(
        registry
            .block_states()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![generic_id]
    );

    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    assert_eq!(install_registry(&mut world, &registry), 1);
    assert_registry_resolution(&world, generic_id, "examplemod:machine", 0x01);

    let cells = [(0usize, generic_id)];
    apply_single_section(&mut world, &cells);
    assert_world_resolution(&world, &cells);
}

#[test]
fn mixed_create_and_generic_frame_iterates_and_installs_both_bands() {
    let rows = [
        (26_684, "create:shaft", 0x0f),
        (26_685, "create:cogwheel", 0x00),
        (1_000_000, "examplemod:machine", 0x01),
        (1_000_042, "examplemod:lamp", 0xef),
    ];
    let registry = decode_rows(26_684, &rows);
    assert_eq!(
        registry
            .block_states()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        rows.iter().map(|row| row.0).collect::<Vec<_>>()
    );

    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    assert_eq!(install_registry(&mut world, &registry), rows.len());
    for &(state_id, name, facts_byte) in &rows {
        assert_registry_resolution(&world, state_id, name, facts_byte);
    }

    let cells = [
        (0usize, rows[0].0),
        (1usize, rows[1].0),
        (2usize, rows[2].0),
        (3usize, rows[3].0),
    ];
    apply_single_section(&mut world, &cells);
    assert_world_resolution(&world, &cells);
}

#[test]
fn vanilla_band_rows_are_rejected_and_cannot_overwrite_vanilla_state() {
    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    assert!(world.blocks.is_air(0));
    let state_count_before = world.blocks.state_count();

    let accepted = world.install_dynamic_mod_registry_states(&[
        (0, "examplemod:fake_air".to_string(), 0x0f),
        (26_683, "examplemod:fake_vanilla".to_string(), 0xff),
    ]);

    assert_eq!(accepted, 0);
    assert_eq!(world.blocks.state_count(), state_count_before);
    assert!(
        world.blocks.is_air(0),
        "vanilla air must remain vanilla-owned"
    );
    assert_eq!(world.blocks.dynamic_light_fact(0), None);
    assert_eq!(world.blocks.dynamic_light_fact(26_683), None);
}
