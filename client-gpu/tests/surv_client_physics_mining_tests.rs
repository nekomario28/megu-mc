// SPDX-License-Identifier: LGPL-3.0-or-later
//! Regression coverage for live survival movement propagation, canonical
//! mining duration/continuation, shaped selection rays, End Credits routing,
//! and component-preserving live HUD metadata.

use megu_client::protocol::{
    write_varint, ClientPlayEvent, DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta,
    ItemComponentPatch, ItemStackComponents, PlayEvent,
};
use megu_client::world::{BlockTable, WorldState};
use megu_client_gpu::live_session::{
    LiveSession, PlayerPose, SessionStatus, SurvivalMovementInput,
};

const AIR: u32 = 0;
const STONE: u32 = 1;
const OBSIDIAN: u32 = 2;
const BOTTOM_SLAB: u32 = 3;
const WATER_SOURCE: u32 = 4;
const DIAMOND_PICKAXE_ITEM: i32 = 840;

fn test_block_table() -> BlockTable {
    let json = r##"{
        "blocks": [
            ["minecraft:air", 0, 1],
            ["minecraft:stone", 1, 1],
            ["minecraft:obsidian", 2, 1],
            ["minecraft:oak_slab", 3, 1],
            ["minecraft:water", 4, 1]
        ],
        "flags": [1, 8, 8, 8, 0],
        "full_sides": [0, 63, 63, 0, 0],
        "shapes": [
            {"min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0]},
            {"min": [0.0, 0.0, 0.0], "max": [1.0, 0.5, 1.0]}
        ],
        "collision_shapes": [[], [0], [0], [1], []]
    }"##;
    BlockTable::from_json_bytes(json.as_bytes()).expect("synthetic block table")
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

fn cell_index(x: usize, y: usize, z: usize) -> usize {
    y * 256 + z * 16 + x
}

fn chunk_with_edits(edits: &[(usize, u32)]) -> DecodedChunk {
    let dimension = DimensionMeta::default();
    let mut sections = Vec::new();
    for section in 0..dimension.section_count() {
        let mut states = Box::new([0u32; 4096]);
        if section == 4 {
            for &(index, state) in edits {
                states[index] = state;
            }
        }
        sections.push(DecodedSection {
            non_air: i16::from(section == 4 && !edits.is_empty()),
            states,
            biomes: [0; 64],
        });
    }
    DecodedChunk {
        cx: 0,
        cz: 0,
        sections,
        light: empty_light(),
        block_entities: Vec::new(),
    }
}

fn world_with_edits(edits: &[(usize, u32)]) -> WorldState {
    let mut world = WorldState::new_without_registry(DimensionMeta::default(), test_block_table());
    world
        .apply_chunk(chunk_with_edits(edits))
        .expect("synthetic chunk applies");
    world
}

fn playing_session(world: WorldState) -> LiveSession {
    let mut live = LiveSession::offline_with_null_audio();
    live.status = SessionStatus::Playing;
    live.world = world;
    live
}

fn floor_edits(include_cell: impl Fn(i32, i32) -> bool) -> Vec<(usize, u32)> {
    let mut edits = Vec::new();
    for x in 0..16 {
        for z in 0..16 {
            if include_cell(x, z) {
                edits.push((cell_index(x as usize, 0, z as usize), STONE));
            }
        }
    }
    edits
}

fn input(forward: f32, sneak: bool) -> SurvivalMovementInput {
    SurvivalMovementInput {
        forward,
        sneak,
        yaw: 0.0,
        pitch: 0.0,
        ..SurvivalMovementInput::default()
    }
}

fn step_many(live: &mut LiveSession, ticks: usize, input: SurvivalMovementInput) {
    for _ in 0..ticks {
        live.step_live_survival_movement(0.05, input);
    }
}

#[test]
fn survival_step_reports_ground_collides_walls_edges_and_gravity() {
    let mut edits = floor_edits(|_, _| true);
    edits.push((cell_index(0, 1, 1), STONE));
    edits.push((cell_index(1, 1, 1), STONE));
    edits.push((cell_index(2, 1, 1), STONE));
    let mut live = playing_session(world_with_edits(&edits));
    assert_eq!(live.world.get_block_state(0, 0, 0), Some(STONE));
    assert_eq!(live.world.get_block_state(0, 1, 1), Some(STONE));
    live.pose = Some(PlayerPose {
        feet: [0.5, 1.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    });

    live.step_live_survival_movement(0.05, input(0.0, false));
    let output = live
        .step_live_survival_movement(0.05, input(0.0, false))
        .expect("physics step");
    assert!(
        output.on_ground,
        "solid floor must produce a ground bit: {output:?} pose={:?}",
        live.pose
    );
    assert_eq!(live.last_movement_on_ground, Some(true));
    assert!((output.eye_height - 1.62).abs() < 1e-5);
    let sneak_input = input(0.0, true);
    let start = live
        .step_live_survival_movement(0.05, sneak_input)
        .expect("sneak physics step");
    let halfway = live
        .step_live_survival_movement(0.025, sneak_input)
        .expect("sneak camera interpolation");
    let sneaked = live
        .step_live_survival_movement(0.025, sneak_input)
        .expect("completed sneak interpolation");
    assert!((start.eye_height - 1.62).abs() < 1e-5);
    assert!((halfway.eye_height - 1.445).abs() < 1e-5);
    assert!((sneaked.eye_height - 1.27).abs() < 1e-5);

    step_many(&mut live, 20, input(1.0, false));
    let z = live.pose.expect("pose").feet[2];
    assert!(
        z <= 0.701,
        "wall at z=1 must stop horizontal movement, got z={z}"
    );

    // Sneaking at the south edge of a one-cell floor must not walk off.
    let mut edge_edits = floor_edits(|_, z| z == 0);
    edge_edits.push((cell_index(0, 1, 0), AIR));
    let mut edge = playing_session(world_with_edits(&edge_edits));
    edge.pose = Some(PlayerPose {
        feet: [0.5, 1.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    });
    step_many(&mut edge, 2, input(0.0, true));
    step_many(&mut edge, 30, input(1.0, true));
    let sneaking = edge.pose.expect("pose");
    assert!(
        sneaking.feet[2] <= 1.31 && sneaking.feet[1] == 1.0,
        "sneak edge prevention failed: {sneaking:?}"
    );

    // The same motion without sneaking leaves the ledge and gravity applies.
    let mut falling = playing_session(world_with_edits(&floor_edits(|_, z| z == 0)));
    falling.pose = Some(PlayerPose {
        feet: [0.5, 1.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    });
    step_many(&mut falling, 2, input(0.0, false));
    step_many(&mut falling, 40, input(1.0, false));
    let fallen = falling.pose.expect("pose");
    assert!(fallen.feet[2] > 1.0, "walked off the ledge: {fallen:?}");
    assert!(
        fallen.feet[1] < 1.0,
        "gravity did not lower the player: {fallen:?}"
    );
    assert_eq!(falling.last_movement_on_ground, Some(false));
}

#[test]
fn obsidian_diamond_pickaxe_mining_does_not_finish_or_restart_early() {
    let mut edits = floor_edits(|x, z| x == 0 && z == 0);
    edits.push((cell_index(0, 1, 0), OBSIDIAN));
    let mut live = playing_session(world_with_edits(&edits));
    live.hud.game_mode = 0;
    live.player_inventory.insert(
        36,
        ItemStackComponents {
            count: 1,
            item_id: DIAMOND_PICKAXE_ITEM,
            added: Vec::new(),
            removed: Vec::new(),
        },
    );
    assert_eq!(
        megu_client_gpu::recipe_ingest::item_name_for_id(DIAMOND_PICKAXE_ITEM as u32),
        "minecraft:diamond_pickaxe"
    );
    live.pose = Some(PlayerPose {
        feet: [0.5, 1.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    });
    step_many(&mut live, 2, input(0.0, false));
    let target = [0, 1, 0];
    live.begin_mining(target, 1);
    assert!(
        !live.tick_mining(2.0, Some((target, 1))),
        "correct 9.375 second obsidian dig must survive two seconds"
    );
    assert!(
        live.active_block_break_overlay().is_some(),
        "the same dig attempt must remain active"
    );
    assert!(
        live.tick_mining(7.4, Some((target, 1))),
        "dig completes at vanilla time"
    );
    assert!(
        live.active_block_break_overlay().is_none(),
        "completed attempt waits for the authoritative block update"
    );
    assert!(
        !live.tick_mining(5.0, Some((target, 1))),
        "completed attempt must not restart while the server update is pending"
    );
}

#[test]
fn raycast_passes_through_empty_upper_half_of_bottom_slab() {
    let edits = vec![
        (cell_index(0, 1, 2), BOTTOM_SLAB),
        (cell_index(0, 1, 4), STONE),
    ];
    let mut live = playing_session(world_with_edits(&edits));
    live.pose = Some(PlayerPose {
        feet: [0.5, 0.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    });
    let (pos, _) = live.crosshair_block().expect("block behind slab");
    assert_eq!(pos, [0, 1, 4]);
}

#[test]
fn end_credits_completion_is_alive_safe_and_win_game_routes_values() {
    let mut live = LiveSession::offline_with_null_audio();
    live.status = SessionStatus::Playing;
    assert!(!live.is_dead());
    assert!(live.send_end_credits_complete());

    live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
        event: 4,
        value: 1.0,
    }))
    .expect("win game event");
    assert!(live.pending_end_credits);
    assert!(live.send_end_credits_complete());
    assert!(!live.pending_end_credits);

    live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
        event: 4,
        value: 0.0,
    }))
    .expect("immediate win respawn event");
    assert!(!live.pending_end_credits);
}

fn text_nbt(text: &str) -> Vec<u8> {
    let mut out = vec![10u8, 8u8];
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(b"text");
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    out
}

fn varint_bytes(value: i32) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, value);
    out
}

#[test]
fn hud_snapshot_preserves_stack_boss_status_and_container_metadata() {
    let mut live = LiveSession::offline_with_null_audio();
    live.status = SessionStatus::Playing;
    live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
        entity_id: 77,
        dimension: DimensionMeta::default(),
        hardcore: false,
    }))
    .expect("join game");

    let mut enchantments = vec![1, 8, 2, 1]; // one entry: efficiency II, show tooltip
    let mut lore = varint_bytes(1);
    lore.extend_from_slice(&text_nbt("Very shiny"));
    live.player_inventory.insert(
        36,
        ItemStackComponents {
            count: 3,
            item_id: DIAMOND_PICKAXE_ITEM,
            added: vec![
                ItemComponentPatch {
                    type_id: 2,
                    data: varint_bytes(100),
                },
                ItemComponentPatch {
                    type_id: 3,
                    data: varint_bytes(7),
                },
                ItemComponentPatch {
                    type_id: 5,
                    data: text_nbt("Digger"),
                },
                ItemComponentPatch {
                    type_id: 7,
                    data: lore,
                },
                ItemComponentPatch {
                    type_id: 9,
                    data: std::mem::take(&mut enchantments),
                },
            ],
            removed: Vec::new(),
        },
    );
    let stack = live.live_hotbar_snapshot()[0]
        .clone()
        .expect("component-preserving hotbar stack");
    assert_eq!(stack.count, 3);
    assert_eq!(stack.damage, Some(7));
    assert_eq!(stack.max_damage, Some(100));
    assert_eq!(stack.custom_name.as_deref(), Some("Digger"));
    assert_eq!(stack.lore, vec!["Very shiny".to_string()]);
    assert!(stack.enchanted);

    let mut boss_payload = text_nbt("Wither");
    boss_payload.extend_from_slice(&0.5f32.to_be_bytes());
    boss_payload.extend_from_slice(&varint_bytes(2));
    boss_payload.extend_from_slice(&varint_bytes(3));
    boss_payload.push(0x05);
    live.apply_event(ClientPlayEvent::Play(PlayEvent::BossEvent {
        uuid: [7; 16],
        action: 0,
        raw_payload: boss_payload,
    }))
    .expect("boss bar add");
    let bar = &live.live_bossbar_snapshot()[0];
    assert_eq!(bar.title, "Wither");
    assert_eq!(bar.health, 0.5);
    assert_eq!(bar.color, 2);
    assert_eq!(bar.divisions, 3);
    assert_eq!(bar.flags, 0x05);

    live.apply_event(ClientPlayEvent::Play(PlayEvent::UpdateMobEffect {
        eid: 77,
        effect_id: 2,
        amplifier: 1,
        duration: 220,
        flags: 0x07,
    }))
    .expect("effect add");
    let effect = &live.live_status_effect_snapshot()[0];
    assert_eq!(effect.name, "haste");
    assert_eq!(effect.amplifier, 1);
    assert_eq!(effect.duration_ticks, 220);
    assert!(effect.ambient && effect.show_particles && effect.show_icon);

    live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
        window_id: 2,
        menu_type: 9,
        title_nbt: text_nbt("Custom Anvil"),
    }))
    .expect("open screen");
    assert_eq!(
        live.open_screen
            .as_ref()
            .and_then(|screen| screen.title_text.as_deref()),
        Some("Custom Anvil")
    );

    let feed = live.hud_feed();
    assert_eq!(feed.hotbar_stacks[0].as_ref().map(|s| s.count), Some(3));
    assert_eq!(feed.bossbars.len(), 1);
    assert_eq!(feed.status_effects.len(), 1);
}

#[test]
fn empty_bucket_selection_clips_water_sources() {
    let edits = vec![(cell_index(0, 1, 2), WATER_SOURCE)];
    let mut live = playing_session(world_with_edits(&edits));
    live.pose = Some(PlayerPose {
        feet: [0.5, 0.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    });
    // Look up the protocol id dynamically through the mirrored registry table:
    // this guards the fixture against accidental renumbering while retaining
    // the live client lookup path used by `selection_fluid_clip_mode`.
    let bucket_id = (0..2048)
        .find(|id| megu_client_gpu::recipe_ingest::item_name_for_id(*id) == "minecraft:bucket")
        .expect("bucket item id") as i32;
    live.player_inventory.insert(
        36,
        ItemStackComponents {
            count: 1,
            item_id: bucket_id,
            added: Vec::new(),
            removed: Vec::new(),
        },
    );
    assert_eq!(live.crosshair_block().map(|(pos, _)| pos), Some([0, 1, 2]));
}
