// SPDX-License-Identifier: LGPL-3.0-or-later
//! Lane V1211-CLIENT-PROTO767: Protocol 767 Play coverage expansion tests —
//! C2S encoder wire layouts + names, S2C bounded decoder roundtrips, and
//! fail-closed truncation/oversized-count behaviour, plus the
//! `--audio-sink coreaudio|winmm` CLI surface.

use std::panic::{catch_unwind, AssertUnwindSafe};

use megu_client::audio_out::AudioSinkKind;
use megu_client::cli::parse_args;
use megu_client::protocol::{
    decode_extended_play_event, encode_container_slot_state_changed, encode_edit_book,
    encode_lock_difficulty, encode_ping_request, encode_place_recipe, encode_player_abilities,
    encode_pong, encode_recipe_book_change_settings, encode_recipe_book_seen_recipe,
    encode_rename_item, encode_select_trade, encode_seen_advancements, encode_set_beacon,
    encode_set_command_block, encode_set_command_minecart, encode_slot_stack,
    encode_teleport_to_entity, pack_position, write_varint, write_varlong, PlayEvent,
};
use megu_proto::{PacketDir, PacketPhase, ProtocolVersion, VersionRouter};

// ---------------------------------------------------------------------------
// Wire builders (test-side mirrors of the canonical 767 layouts)
// ---------------------------------------------------------------------------

fn vi(v: i32) -> Vec<u8> {
    let mut b = Vec::new();
    write_varint(&mut b, v);
    b
}

fn vl(v: i64) -> Vec<u8> {
    let mut b = Vec::new();
    write_varlong(&mut b, v);
    b
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

fn be_i32(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i64(v: i64) -> [u8; 8] {
    v.to_be_bytes()
}

fn be_f32(v: f32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_f64(v: f64) -> [u8; 8] {
    v.to_be_bytes()
}

fn s(text: &str) -> Vec<u8> {
    cat(&[&vi(text.len() as i32), text.as_bytes()])
}

fn opt_string(value: Option<&str>) -> Vec<u8> {
    match value {
        Some(t) => cat(&[&[1], &s(t)]),
        None => vec![0],
    }
}

fn nbt_string_entry(name: &str, value: &str) -> Vec<u8> {
    let mut v = vec![8u8]; // TAG_String
    v.extend_from_slice(&(name.len() as u16).to_be_bytes());
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&(value.len() as u16).to_be_bytes());
    v.extend_from_slice(value.as_bytes());
    v
}

fn nbt_compound(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![10u8]; // TAG_Compound
    for e in entries {
        v.extend_from_slice(e);
    }
    v.push(0); // TAG_End
    v
}

fn chat_text_nbt(text: &str) -> Vec<u8> {
    nbt_compound(&[nbt_string_entry("text", text)])
}

/// Merchant `ItemCost` with zero added components.
fn item_cost(item: i64, count: i32) -> Vec<u8> {
    cat(&[&vi(item as i32), &vi(count), &vi(0)])
}

/// One `merchant_offers` trade: cost_a, result slot, no cost_b, not
/// disabled, then uses/max_uses/xp/special_price i32s, price_multiplier
/// f32 and demand i32.
fn merchant_offer() -> Vec<u8> {
    let mut o = item_cost(5, 3);
    o.extend_from_slice(&encode_slot_stack(7, 1)); // result slot
    o.push(0); // no cost_b
    o.push(0); // not disabled
    for v in [1i32, 12, 4, -2] {
        o.extend_from_slice(&be_i32(v));
    }
    o.extend_from_slice(&1.0f32.to_be_bytes()); // price_multiplier
    o.extend_from_slice(&be_i32(0)); // demand
    o
}

fn decode(name: &str, body: &[u8]) -> PlayEvent {
    decode_extended_play_event(name, body)
        .unwrap_or_else(|e| panic!("{name}: decode error {e}"))
        .unwrap_or_else(|| panic!("{name}: not handled"))
}

fn assert_truncation_safe(name: &str, body: &[u8]) {
    // Every strict prefix must fail closed (error or `None`), never panic.
    for end in 0..body.len() {
        catch_unwind(AssertUnwindSafe(|| {
            let _ = decode_extended_play_event(name, &body[..end]);
        }))
        .unwrap_or_else(|_| panic!("{name}: panicked on {}-byte prefix", end));
    }
    catch_unwind(AssertUnwindSafe(|| {
        let _ = decode_extended_play_event(name, body);
    }))
    .expect("decoder panicked on full body");
}

fn v767_table() -> megu_proto::ProtocolTable {
    VersionRouter::with_v767_and_v777()
        .select(ProtocolVersion::V1_21_1.number())
        .expect("select v767")
        .table()
        .clone()
}

// ---------------------------------------------------------------------------
// C2S: encoder names resolve to the canonical 767 serverbound ids
// ---------------------------------------------------------------------------

#[test]
fn c2s_encoder_names_resolve_to_v767_ids() {
    let table = v767_table();
    let cases: &[(&str, u32)] = &[
        ("ping_request", 0x21),
        ("pong", 0x27),
        ("player_abilities", 0x23),
        ("edit_book", 0x14),
        ("set_command_block", 0x30),
        ("set_command_minecart", 0x31),
        ("set_beacon", 0x2e),
        ("select_trade", 0x2d),
        ("place_recipe", 0x22),
        ("recipe_book_change_settings", 0x28),
        ("recipe_book_seen_recipe", 0x29),
        ("rename_item", 0x2a),
        ("container_slot_state_changed", 0x10),
        ("seen_advancements", 0x2c),
        ("teleport_to_entity", 0x37),
        ("lock_difficulty", 0x19),
    ];
    for (name, expected) in cases {
        assert_eq!(
            table.packet_id(PacketPhase::Play, PacketDir::Serverbound, name),
            Some(*expected),
            "wrong v767 id for {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// C2S: exact wire layouts
// ---------------------------------------------------------------------------

#[test]
fn encodes_ping_pong_and_abilities() {
    assert_eq!(encode_ping_request(0x0102_0304_0506_0708).1.len(), 8);
    let (n, p) = encode_ping_request(-1);
    assert_eq!(n, "ping_request");
    assert_eq!(p, (-1i64).to_be_bytes());

    let (n, p) = encode_pong(-3);
    assert_eq!(n, "pong");
    assert_eq!(p, (-3i32).to_be_bytes());

    assert_eq!(encode_player_abilities(0x02), ("player_abilities", vec![0x02]));
    assert_eq!(encode_lock_difficulty(true), ("lock_difficulty", vec![1]));
    assert_eq!(encode_lock_difficulty(false), ("lock_difficulty", vec![0]));
}

#[test]
fn encodes_edit_book_layout() {
    let (n, p) = encode_edit_book(5, &["a", "bc"], Some("title"));
    assert_eq!(n, "edit_book");
    let mut expect = vi(5);
    expect.extend(vi(2));
    expect.extend(s("a"));
    expect.extend(s("bc"));
    expect.push(1);
    expect.extend(s("title"));
    assert_eq!(p, expect);

    let (_, p) = encode_edit_book(0, &[], None);
    assert_eq!(p, cat(&[&vi(0), &vi(0), &[0]]));
}

#[test]
fn encodes_command_block_packets() {
    let (n, p) = encode_set_command_block([1, 64, -1], "say hi", 2, true, false, true);
    assert_eq!(n, "set_command_block");
    let mut expect = pack_position([1, 64, -1]).to_be_bytes().to_vec();
    expect.extend(s("say hi"));
    expect.extend(vi(2));
    expect.push(0b101);
    assert_eq!(p, expect);

    let (n, p) = encode_set_command_minecart(42, "tp @s 0 0 0", true);
    assert_eq!(n, "set_command_minecart");
    let mut expect = vi(42);
    expect.extend(s("tp @s 0 0 0"));
    expect.push(1);
    assert_eq!(p, expect);
}

#[test]
fn encodes_beacon_trade_and_recipe_packets() {
    assert_eq!(
        encode_set_beacon(Some(3), None),
        ("set_beacon", cat(&[&[1], &vi(3), &[0]]))
    );
    assert_eq!(
        encode_set_beacon(None, Some(-1)),
        ("set_beacon", cat(&[&[0], &[1], &vi(-1)]))
    );
    assert_eq!(encode_select_trade(7), ("select_trade", vi(7)));

    let (n, p) = encode_place_recipe(2, "minecraft:stone", true);
    assert_eq!(n, "place_recipe");
    let mut expect = vec![2];
    expect.extend(s("minecraft:stone"));
    expect.push(1);
    assert_eq!(p, expect);

    assert_eq!(
        encode_recipe_book_change_settings(1, true, false),
        ("recipe_book_change_settings", cat(&[&vi(1), &[1], &[0]]))
    );
    let (n, p) = encode_recipe_book_seen_recipe("minecraft:bread");
    assert_eq!(n, "recipe_book_seen_recipe");
    assert_eq!(p, s("minecraft:bread"));
}

#[test]
fn encodes_rename_slot_state_advancements_teleport() {
    assert_eq!(encode_rename_item("Excalibur"), ("rename_item", s("Excalibur")));
    assert_eq!(
        encode_container_slot_state_changed(4, 1, true),
        ("container_slot_state_changed", cat(&[&vi(4), &vi(1), &[1]]))
    );
    let (n, p) = encode_seen_advancements(Some("minecraft:story/root"));
    assert_eq!(n, "seen_advancements");
    let mut expect = vi(0);
    expect.extend(s("minecraft:story/root"));
    assert_eq!(p, expect);
    assert_eq!(encode_seen_advancements(None), ("seen_advancements", vi(1)));
    assert_eq!(encode_teleport_to_entity([0xAB; 16]).1, vec![0xAB; 16]);
}

// ---------------------------------------------------------------------------
// S2C: decodes
// ---------------------------------------------------------------------------

#[test]
fn decodes_pong_response_and_border_packets() {
    match decode("pong_response", &be_i64(-42)) {
        PlayEvent::PongResponse { id } => assert_eq!(id, -42),
        other => panic!("unexpected {other:?}"),
    }

    let mut body = be_f64(1.5).to_vec();
    body.extend(be_f64(-2.0));
    body.extend(be_f64(6.0));
    body.extend(be_f64(4.0));
    body.extend(vl(500));
    body.extend(vi(29999984));
    body.extend(vi(150));
    body.extend(vi(200));
    match decode("initialize_border", &body) {
        PlayEvent::InitializeBorder {
            x,
            z,
            old_diameter,
            new_diameter,
            lerp_ms,
            portal_boundary,
            warning_blocks,
            warning_time,
        } => {
            assert_eq!(x, 1.5);
            assert_eq!(z, -2.0);
            assert_eq!(old_diameter, 6.0);
            assert_eq!(new_diameter, 4.0);
            assert_eq!(lerp_ms, 500);
            assert_eq!(portal_boundary, 29999984);
            assert_eq!(warning_blocks, 150);
            assert_eq!(warning_time, 200);
        }
        other => panic!("unexpected {other:?}"),
    }

    match decode("set_border_center", &cat(&[&be_f64(-15.5), &be_f64(33.25)])) {
        PlayEvent::SetBorderCenter { x, z } => {
            assert_eq!(x, -15.5);
            assert_eq!(z, 33.25);
        }
        other => panic!("unexpected {other:?}"),
    }
    match decode("set_border_size", &be_f64(1.5)) {
        PlayEvent::SetBorderSize { diameter } => assert_eq!(diameter, 1.5),
        other => panic!("unexpected {other:?}"),
    }
    let mut lerp = be_f64(140.0).to_vec();
    lerp.extend(be_f64(180.0));
    lerp.extend(vl(500));
    match decode("set_border_lerp_size", &lerp) {
        PlayEvent::SetBorderLerpSize {
            old_diameter,
            new_diameter,
            lerp_ms,
        } => {
            assert_eq!(old_diameter, 140.0);
            assert_eq!(new_diameter, 180.0);
            assert_eq!(lerp_ms, 500);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn decodes_set_player_team_modes() {
    // Mode 0 (create): name + params + entity list.
    let mut body = s("red");
    body.push(0);
    body.extend(chat_text_nbt("Red Team"));
    body.push(0x03); // friendly flags
    body.extend(s("always"));
    body.extend(s("push_other_teams"));
    body.extend(vi(-1)); // color: none
    body.extend(chat_text_nbt("[R] "));
    body.extend(chat_text_nbt(""));
    body.extend(vi(2));
    body.extend(s("Steve"));
    body.extend(s("Alex"));
    match decode("set_player_team", &body) {
        PlayEvent::SetPlayerTeam {
            name,
            mode,
            info,
            entities,
        } => {
            assert_eq!(name, "red");
            assert_eq!(mode, 0);
            let info = info.expect("mode 0 carries info");
            assert_eq!(info.friendly_flags, 0x03);
            assert_eq!(info.nametag_visibility, "always");
            assert_eq!(info.collision_rule, "push_other_teams");
            assert_eq!(info.color, -1);
            assert_eq!(entities, vec!["Steve".to_string(), "Alex".to_string()]);
        }
        other => panic!("unexpected {other:?}"),
    }

    // Mode 3 (add entities): no parameter block.
    let mut body = s("blue");
    body.push(3);
    body.extend(vi(1));
    body.extend(s("Herobrine"));
    match decode("set_player_team", &body) {
        PlayEvent::SetPlayerTeam {
            name, mode, info, entities, ..
        } => {
            assert_eq!(name, "blue");
            assert_eq!(mode, 3);
            assert!(info.is_none());
            assert_eq!(entities, vec!["Herobrine".to_string()]);
        }
        other => panic!("unexpected {other:?}"),
    }

    // Mode 5 is not a valid 767 team mode.
    let mut bad = s("x");
    bad.push(5);
    assert!(decode_extended_play_event("set_player_team", &bad).is_err());
}

#[test]
fn decodes_merchant_offers() {
    let mut body = vi(1); // window id
    body.extend(vi(1)); // one offer
    body.extend(merchant_offer());
    body.extend(vi(5)); // villager level
    body.extend(vi(11)); // villager xp
    body.push(1); // show progress
    body.push(1); // can restock
    match decode("merchant_offers", &body) {
        PlayEvent::MerchantOffers {
            window_id,
            offers,
            villager_level,
            villager_xp,
            show_progress,
            can_restock,
        } => {
            assert_eq!(window_id, 1);
            assert_eq!(offers.len(), 1);
            let offer = &offers[0];
            assert_eq!(offer.cost_a.item, 5);
            assert_eq!(offer.cost_a.count, 3);
            assert_eq!(offer.cost_a.components_wire, vec![0]);
            assert_eq!(offer.result_slot_wire, encode_slot_stack(7, 1));
            assert!(offer.cost_b.is_none());
            assert!(!offer.disabled);
            assert_eq!(offer.uses, 1);
            assert_eq!(offer.max_uses, 12);
            assert_eq!(offer.xp, 4);
            assert_eq!(offer.special_price, -2);
            assert_eq!(villager_level, 5);
            assert_eq!(villager_xp, 11);
            assert!(show_progress);
            assert!(can_restock);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn decodes_horse_map_look_at_and_tabs() {
    // horse_screen_open: window u8, slots varint, entity i32.
    let mut body = vec![3];
    body.extend(vi(9));
    body.extend(be_i32(-2));
    match decode("horse_screen_open", &body) {
        PlayEvent::HorseScreenOpen {
            window_id,
            slots,
            entity_id,
        } => {
            assert_eq!(window_id, 3);
            assert_eq!(slots, 9);
            assert_eq!(entity_id, -2);
        }
        other => panic!("unexpected {other:?}"),
    }

    // map_item_data with one icon and a 2x1 color patch.
    let mut body = vi(7); // map id
    body.push(0); // scale
    body.push(1); // locked
    body.push(1); // icons present
    body.extend(vi(1)); // one icon
    body.extend(vi(4)); // kind
    body.extend([0xF0u8, 0x10, 0x05]); // x, z, direction
    body.push(1); // name present
    body.extend(chat_text_nbt("Home"));
    body.push(2); // columns
    body.extend([1, 1, 3]); // rows, x, y
    body.extend(vi(2)); // 2 colors
    body.extend([0xAB, 0xCD]);
    match decode("map_item_data", &body) {
        PlayEvent::MapItemData {
            map_id,
            locked,
            icons,
            columns,
            rows,
            colors,
            ..
        } => {
            assert_eq!(map_id, 7);
            assert!(locked);
            let icons = icons.expect("icons present");
            assert_eq!(icons.len(), 1);
            assert_eq!(icons[0].kind, 4);
            assert_eq!(icons[0].x, -16);
            assert_eq!(icons[0].z, 16);
            assert_eq!(icons[0].direction, 5);
            assert!(icons[0].name_nbt.is_some());
            assert_eq!(columns, 2);
            assert_eq!(rows, 1);
            assert_eq!(colors, vec![0xAB, 0xCD]);
        }
        other => panic!("unexpected {other:?}"),
    }

    // map_item_data without patch (columns = 0).
    let mut body = vi(1);
    body.extend([0, 0, 0]); // scale, locked, no icons
    body.push(0); // columns = 0 -> no patch fields
    match decode("map_item_data", &body) {
        PlayEvent::MapItemData {
            columns, colors, ..
        } => {
            assert_eq!(columns, 0);
            assert!(colors.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }

    // player_look_at at an entity's eyes.
    let mut body = vi(1); // source anchor = eyes
    body.extend(be_f64(1.0));
    body.extend(be_f64(2.0));
    body.extend(be_f64(3.0));
    body.push(1); // is entity
    body.extend(vi(42));
    body.extend(vi(1)); // entity anchor = eyes
    match decode("player_look_at", &body) {
        PlayEvent::PlayerLookAt {
            anchor,
            target,
            entity,
        } => {
            assert_eq!(anchor, 1);
            assert_eq!(target, [1.0, 2.0, 3.0]);
            assert_eq!(entity, Some((42, 1)));
        }
        other => panic!("unexpected {other:?}"),
    }

    match decode("select_advancements_tab", &[0]) {
        PlayEvent::SelectAdvancementsTab { tab } => assert_eq!(tab, None),
        other => panic!("unexpected {other:?}"),
    }
    let mut body = vec![1];
    body.extend(s("minecraft:story/root"));
    match decode("select_advancements_tab", &body) {
        PlayEvent::SelectAdvancementsTab { tab } => {
            assert_eq!(tab.as_deref(), Some("minecraft:story/root"))
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn decodes_update_advancements_tree() {
    let mut body = vec![1]; // reset
    body.extend(vi(1)); // one advancement
    body.extend(s("minecraft:story/root"));
    body.extend(opt_string(Some("minecraft:story/mine_stone")));
    body.push(1); // display present
    body.extend(chat_text_nbt("Stone Age"));
    body.extend(chat_text_nbt("Mine stone"));
    body.extend(encode_slot_stack(1, 1)); // icon slot
    body.extend(vi(0)); // frame = task
    body.extend(be_i32(0x01 | 0x02)); // flags: background + toast
    body.extend(s("minecraft:textures/gui/advancements/backgrounds/stone.png"));
    body.extend(be_f32(0.0));
    body.extend(be_f32(0.0));
    body.extend(vi(1)); // one requirement group
    body.extend(vi(2)); // two criteria
    body.extend(s("minecraft:story/mine_stone"));
    body.extend(s("dummy"));
    body.push(1); // telemetry
    body.extend(vi(1)); // one removed id
    body.extend(s("minecraft:old/removed"));
    body.extend(vi(1)); // one progress entry
    body.extend(s("minecraft:story/root"));
    body.extend(vi(1)); // one criterion
    body.extend(s("minecraft:story/mine_stone"));
    body.push(1); // achieved
    body.extend(be_i64(1_700_000_000_000));
    match decode("update_advancements", &body) {
        PlayEvent::UpdateAdvancements {
            reset,
            advancements,
            removed,
            progress,
        } => {
            assert!(reset);
            assert_eq!(advancements.len(), 1);
            let adv = &advancements[0];
            assert_eq!(adv.key, "minecraft:story/root");
            assert_eq!(
                adv.parent_id.as_deref(),
                Some("minecraft:story/mine_stone")
            );
            let display = adv.display.as_ref().expect("display present");
            assert_eq!(display.frame_type, 0);
            assert_eq!(display.flags, 0x03);
            assert!(display.background.is_some());
            assert_eq!(adv.requirements.len(), 1);
            assert_eq!(adv.requirements[0].len(), 2);
            assert!(adv.sends_telemetry);
            assert_eq!(removed, vec!["minecraft:old/removed".to_string()]);
            assert_eq!(progress.len(), 1);
            assert_eq!(
                progress[0].criteria[0].1,
                Some(1_700_000_000_000)
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn decodes_award_stats_server_data_chunks_biomes() {
    // award_stats: count + (category, statistic, value) varint triples.
    let mut body = vi(2);
    for t in [(0, 1, 5), (2, 9, 42)] {
        body.extend(vi(t.0));
        body.extend(vi(t.1));
        body.extend(vi(t.2));
    }
    match decode("award_stats", &body) {
        PlayEvent::AwardStats { entries } => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].category, 0);
            assert_eq!(entries[0].statistic, 1);
            assert_eq!(entries[0].value, 5);
            assert_eq!(entries[1].value, 42);
        }
        other => panic!("unexpected {other:?}"),
    }

    // server_data: MOTD component + optional icon bytes.
    let mut body = chat_text_nbt("A Minecraft Server");
    body.push(1);
    body.extend(vi(4));
    body.extend([0x89, 0x50, 0x4E, 0x47]);
    match decode("server_data", &body) {
        PlayEvent::ServerData { motd_nbt, icon } => {
            assert_eq!(motd_nbt, chat_text_nbt("A Minecraft Server"));
            assert_eq!(icon.as_deref(), Some(&[0x89, 0x50, 0x4E, 0x47][..]));
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut body = chat_text_nbt("no icon");
    body.push(0);
    match decode("server_data", &body) {
        PlayEvent::ServerData { icon, .. } => assert!(icon.is_none()),
        other => panic!("unexpected {other:?}"),
    }

    // chunks_biomes: count + (z i32, x i32, varint len, data) per column.
    let mut body = vi(2);
    body.extend(be_i32(3)); // z
    body.extend(be_i32(-2)); // x
    body.extend(vi(2));
    body.extend([0xAB, 0xCD]);
    body.extend(be_i32(-1));
    body.extend(be_i32(7));
    body.extend(vi(0)); // empty data
    match decode("chunks_biomes", &body) {
        PlayEvent::ChunksBiomes { chunks } => {
            assert_eq!(chunks.len(), 2);
            assert_eq!(chunks[0].cz, 3);
            assert_eq!(chunks[0].cx, -2);
            assert_eq!(chunks[0].data, vec![0xAB, 0xCD]);
            assert_eq!(chunks[1].cx, 7);
            assert!(chunks[1].data.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// S2C: fail-closed behaviour
// ---------------------------------------------------------------------------

#[test]
fn clientbound_play_gap_decoders_cover_entity_tick_chat_and_ui_events() {
    assert!(matches!(decode("animate", &cat(&[&vi(42), &[0]])), PlayEvent::Animate { eid: 42, animation: 0 }));
    assert!(matches!(decode("set_chunk_cache_radius", &vi(12)), PlayEvent::SetChunkCacheRadius(12)));
    assert!(matches!(decode("set_simulation_distance", &vi(8)), PlayEvent::SetSimulationDistance(8)));
    assert!(matches!(decode("ticking_state", &cat(&[&be_f32(10.0), &[1]])), PlayEvent::TickingState { tick_rate: 10.0, frozen: true }));
    assert!(matches!(decode("ticking_step", &vi(3)), PlayEvent::TickingStep { steps: 3 }));
    assert!(matches!(decode("set_entity_link", &cat(&[&vi(4), &vi(5)])), PlayEvent::SetEntityLink { source: 4, destination: 5 }));
    let mut attributes = cat(&[&vi(42), &vi(1), &vi(5), &be_f64(20.0), &vi(1)]);
    attributes.extend([1u8; 16]);
    attributes.extend(be_f64(1.5));
    attributes.extend(vi(0));
    assert!(matches!(decode("update_attributes", &attributes), PlayEvent::UpdateAttributes { eid: 42, attributes } if attributes[0].attribute == 5 && attributes[0].base == 20.0 && attributes[0].modifiers[0].amount == 1.5));
    assert!(matches!(decode("cookie_request", &s("minecraft:settings")), PlayEvent::CookieRequest(key) if key == "minecraft:settings"));
    let mut cookie = s("minecraft:settings");
    cookie.extend(vi(2));
    cookie.extend([0xaa, 0xbb]);
    assert!(matches!(decode("store_cookie", &cookie), PlayEvent::StoreCookie { key, payload } if key == "minecraft:settings" && payload == [0xaa, 0xbb]));
    assert!(matches!(decode("set_passengers", &cat(&[&vi(7), &vi(2), &vi(8), &vi(9)])), PlayEvent::SetPassengers { vehicle: 7, passengers } if passengers == [8, 9]));
    assert!(matches!(decode("take_item_entity", &cat(&[&vi(11), &vi(12), &vi(2)])), PlayEvent::TakeItemEntity { item_entity: 11, player: 12, amount: 2 }));
    assert!(matches!(decode("command_suggestions", &cat(&[&vi(3), &vi(1), &vi(2), &vi(1), &s("help"), &[0]])), PlayEvent::CommandSuggestions { transaction_id: 3, start: 1, length: 2, matches } if matches[0].text == "help"));

    let mut chat = vec![7; 16];
    chat.extend_from_slice(&vi(0));
    chat.push(0);
    chat.extend_from_slice(&s("hello"));
    chat.extend_from_slice(&be_i64(123));
    chat.extend_from_slice(&be_i64(456));
    chat.extend_from_slice(&vi(0));
    chat.extend_from_slice(&[0, 0, 0]);
    chat.push(0);
    chat.extend_from_slice(&vi(0));
    chat.extend_from_slice(&vi(1));
    chat.extend_from_slice(&chat_text_nbt("sender"));
    chat.push(0);
    assert!(matches!(decode("player_chat", &chat), PlayEvent::PlayerChat { text, .. } if text == "hello"));
    assert!(matches!(decode("delete_chat", &vi(6)), PlayEvent::DeleteChat { message_index: Some(5), signature: None }));
}

#[test]
fn new_s2c_decoders_fail_closed_on_truncation_and_hostile_counts() {
    let bodies: &[(&str, Vec<u8>)] = &[
        ("pong_response", be_i64(1).to_vec()),
        (
            "initialize_border",
            cat(&[&be_f64(0.0), &be_f64(0.0), &be_f64(0.0), &be_f64(0.0), &vl(0), &vi(0), &vi(0), &vi(0)]),
        ),
        ("set_border_center", cat(&[&be_f64(0.0), &be_f64(0.0)])),
        ("set_border_size", be_f64(0.0).to_vec()),
        (
            "set_border_lerp_size",
            cat(&[&be_f64(0.0), &be_f64(0.0), &vl(0)]),
        ),
        (
            "set_player_team",
            cat(&[&s("t"), &[0], &chat_text_nbt("n"), &[0], &s("a"), &s("a"), &vi(0), &chat_text_nbt(""), &chat_text_nbt(""), &vi(0)]),
        ),
        (
            "merchant_offers",
            cat(&[&vi(0), &vi(1), &merchant_offer(), &vi(1), &vi(0), &[1], &[1]]),
        ),
        ("horse_screen_open", cat(&[&[0], &vi(3), &be_i32(1)])),
        (
            "map_item_data",
            cat(&[&vi(1), &[0], &[0], &[0], &[2], &[1, 1, 0, 0], &vi(2), &[0xAB, 0xCD]]),
        ),
        (
            "player_look_at",
            cat(&[&vi(0), &be_f64(0.0), &be_f64(0.0), &be_f64(0.0), &[0]]),
        ),
        (
            "update_advancements",
            cat(&[&[0], &vi(0), &vi(0), &vi(0)]),
        ),
        ("select_advancements_tab", vec![0]),
        ("award_stats", cat(&[&vi(1), &vi(0), &vi(0), &vi(0)])),
        ("server_data", chat_text_nbt("m")),
        (
            "chunks_biomes",
            cat(&[&vi(1), &be_i32(0), &be_i32(0), &vi(0)]),
        ),
    ];
    for (name, body) in bodies {
        assert_truncation_safe(name, body);
    }
}

#[test]
fn new_s2c_decoders_reject_oversized_counts() {
    // Huge count prefixes must error before any allocation.
    assert!(decode_extended_play_event("merchant_offers", &cat(&[&vi(0), &vi(i32::MAX)])).is_err());
    assert!(decode_extended_play_event("chunks_biomes", &vi(i32::MAX)).is_err());
    assert!(decode_extended_play_event("award_stats", &vi(i32::MAX)).is_err());
    assert!(decode_extended_play_event("update_advancements", &cat(&[&[0], &vi(i32::MAX)])).is_err());
    assert!(decode_extended_play_event("map_item_data", &cat(&[&vi(0), &[0], &[0], &[1], &vi(i32::MAX)])).is_err());
    assert!(decode_extended_play_event("set_player_team", &cat(&[&s("t"), &[3], &vi(i32::MAX)])).is_err());

    // Negative counts fail closed too.
    assert!(decode_extended_play_event("chunks_biomes", &vi(-1)).is_err());
    assert!(decode_extended_play_event("award_stats", &vi(-1)).is_err());

    // Map color count must equal columns * rows.
    let mut bad_map = vi(1);
    bad_map.extend([0, 0, 0]); // scale, locked, no icons
    bad_map.push(2); // columns
    bad_map.extend([1, 0, 0]); // rows=1 x=0 y=0 -> expect 2 colors
    bad_map.extend(vi(3)); // but declares 3
    bad_map.extend([0; 3]);
    assert!(decode_extended_play_event("map_item_data", &bad_map).is_err());

    // look-at anchors are 0|1 only.
    let mut bad_look = vi(2);
    bad_look.extend([0; 24]);
    bad_look.push(0);
    assert!(decode_extended_play_event("player_look_at", &bad_look).is_err());
}

#[test]
fn hostile_slot_bodies_fail_closed_in_merchant_and_advancements() {
    // Merchant offer whose result slot claims a huge component count.
    let mut offer = item_cost(1, 1);
    offer.extend(vi(1)); // slot count
    offer.extend(vi(1)); // item id
    offer.extend(vi(i32::MAX)); // absurd added-component count
    let mut body = vi(0);
    body.extend(vi(1));
    body.extend(offer);
    assert!(decode_extended_play_event("merchant_offers", &body).is_err());

    // Advancement display whose icon slot is truncated mid-component.
    let mut body = vec![0]; // no reset
    body.extend(vi(1)); // one advancement
    body.extend(s("k"));
    body.extend([0]); // no parent
    body.push(1); // display present
    body.extend(chat_text_nbt("t"));
    body.extend(chat_text_nbt("d"));
    body.extend(encode_slot_stack(1, 1)); // icon slot ok
    body.extend(vi(0)); // frame
    body.extend(be_i32(0)); // flags: no background
    body.extend(be_f32(0.0));
    body.extend(be_f32(0.0));
    body.extend(vi(0)); // no requirements
    body.push(0); // no telemetry
    body.extend(vi(0)); // no removed
    body.extend(vi(0)); // no progress
    match decode("update_advancements", &body) {
        PlayEvent::UpdateAdvancements { advancements, .. } => {
            assert_eq!(advancements.len(), 1);
            assert!(advancements[0].display.is_some());
        }
        other => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// CLI: --audio-sink coreaudio|winmm (case-insensitive)
// ---------------------------------------------------------------------------

#[test]
fn cli_audio_sink_accepts_native_backends_case_insensitive() {
    let cfg = parse_args(&["--audio-sink", "coreaudio"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::CoreAudio);
    let cfg = parse_args(&["--audio-sink", "CoreAudio"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::CoreAudio);
    let cfg = parse_args(&["--audio-sink=WINMM"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::WinMm);
    let cfg = parse_args(&["--audio-sink", "winmm"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::WinMm);
    // Existing values keep working and are now case-insensitive too.
    let cfg = parse_args(&["--audio-sink", "PipeWire"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::PipeWire);
    let cfg = parse_args(&["--audio-sink", "ALSA"]).unwrap();
    assert_eq!(cfg.audio_sink, AudioSinkKind::Alsa);
    assert!(parse_args(&["--audio-sink", "coreaudio2"]).is_err());
    assert!(parse_args(&["--audio-sink", "wasapi"]).is_err());
}

// ---------------------------------------------------------------------------
// C2S resource_pack: 767 + 777 wire ids and the uuid+status payload
// ---------------------------------------------------------------------------

#[test]
fn resource_pack_response_resolves_per_version() {
    use megu_client::protocol::encode_resource_pack;
    use megu_proto::{PacketDir, PacketPhase, VersionRouter};

    let uuid = [0xabu8; 16];
    let (name, payload) = encode_resource_pack(uuid, 3);
    assert_eq!(name, "resource_pack");
    assert_eq!(&payload[..16], &uuid);
    assert_eq!(&payload[16..], vi(3).as_slice());

    let router = VersionRouter::with_v767_and_v777();
    let t767 = router.select(767).unwrap().table().clone();
    let t777 = router.select(777).unwrap().table().clone();
    // Play-phase serverbound ids differ between the two versions.
    assert_eq!(
        t767.packet_id(PacketPhase::Play, PacketDir::Serverbound, "resource_pack"),
        Some(0x2b)
    );
    assert_eq!(
        t777.packet_id(PacketPhase::Play, PacketDir::Serverbound, "resource_pack"),
        Some(0x32)
    );
    // Configuration-phase: 767 names it `resource_pack_response`,
    // 777 names it `resource_pack` — the sender picks whichever the
    // active table knows.
    assert_eq!(
        t767.packet_id(
            PacketPhase::Configuration,
            PacketDir::Serverbound,
            "resource_pack_response"
        ),
        Some(0x06)
    );
    assert_eq!(
        t767.packet_id(
            PacketPhase::Configuration,
            PacketDir::Serverbound,
            "resource_pack"
        ),
        None
    );
    assert_eq!(
        t777.packet_id(
            PacketPhase::Configuration,
            PacketDir::Serverbound,
            "resource_pack"
        ),
        Some(0x06)
    );
}

#[test]
fn resource_pack_status_codes_match_vanilla_actions() {
    use megu_client::protocol::{
        RESOURCE_PACK_ACCEPTED, RESOURCE_PACK_DECLINED, RESOURCE_PACK_DISCARDED,
        RESOURCE_PACK_DOWNLOADED, RESOURCE_PACK_FAILED_DOWNLOAD, RESOURCE_PACK_FAILED_RELOAD,
        RESOURCE_PACK_INVALID_URL, RESOURCE_PACK_SUCCESSFULLY_LOADED,
    };
    assert_eq!(RESOURCE_PACK_SUCCESSFULLY_LOADED, 0);
    assert_eq!(RESOURCE_PACK_DECLINED, 1);
    assert_eq!(RESOURCE_PACK_FAILED_DOWNLOAD, 2);
    assert_eq!(RESOURCE_PACK_ACCEPTED, 3);
    assert_eq!(RESOURCE_PACK_DOWNLOADED, 4);
    assert_eq!(RESOURCE_PACK_INVALID_URL, 5);
    assert_eq!(RESOURCE_PACK_FAILED_RELOAD, 6);
    assert_eq!(RESOURCE_PACK_DISCARDED, 7);
}
