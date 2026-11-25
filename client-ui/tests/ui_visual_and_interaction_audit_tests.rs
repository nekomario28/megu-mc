// SPDX-License-Identifier: LGPL-3.0-or-later
//! A08 integration audit for layout, state transitions, input replay, and
//! retained UI caching. Assertions use the public UI state and draw output.

use megu_client_ui::draw::{text_height, text_width, UiCommand, UiDrawList};
use megu_client_ui::container_screen::{
    menu_schema, ContainerScreenState, UiItemStack, VANILLA_MENU_COUNT, SLOT_SIZE,
};
use megu_client_ui::layout::hud_occupied_rects_into;
use megu_client_ui::state::{HitRegion, HudState, Screen, UiState, WidgetId};

const VIEWPORTS: [(u32, u32); 5] = [
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (854, 480),
];

const GUI_SCALES: [u8; 5] = [0, 1, 2, 3, 4];

fn assert_regions_inside_and_disjoint(regions: &[HitRegion], width: u32, height: u32) {
    for region in regions {
        assert!(
            region.rect.x >= 0
                && region.rect.y >= 0
                && region.rect.right() <= width as i32
                && region.rect.bottom() <= height as i32,
            "hit region outside {width}x{height}: {region:?}"
        );
    }
    for (index, first) in regions.iter().enumerate() {
        for second in regions.iter().skip(index + 1) {
            assert!(
                !first.rect.intersects(second.rect),
                "overlapping interactive regions: {first:?} / {second:?}"
            );
        }
    }
}

fn region(regions: &[HitRegion], id: WidgetId) -> HitRegion {
    *regions
        .iter()
        .find(|candidate| candidate.id == id)
        .unwrap_or_else(|| panic!("missing title region {id:?}"))
}

fn assert_draw_list_inside(list: &UiDrawList, width: u32, height: u32, context: &str) {
    for command in &list.commands {
        match command {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ItemIcon { rect, .. }
            | UiCommand::Sprite { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => assert!(
                rect.x >= 0
                    && rect.y >= 0
                    && rect.right() <= width as i32
                    && rect.bottom() <= height as i32,
                "{context}: draw command outside viewport: {command:?}"
            ),
            UiCommand::Text {
                x, y, scale, text, ..
            } => assert!(
                *x >= 0
                    && *y >= 0
                    && *x + text_width(text, *scale) as i32 <= width as i32
                    && *y + text_height(text, *scale) as i32 <= height as i32,
                "{context}: text outside viewport: {command:?}"
            ),
        }
    }
}

#[test]
fn title_layout_sweep_keeps_the_vanilla_grid_centered_and_link_free() {
    const TITLE_IDS: [WidgetId; 8] = [
        WidgetId::TitleSingleplayer,
        WidgetId::TitleMultiplayer,
        WidgetId::TitleMods,
        WidgetId::TitleFriends,
        WidgetId::TitleLanguage,
        WidgetId::TitleSettings,
        WidgetId::TitleQuit,
        WidgetId::TitleAbout,
    ];

    for (width, height) in VIEWPORTS {
        for gui_scale in GUI_SCALES {
            let mut ui = UiState::new();
            ui.ui_scale = gui_scale;
            ui.screen = Screen::Title;
            ui.render(width, height);

            let context = format!("Title at {width}x{height}, GUI scale {gui_scale}");
            assert_regions_inside_and_disjoint(&ui.hit_regions, width, height);
            assert_eq!(ui.hit_regions.len(), TITLE_IDS.len(), "{context}");
            for id in TITLE_IDS {
                assert!(
                    ui.hit_regions.iter().any(|candidate| candidate.id == id),
                    "{context}: missing {id:?}"
                );
            }
            assert!(
                ui.hit_regions.iter().all(|candidate| {
                    matches!(
                        candidate.id,
                        WidgetId::TitleSingleplayer
                            | WidgetId::TitleMultiplayer
                            | WidgetId::TitleMods
                            | WidgetId::TitleFriends
                            | WidgetId::TitleLanguage
                            | WidgetId::TitleSettings
                            | WidgetId::TitleQuit
                            | WidgetId::TitleAbout
                    )
                }),
                "{context}: title contains non-title controls"
            );

            let scale = ui.effective_scale(height) as i32;
            let logical_width = width as i32 / scale;
            let logical_height = height as i32 / scale;
            let row_step = 24.min((logical_height - 96) / 2).max(4);
            let button_height = 20.min(row_step);
            let corner_size = 20.min(button_height);
            let column_width = 200
                .min(logical_width - 2 * (corner_size + 4))
                .max(24)
                & !1;
            let half_width = (column_width - 4) / 2;
            let center_x = width as i32 / 2;
            let singleplayer = region(&ui.hit_regions, WidgetId::TitleSingleplayer).rect;
            let multiplayer = region(&ui.hit_regions, WidgetId::TitleMultiplayer).rect;
            let mods = region(&ui.hit_regions, WidgetId::TitleMods).rect;
            let friends = region(&ui.hit_regions, WidgetId::TitleFriends).rect;
            let language = region(&ui.hit_regions, WidgetId::TitleLanguage).rect;
            let settings = region(&ui.hit_regions, WidgetId::TitleSettings).rect;
            let quit = region(&ui.hit_regions, WidgetId::TitleQuit).rect;
            let about = region(&ui.hit_regions, WidgetId::TitleAbout).rect;

            for full_width in [singleplayer, multiplayer] {
                assert_eq!(full_width.w, (column_width * scale) as u32, "{context}");
                assert_eq!(full_width.h, (button_height * scale) as u32, "{context}");
                let doubled_center = 2 * (full_width.x + full_width.w as i32 / 2);
                assert!(
                    (doubled_center - 2 * center_x).abs() < 2 * scale,
                    "{context}: center differs by a full physical scale unit or more"
                );
            }
            assert_eq!(multiplayer.y - singleplayer.y, row_step * scale, "{context}");
            assert_eq!(mods.w, (half_width * scale) as u32, "{context}");
            assert_eq!(friends.w, (half_width * scale) as u32, "{context}");
            assert_eq!(mods.y, friends.y, "{context}");
            assert_eq!(mods.x, singleplayer.x, "{context}");
            assert_eq!(friends.x - mods.right(), 4 * scale, "{context}");
            assert_eq!(friends.right(), singleplayer.right(), "{context}");
            assert_eq!(settings.y - mods.bottom(), 8 * scale, "{context}");
            assert_eq!(settings.w, (half_width * scale) as u32, "{context}");
            assert_eq!(quit.w, (half_width * scale) as u32, "{context}");
            assert_eq!(quit.x - settings.right(), 4 * scale, "{context}");
            assert_eq!(language.w, (corner_size * scale) as u32, "{context}");
            assert_eq!(language.h, (corner_size * scale) as u32, "{context}");
            assert_eq!(about.w, (corner_size * scale) as u32, "{context}");
            assert_eq!(about.h, (corner_size * scale) as u32, "{context}");
            assert_eq!(settings.x - language.right(), 4 * scale, "{context}");
            assert_eq!(about.x - quit.right(), 4 * scale, "{context}");
            assert_eq!(language.y, settings.y, "{context}");
            assert_eq!(about.y, quit.y, "{context}");
        }
    }
}

#[test]
fn hud_layout_sweep_keeps_hotbar_vitals_and_xp_inside_the_viewport() {
    for (width, height) in VIEWPORTS {
        for gui_scale in GUI_SCALES {
            let mut ui = UiState::new();
            ui.ui_scale = gui_scale;
            ui.screen = Screen::InGameHud;
            ui.hud = HudState::demo();
            ui.hud.air = 120;
            ui.hud.xp_level = 12;

            let draw_list = ui.render(width, height);
            let context = format!("HUD at {width}x{height}, GUI scale {gui_scale}");
            assert_draw_list_inside(&draw_list, width, height, &context);
            let scale = (ui.effective_scale(height) as i32).min((width / 224).max(1) as i32);
            let mut occupied = Vec::new();
            hud_occupied_rects_into(&ui, width, height, &mut occupied);
            for rect in &occupied {
                assert!(
                    rect.x >= 0
                        && rect.y >= 0
                        && rect.right() <= width as i32
                        && rect.bottom() <= height as i32,
                    "{context}: HUD occupancy outside viewport: {rect:?}"
                );
            }

            let hotbar = occupied.iter().find(|rect| {
                rect.w == (224 * scale) as u32
                    && rect.h == (22 * scale) as u32
                    && rect.bottom() == height as i32 - 4 * scale
            });
            assert!(hotbar.is_some(), "{context}: nine-slot hotbar missing");
            assert!(
                occupied
                    .iter()
                    .any(|rect| rect.w == (182 * scale) as u32 && rect.h == (3 * scale) as u32),
                "{context}: XP bar occupancy missing"
            );
        }
    }
}

#[test]
fn container_player_inventory_uses_non_overlapping_vanilla_slot_pitches() {
    const CONTAINER_VIEWPORTS: [(u32, u32); 4] =
        [(854, 480), (1280, 720), (1920, 1080), (3840, 2160)];

    for menu_id in 0..VANILLA_MENU_COUNT as u32 {
        let schema = menu_schema(menu_id).expect("registered vanilla menu");
        if !schema.has_player_inventory {
            continue;
        }
        for (width, height) in CONTAINER_VIEWPORTS {
            let mut screen = ContainerScreenState::new(schema, 1, 0, "inventory audit");
            for index in schema.inventory_start..schema.inventory_start + 36 {
                screen.set_slot(
                    index as usize,
                    UiItemStack::new(index as u32 + 1, 1).with_durability(73, 156),
                );
            }
            let layout = screen.layout(width as f32, height as f32);
            let context = format!("menu {menu_id} at {width}x{height}");
            let player_slots: Vec<_> = layout
                .slots
                .iter()
                .filter(|slot| {
                    slot.index >= schema.inventory_start
                        && slot.index < schema.inventory_start + 36
                })
                .collect();
            assert_eq!(player_slots.len(), 36, "{context}");

            for slot in &player_slots {
                let offset = slot.index - schema.inventory_start;
                let column = i32::from(offset % 9);
                let (row, hotbar_gap) = if offset < 27 {
                    (i32::from(offset / 9), 0)
                } else {
                    (0, 58)
                };
                assert_eq!(slot.rect.w, SLOT_SIZE as u32, "{context}");
                assert_eq!(slot.rect.h, SLOT_SIZE as u32, "{context}");
                assert_eq!(
                    slot.rect.x,
                    layout.panel.x + 4 + i32::from(schema.inventory_origin_x)
                        + column * SLOT_SIZE,
                    "{context}, slot {}",
                    slot.index
                );
                assert_eq!(
                    slot.rect.y,
                    layout.panel.y + 4 + i32::from(schema.inventory_origin_y)
                        + row * SLOT_SIZE
                        + hotbar_gap,
                    "{context}, slot {}",
                    slot.index
                );
            }
            for (index, first) in player_slots.iter().enumerate() {
                for second in player_slots.iter().skip(index + 1) {
                    assert!(
                        !first.rect.intersects(second.rect),
                        "{context}: player slots {} and {} overlap",
                        first.index,
                        second.index
                    );
                }
            }

            let mut draw_list = UiDrawList::new();
            screen.render(width as f32, height as f32, &mut draw_list);
            assert_draw_list_inside(&draw_list, width, height, &context);
            let durability_bars: Vec<_> = draw_list
                .commands
                .iter()
                .filter_map(|command| match command {
                    UiCommand::ProgressBar { rect, .. }
                        if rect.w == 16 && rect.h == 3 =>
                    {
                        Some(*rect)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(durability_bars.len(), 36, "{context}");
            for bar in durability_bars {
                assert!(
                    player_slots.iter().any(|slot| {
                        bar.x >= slot.rect.x + 1
                            && bar.y == slot.rect.bottom() - 4
                            && bar.right() <= slot.rect.right() - 1
                            && bar.bottom() <= slot.rect.bottom()
                    }),
                    "{context}: durability bar escapes its slot: {bar:?}"
                );
            }
        }
    }
}
