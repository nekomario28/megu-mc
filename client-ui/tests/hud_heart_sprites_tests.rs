// SPDX-License-Identifier: LGPL-3.0-or-later
//! Vanilla 1.21.1 heart-sprite parity through the real HUD render path:
//! family selection (normal / poison / wither / frozen / absorbing /
//! hardcore / mount), the damage blink twins, the regeneration bounce, and
//! the low-health jitter.
//!
//! Every sprite key the HUD can bind is installed in a fake atlas with a
//! unique UV payload, so the emitted `@gui_uv` quad identifies exactly which
//! Vanilla key the painter asked for.

use megu_client_ui::draw::{GuiSpriteAtlas, GuiSpriteEntry, UiCommand, UiDrawList, UiRect};
use megu_client_ui::hud_sprite_catalog::{
    HeartContainer, HeartFill, HeartJitter, heart_row,
};
use megu_client_ui::state::{HeartStyle, LiveStatusEffect, Screen, UiState};

const VIEW: (u32, u32) = (1280, 720);

/// Every heart key the catalog can emit, in binding order; the position in
/// this list is the index the fake UV payload carries.
const HEART_KEYS: &[&str] = &megu_client_ui::hud_sprite_catalog::HEART_SPRITE_KEYS;

fn entry(index: usize) -> GuiSpriteEntry {
    let u = index as f32;
    GuiSpriteEntry {
        uv_min: [u, 0.0],
        uv_max: [u + 0.5, 1.0],
        width: 9,
        height: 9,
        nine_slice_border: [0, 0, 0, 0],
    }
}

/// Atlas with the heart family installed; `twins = false` drops every
/// `_blinking` key so the resting-set fallback is exercised.
fn heart_atlas(twins: bool) -> GuiSpriteAtlas {
    let mut atlas = GuiSpriteAtlas::default();
    for (index, key) in HEART_KEYS.iter().enumerate() {
        if !twins && key.ends_with("_blinking") {
            continue;
        }
        atlas.insert_named_sprite(*key, entry(index));
    }
    atlas
}

fn hud_ui() -> UiState {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui
}

fn effect(name: &str, duration_ticks: u32) -> LiveStatusEffect {
    LiveStatusEffect {
        effect_id: 0,
        name: name.to_string(),
        amplifier: 0,
        duration_ticks,
        beneficial: true,
    }
}

/// `(key, rect)` for every heart sprite the HUD emitted, in draw order.
fn heart_quads(list: &UiDrawList) -> Vec<(&'static str, UiRect)> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::ItemIcon { rect, item_id } => {
                let index = decode_index(item_id)?;
                HEART_KEYS.get(index).map(|key| (*key, *rect))
            }
            _ => None,
        })
        .collect()
}

fn decode_index(item_id: &str) -> Option<usize> {
    let rest = item_id.strip_prefix("@gui_uv:")?;
    let mut fields = rest.split(':');
    let u_min = u32::from_str_radix(fields.next()?, 16).ok()?;
    let index = f32::from_bits(u_min) as usize;
    // The fake entries encode the index in `uv_min[0]`; reject anything the
    // HUD bound from a different atlas (the hotbar, XP bar, ...).
    (index < HEART_KEYS.len()).then_some(index)
}

fn render(ui: &mut UiState, atlas: &GuiSpriteAtlas) -> UiDrawList {
    let mut list = UiDrawList::new();
    ui.render_into_with_gui_atlas(VIEW.0, VIEW.1, &mut list, Some(atlas));
    list
}

fn count(quads: &[(&'static str, UiRect)], key: &str) -> usize {
    quads.iter().filter(|(k, _)| *k == key).count()
}

/// Physical pixels per logical pixel, read off the 9 px heart sprite.
fn hud_scale(quads: &[(&'static str, UiRect)]) -> i32 {
    let (_, rect) = quads.first().expect("no heart sprites were emitted");
    (rect.h as i32) / 9
}

#[test]
fn plain_hearts_draw_the_container_and_full_fill() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    let quads = heart_quads(&render(&mut ui, &atlas));

    // Health 20 = ten full hearts, each a container plus a fill quad.
    assert_eq!(count(&quads, "hud/heart/container"), 10);
    assert_eq!(count(&quads, "hud/heart/full"), 10);
    assert_eq!(quads.len(), 20, "{quads:?}");
}

#[test]
fn half_hearts_use_the_half_fill() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.health = 3.0;
    let quads = heart_quads(&render(&mut ui, &atlas));

    assert_eq!(count(&quads, "hud/heart/full"), 1);
    assert_eq!(count(&quads, "hud/heart/half"), 1);
    // Anything past the filled hearts only keeps its container.
    assert_eq!(count(&quads, "hud/heart/container"), 10);
}

#[test]
fn hardcore_swaps_both_the_container_and_the_fill() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.hardcore = true;
    ui.hud.health = 7.0;
    let quads = heart_quads(&render(&mut ui, &atlas));

    assert_eq!(count(&quads, "hud/heart/container_hardcore"), 10);
    assert_eq!(count(&quads, "hud/heart/hardcore_full"), 3);
    assert_eq!(count(&quads, "hud/heart/hardcore_half"), 1);
    // No plain container or plain fill may leak into a hardcore row.
    assert_eq!(count(&quads, "hud/heart/container"), 0);
    assert_eq!(count(&quads, "hud/heart/full"), 0);
    assert_eq!(count(&quads, "hud/heart/half"), 0);
}

#[test]
fn poison_wither_and_frozen_swap_only_the_fill() {
    for (effect_name, fill) in [
        ("Poison", "hud/heart/poisoned_full"),
        ("Wither", "hud/heart/withered_full"),
    ] {
        let atlas = heart_atlas(true);
        let mut ui = hud_ui();
        ui.hud.status_effects.push(effect(effect_name, 400));
        let quads = heart_quads(&render(&mut ui, &atlas));

        assert_eq!(count(&quads, fill), 10, "{effect_name} fill");
        assert_eq!(
            count(&quads, "hud/heart/container"),
            10,
            "{effect_name} keeps the plain container"
        );
        assert_eq!(count(&quads, "hud/heart/full"), 0, "{effect_name} leak");
    }

    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.screen_overlay.freezing_alpha = 0.5;
    let quads = heart_quads(&render(&mut ui, &atlas));
    assert_eq!(count(&quads, "hud/heart/frozen_full"), 10);
    assert_eq!(count(&quads, "hud/heart/container"), 10);
}

#[test]
fn hardcore_wins_over_the_damage_families() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.hardcore = true;
    ui.hud.status_effects.push(effect("Poison", 400));
    let quads = heart_quads(&render(&mut ui, &atlas));

    assert_eq!(count(&quads, "hud/heart/hardcore_full"), 10);
    assert_eq!(count(&quads, "hud/heart/poisoned_full"), 0);
}

#[test]
fn absorption_draws_absorbing_fills_and_the_hardcore_container() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.absorption = 4.0;
    let quads = heart_quads(&render(&mut ui, &atlas));
    assert_eq!(count(&quads, "hud/heart/absorbing_full"), 2);
    // The absorption row keeps the shared full-width container row.
    assert_eq!(count(&quads, "hud/heart/container"), 20);

    let mut ui = hud_ui();
    ui.hud.absorption = 4.0;
    ui.hud.hardcore = true;
    let quads = heart_quads(&render(&mut ui, &atlas));
    // The absorption row keeps its golden fill but takes the hardcore slot.
    assert_eq!(count(&quads, "hud/heart/absorbing_full"), 2);
    assert_eq!(count(&quads, "hud/heart/container_hardcore"), 20);
    assert_eq!(count(&quads, "hud/heart/container"), 0);
}

#[test]
fn mount_health_uses_the_vehicle_sprites() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.mount_health = Some((14.0, 20.0));
    let quads = heart_quads(&render(&mut ui, &atlas));

    assert_eq!(count(&quads, "hud/heart/vehicle_container"), 10);
    assert_eq!(count(&quads, "hud/heart/vehicle_full"), 7);
}

#[test]
fn damage_flash_lights_the_blinking_twins_then_relaxes() {
    let atlas = heart_atlas(true);

    // Alpha 0.5 = ten of the twenty blink ticks: (10 / 3) is odd, so the
    // blinking twins are lit for both the container and the fill.
    let mut ui = hud_ui();
    ui.hud.screen_overlay.hurt_alpha = 0.5;
    let quads = heart_quads(&render(&mut ui, &atlas));
    assert_eq!(count(&quads, "hud/heart/container_blinking"), 10);
    assert_eq!(count(&quads, "hud/heart/full_blinking"), 10);
    assert_eq!(count(&quads, "hud/heart/container"), 0);
    assert_eq!(count(&quads, "hud/heart/full"), 0);

    // A full flash reads as twenty ticks: (20 / 3) is even, so the resting
    // sprites come back.
    let mut ui = hud_ui();
    ui.hud.screen_overlay.hurt_alpha = 1.0;
    let quads = heart_quads(&render(&mut ui, &atlas));
    assert_eq!(count(&quads, "hud/heart/container"), 10);
    assert_eq!(count(&quads, "hud/heart/full"), 10);
    assert_eq!(count(&quads, "hud/heart/full_blinking"), 0);
}

#[test]
fn blinking_falls_back_to_the_resting_set_without_twins() {
    let atlas = heart_atlas(false);
    let mut ui = hud_ui();
    ui.hud.screen_overlay.hurt_alpha = 0.5;
    let quads = heart_quads(&render(&mut ui, &atlas));

    assert_eq!(count(&quads, "hud/heart/container"), 10);
    assert_eq!(count(&quads, "hud/heart/full"), 10);
    assert_eq!(quads.len(), 20, "half a flicker must not be drawn");
}

#[test]
fn regeneration_wave_lifts_exactly_one_heart() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    // 6 % (20 + 5) = 6, and 6 / 2 = heart index 3 — the fourth heart.
    ui.hud.status_effects.push(effect("Regeneration", 6));
    let quads = heart_quads(&render(&mut ui, &atlas));
    let scale = hud_scale(&quads);

    let containers: Vec<UiRect> = quads
        .iter()
        .filter(|(key, _)| *key == "hud/heart/container")
        .map(|(_, rect)| *rect)
        .collect();
    assert_eq!(containers.len(), 10);
    let resting_y = containers[0].y;
    let lifted: Vec<usize> = containers
        .iter()
        .enumerate()
        .filter(|(_, rect)| rect.y == resting_y - scale)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(lifted, vec![3], "one-pixel bounce on heart 3");

    // The fill bounces with its container.
    let fills: Vec<UiRect> = quads
        .iter()
        .filter(|(key, _)| *key == "hud/heart/full")
        .map(|(_, rect)| *rect)
        .collect();
    assert_eq!(fills.len(), 10);
    assert_eq!(fills[3].y, resting_y - scale, "lifted fill");
    assert_eq!(fills[0].y, resting_y, "resting fill");
}

#[test]
fn regeneration_wave_wraps_with_the_effect_countdown() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.status_effects.push(effect("Regeneration", 25 + 2));
    let quads = heart_quads(&render(&mut ui, &atlas));
    let scale = hud_scale(&quads);
    let containers: Vec<UiRect> = quads
        .iter()
        .filter(|(key, _)| *key == "hud/heart/container")
        .map(|(_, rect)| *rect)
        .collect();
    let resting_y = containers[0].y;
    let lifted: Vec<usize> = containers
        .iter()
        .enumerate()
        .filter(|(_, rect)| rect.y == resting_y - scale)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(lifted, vec![1], "27 % 25 = 2, i.e. heart index 1");
}

#[test]
fn low_health_jitter_is_vanilla_seeded_and_stable() {
    let atlas = heart_atlas(true);
    let scale = hud_scale(&heart_quads(&render(&mut hud_ui(), &atlas)));

    // Two hearts at four hit points: the vanilla tick-0 stream is [1, 1],
    // so both hearts sit one pixel low.
    let mut ui = hud_ui();
    ui.hud.health = 4.0;
    let quads = heart_quads(&render(&mut ui, &atlas));
    let containers: Vec<UiRect> = quads
        .iter()
        .filter(|(key, _)| *key == "hud/heart/container")
        .map(|(_, rect)| *rect)
        .collect();
    let resting_y = hud_resting_y(&atlas);
    assert_eq!(containers.len(), 10);
    for (index, expected) in [(0usize, 1), (1, 1)] {
        let mut jitter = HeartJitter::new(0, 4.0);
        let offsets: Vec<i32> = (0..=index).map(|_| jitter.next_offset()).collect();
        assert_eq!(offsets[index], expected, "vanilla tick-0 stream");
        assert_eq!(
            containers[index].y,
            resting_y + expected * scale,
            "heart {index} offset"
        );
    }
    // Hearts past the player's health still jitter, exactly like vanilla:
    // the RNG walks the whole row.
    let mut jitter = HeartJitter::new(0, 4.0);
    for (index, rect) in containers.iter().enumerate() {
        let offset = jitter.next_offset();
        assert_eq!(rect.y, resting_y + offset * scale, "heart {index}");
    }

    // Deterministic: the same snapshot renders the same pixels.
    let again = heart_quads(&render(&mut ui, &atlas));
    assert_eq!(quads, again);

    // Above four hit points nothing moves.
    let mut ui = hud_ui();
    ui.hud.health = 5.0;
    let quads = heart_quads(&render(&mut ui, &atlas));
    assert!(quads.iter().all(|(_, rect)| rect.y == resting_y));
}

/// Resting (un-jittered) top of the health row, read from a full bar where
/// the low-health gate is off.
fn hud_resting_y(atlas: &GuiSpriteAtlas) -> i32 {
    let mut ui = hud_ui();
    ui.hud.health = 20.0;
    let quads = heart_quads(&render(&mut ui, atlas));
    quads
        .iter()
        .find(|(key, _)| *key == "hud/heart/container")
        .map(|(_, rect)| rect.y)
        .expect("no container quad")
}

#[test]
fn catalog_rows_match_the_keys_the_painter_binds() {
    let atlas = heart_atlas(true);
    for (container, fill) in [
        (HeartContainer::Normal, HeartFill::Normal),
        (HeartContainer::Hardcore, HeartFill::Hardcore),
        (HeartContainer::Normal, HeartFill::Absorbing),
        (HeartContainer::Vehicle, HeartFill::Vehicle),
    ] {
        for blinking in [false, true] {
            let row = heart_row(container, fill, blinking);
            assert!(
                row.present(|key| atlas.has_named_sprite(key)),
                "catalog row {row:?} is not bindable from the fake atlas"
            );
        }
    }

    // The HUD never invents a heart key outside the Vanilla texture set.
    let mut ui = hud_ui();
    ui.hud.hardcore = true;
    ui.hud.absorption = 6.0;
    ui.hud.health = 9.0;
    ui.hud.screen_overlay.hurt_alpha = 0.5;
    ui.hud.status_effects.push(effect("Wither", 200));
    for (key, _) in heart_quads(&render(&mut ui, &atlas)) {
        assert!(HEART_KEYS.contains(&key), "unknown heart key {key}");
    }
}

#[test]
fn explicit_heart_style_is_honoured() {
    let atlas = heart_atlas(true);
    let mut ui = hud_ui();
    ui.hud.heart_style = HeartStyle::Frozen;
    let quads = heart_quads(&render(&mut ui, &atlas));
    assert_eq!(count(&quads, "hud/heart/frozen_full"), 10);
}
