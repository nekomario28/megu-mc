// SPDX-License-Identifier: LGPL-3.0-or-later
//! Behavior tests for the declarative container/menu screen engine.

use megu_client_ui::container_screen::{
    menu_schema, ContainerScreenState, SlotKind, UiItemStack, VANILLA_MENU_COUNT,
};
use megu_client_ui::draw::{
    rasterize_draw_list, text_height, text_width, UiColor, UiCommand, UiDrawList,
};

fn state(menu_id: u32) -> ContainerScreenState {
    ContainerScreenState::new(menu_schema(menu_id).unwrap(), 1, 0, "test menu")
}

fn stack(item_id: u32, count: u8) -> UiItemStack {
    UiItemStack::new(item_id, count).with_name(format!("Item {item_id}"))
}

fn assert_conserved(before: u64, screen: &ContainerScreenState) {
    assert_eq!(
        before,
        screen.total_count(),
        "non-drop click must conserve items"
    );
}

fn assert_draws_within_viewport(list: &UiDrawList, w: u32, h: u32) {
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ItemIcon { rect, .. }
            | UiCommand::Sprite { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => {
                assert!(
                    rect.x >= 0
                        && rect.y >= 0
                        && rect.right() <= w as i32
                        && rect.bottom() <= h as i32,
                    "draw outside viewport: {cmd:?}"
                );
            }
            UiCommand::Text {
                x, y, scale, text, ..
            } => {
                let right = *x + text_width(text, *scale) as i32;
                let bottom = *y + text_height(text, *scale) as i32;
                assert!(
                    *x >= 0 && *y >= 0 && right <= w as i32 && bottom <= h as i32,
                    "text outside viewport: {cmd:?}"
                );
            }
        }
    }
}

#[test]
fn every_vanilla_menu_schema_lays_out_and_renders() {
    assert_eq!(VANILLA_MENU_COUNT, 25);
    for menu_id in 0..VANILLA_MENU_COUNT as u32 {
        let schema = menu_schema(menu_id).unwrap_or_else(|| panic!("missing menu {menu_id}"));
        assert_eq!(schema.menu_id, menu_id);
        let screen = state(menu_id);
        assert_eq!(
            screen.slot_count(),
            schema.slot_count as usize,
            "menu {menu_id}"
        );
        for (w, h) in [(800.0, 600.0), (1920.0, 1080.0)] {
            let layout = screen.layout(w, h);
            let panel = layout.panel;
            assert!(panel.w > 0 && panel.h > 0);
            for (i, a) in layout.slots.iter().enumerate() {
                let r = a.rect;
                assert!(
                    r.x > panel.x
                        && r.y > panel.y
                        && r.right() < panel.right()
                        && r.bottom() < panel.bottom(),
                    "menu {menu_id} slot {} rect {r:?} not strictly inside panel {panel:?}",
                    a.index
                );
                for b in layout.slots.iter().skip(i + 1) {
                    assert!(
                        !r.intersects(b.rect),
                        "menu {menu_id} slots {} and {} overlap",
                        a.index,
                        b.index
                    );
                }
            }
            let mut draw = UiDrawList::new();
            screen.render(w, h, &mut draw);
            assert!(!draw.is_empty(), "menu {menu_id} rendered empty at {w}x{h}");
            assert_draws_within_viewport(&draw, w as u32, h as u32);
            let rgba = rasterize_draw_list(w as u32, h as u32, &draw);
            assert!(!rgba.is_empty());
        }
    }
}

#[test]
fn slot_kinds_cover_output_and_player_inventory() {
    let furnace = state(14);
    assert_eq!(furnace.slot_kind(0), Some(SlotKind::Input));
    assert_eq!(furnace.slot_kind(1), Some(SlotKind::Fuel));
    assert_eq!(furnace.slot_kind(2), Some(SlotKind::Output));
    assert!(furnace.slot_kind(3).is_some());
    let shulker = state(20);
    assert!(shulker.is_player_slot(30));
    assert!(!shulker.is_player_slot(0));
}

#[test]
fn left_click_pickup_and_place_conserves_items() {
    let mut screen = state(0);
    screen.set_slot(10, stack(1, 5));
    let before = screen.total_count();
    let action = screen.click(10, 0).expect("left click");
    assert_eq!(action.window_id, 1);
    assert_eq!(action.state_id, 0);
    assert_eq!(action.slot, 10);
    assert_eq!(action.button, 0);
    assert_eq!(action.mode, 0);
    assert!(action.carried_item.as_ref().is_some_and(|s| s.count == 5));
    assert!(action
        .changed_slots
        .iter()
        .any(|c| c.slot == 10 && c.stack.is_empty()));
    assert_conserved(before, &screen);

    let before = screen.total_count();
    screen.click(20, 0).expect("place");
    assert!(screen.cursor.is_none());
    assert_eq!(screen.slot(20).unwrap().count, 5);
    assert_conserved(before, &screen);
}

#[test]
fn right_click_half_split_conserves_items() {
    let mut screen = state(0);
    screen.set_slot(10, stack(2, 5));
    let before = screen.total_count();
    screen.click(10, 1).expect("right pickup");
    assert_eq!(screen.cursor.as_ref().unwrap().count, 3);
    assert_eq!(screen.slot(10).unwrap().count, 2);
    assert_conserved(before, &screen);
}

#[test]
fn shift_click_quick_move_conserves_items() {
    let mut screen = state(0);
    screen.set_slot(0, stack(3, 10));
    let before = screen.total_count();
    let action = screen.click_mode(0, 0, 1).expect("quick move");
    assert_eq!(action.mode, 1);
    assert!(screen.slot(0).unwrap().is_empty());
    assert_eq!(screen.slot(9).unwrap().count, 10);
    assert_conserved(before, &screen);
}

#[test]
fn hotbar_swap_conserves_items() {
    let mut screen = state(0);
    screen.set_slot(10, stack(4, 3));
    let hotbar = 9 + 27 + 2;
    screen.set_slot(hotbar, stack(5, 4));
    let before = screen.total_count();
    let action = screen.click_mode(10, 2, 2).expect("hotbar swap");
    assert_eq!(action.mode, 2);
    assert_eq!(screen.slot(10).unwrap().item_id, 5);
    assert_eq!(screen.slot(hotbar).unwrap().item_id, 4);
    assert_conserved(before, &screen);
}

#[test]
fn drag_split_conserves_items() {
    let mut screen = state(0);
    screen.set_cursor(Some(stack(6, 8)));
    let before = screen.total_count();
    screen.click_mode(20, 0, 5).expect("drag begin");
    screen.click_mode(21, 0, 5).expect("drag add");
    let end = screen.click_mode(-999, 0, 5).expect("drag end");
    assert_eq!(end.mode, 5);
    assert_eq!(screen.slot(20).unwrap().count, 4);
    assert_eq!(screen.slot(21).unwrap().count, 4);
    assert!(screen.cursor.is_none());
    assert_conserved(before, &screen);
}

#[test]
fn double_click_collects_to_cursor() {
    let mut screen = state(0);
    screen.set_cursor(Some(stack(7, 2)));
    screen.set_slot(10, stack(7, 3));
    screen.set_slot(20, stack(7, 4));
    screen.set_slot(30, stack(8, 6));
    let before = screen.total_count();
    let action = screen.click_mode(30, 0, 6).expect("collect");
    assert_eq!(action.mode, 6);
    assert_eq!(screen.cursor.as_ref().unwrap().count, 9);
    assert!(screen.slot(10).unwrap().is_empty());
    assert!(screen.slot(20).unwrap().is_empty());
    assert_eq!(screen.slot(30).unwrap().count, 6);
    assert_conserved(before, &screen);
}

#[test]
fn output_slot_rejects_placement_but_allows_pickup() {
    let mut screen = state(14);
    screen.set_cursor(Some(stack(9, 3)));
    let before = screen.total_count();
    let action = screen.click(2, 0).expect("output click");
    assert!(action.changed_slots.is_empty());
    assert!(screen.slot(2).unwrap().is_empty());
    assert_eq!(screen.cursor.as_ref().unwrap().count, 3);
    assert_conserved(before, &screen);

    screen.set_slot(2, stack(10, 1));
    screen.set_cursor(None);
    screen.click(2, 0).expect("take output");
    assert!(screen.slot(2).unwrap().is_empty());
    assert_eq!(screen.cursor.as_ref().unwrap().item_id, 10);
}

#[test]
fn drop_modes_reduce_total_intentionally() {
    let mut screen = state(0);
    screen.set_slot(10, stack(11, 5));
    screen.click_mode(10, 0, 4).expect("drop one");
    assert_eq!(screen.slot(10).unwrap().count, 4);
    screen.click_mode(10, 1, 4).expect("drop stack");
    assert!(screen.slot(10).unwrap().is_empty());
}

#[test]
fn tooltip_formats_rarity_enchantments_and_lore_inside_viewport() {
    let mut screen = state(0);
    let item = stack(12, 1)
        .with_name("Ruby Sword")
        .with_rarity(UiColor::rgb(255, 85, 85))
        .with_enchantment("Sharpness", 5)
        .with_lore(["Forged in the deep".to_string()]);
    screen.set_slot(10, item);
    screen.hover(Some(10));
    let layout = screen.layout(800.0, 600.0);
    let tip = layout.tooltip.expect("tooltip rect");
    assert!(tip.x >= 0 && tip.y >= 0 && tip.right() <= 800 && tip.bottom() <= 600);
    for slot in &layout.slots {
        assert!(
            !tip.intersects(slot.rect),
            "tooltip overlaps slot {}",
            slot.index
        );
    }

    let mut draw = UiDrawList::new();
    screen.render(800.0, 600.0, &mut draw);
    let mut saw_name = false;
    let mut saw_ench = false;
    let mut saw_lore = false;
    for cmd in &draw.commands {
        if let UiCommand::Text { color, text, .. } = cmd {
            saw_name |= text == "Ruby Sword" && *color == UiColor::rgb(255, 85, 85);
            saw_ench |= text.contains("Sharpness");
            saw_lore |= text.contains("Forged in the deep");
        }
    }
    assert!(saw_name && saw_ench && saw_lore);
}

#[test]
fn tooltip_clamps_inside_small_viewport_without_slot_overlap() {
    let mut screen = state(0);
    screen.set_slot(
        10,
        stack(13, 1).with_name("Very Long Display Name For Tooltip Clamping"),
    );
    screen.set_cursor(Some(stack(14, 2).with_name("Cursor Stack Tooltip")));
    screen.hover(None);
    let layout = screen.layout(320.0, 240.0);
    let tip = layout.tooltip.expect("tooltip rect");
    assert!(tip.x >= 0 && tip.y >= 0 && tip.right() <= 320 && tip.bottom() <= 240);
    for slot in &layout.slots {
        assert!(!tip.intersects(slot.rect));
    }
}

#[test]
fn furnace_render_emits_progress_bars_for_properties() {
    let mut screen = state(14);
    screen.set_property(0, 100);
    screen.set_property(1, 200);
    screen.set_property(2, 30);
    screen.set_property(3, 200);
    let mut draw = UiDrawList::new();
    screen.render(800.0, 600.0, &mut draw);
    let bars = draw
        .commands
        .iter()
        .filter(|c| matches!(c, UiCommand::ProgressBar { .. }))
        .count();
    assert!(
        bars >= 2,
        "expected furnace flame and arrow bars, got {bars}"
    );
}

#[test]
fn button_click_emits_deterministic_container_button_click() {
    let mut enchant = state(13);
    assert_eq!(
        enchant.button_click(1),
        Some(megu_client_ui::container_screen::ContainerButtonClick {
            window_id: 1,
            button_id: 1
        })
    );
    assert_eq!(
        enchant.button_click(1),
        Some(megu_client_ui::container_screen::ContainerButtonClick {
            window_id: 1,
            button_id: 1
        })
    );
    assert!(enchant.button_click(9).is_none());

    let mut stonecutter = state(24);
    assert_eq!(
        stonecutter.button_click(7),
        Some(megu_client_ui::container_screen::ContainerButtonClick {
            window_id: 1,
            button_id: 7
        })
    );

    let mut lectern = state(17);
    assert_eq!(
        lectern.button_click(3),
        Some(megu_client_ui::container_screen::ContainerButtonClick {
            window_id: 1,
            button_id: 3
        })
    );
    assert_eq!(
        lectern.button_click(105),
        Some(megu_client_ui::container_screen::ContainerButtonClick {
            window_id: 1,
            button_id: 105
        })
    );
    assert!(lectern.button_click(4).is_none());
}

fn decode_gui_uv(item_id: &str) -> Option<([f32; 2], [f32; 2])> {
    let mut parts = item_id.strip_prefix("@gui_uv:")?.split(':');
    let mut next = || {
        Some(f32::from_bits(
            u32::from_str_radix(parts.next()?, 16).ok()?,
        ))
    };
    let u0 = next()?;
    let v0 = next()?;
    let u1 = next()?;
    let v1 = next()?;
    Some(([u0, v0], [u1, v1]))
}

#[test]
fn generic_chests_use_vanilla_two_blit_uv_crops() {
    use megu_client_ui::draw::{GuiSpriteAtlas, GuiSpriteEntry};

    let packed = GuiSpriteEntry {
        uv_min: [0.25, 0.5],
        uv_max: [0.75, 1.0],
        width: 176,
        height: 222,
        nine_slice_border: [0, 0, 0, 0],
    };
    let assert_rows = |menu_id: u32, rows: u32| {
        let atlas = GuiSpriteAtlas::from_packed_with_container_panels(
            2,
            1,
            vec![0; 8],
            Vec::new(),
            vec![(menu_id, packed)],
        );
        let screen = state(menu_id);
        let mut draw = UiDrawList::new();
        screen.render_with_atlas(800.0, 600.0, None, &atlas, &mut draw);
        let icons: Vec<_> = draw
            .commands
            .iter()
            .filter_map(|cmd| match cmd {
                UiCommand::ItemIcon { rect, item_id } if item_id.starts_with("@gui_uv:") => {
                    Some((*rect, decode_gui_uv(item_id).expect("encoded gui uv")))
                }
                _ => None,
            })
            .collect();
        assert_eq!(icons.len(), 2, "menu {menu_id} must use two texture blits");
        let top_h = 17 + rows * 18;
        assert_eq!(icons[0].0.h, top_h);
        assert_eq!(icons[1].0.h, 96);
        assert_eq!(icons[1].0.y, icons[0].0.y + top_h as i32);

        let du = packed.uv_max[0] - packed.uv_min[0];
        let dv = packed.uv_max[1] - packed.uv_min[1];
        let expect_v = |src_y: u32| packed.uv_min[1] + dv * (src_y as f32 / 222.0);
        let eps = 1e-6;
        assert!((icons[0].1.0[0] - packed.uv_min[0]).abs() < eps);
        assert!((icons[0].1.0[1] - packed.uv_min[1]).abs() < eps);
        assert!((icons[0].1.1[0] - (packed.uv_min[0] + du)).abs() < eps);
        assert!((icons[0].1.1[1] - expect_v(top_h)).abs() < eps);
        assert!((icons[1].1.0[1] - expect_v(126)).abs() < eps);
        assert!((icons[1].1.1[1] - expect_v(222)).abs() < eps);
    };
    assert_rows(0, 1);
    assert_rows(2, 3);
    assert_rows(5, 6);
}

#[test]
fn generic_chest_two_blit_scales_source_crops_for_high_dpi_sheet() {
    use megu_client_ui::draw::{GuiSpriteAtlas, GuiSpriteEntry};

    let packed = GuiSpriteEntry {
        uv_min: [0.25, 0.5],
        uv_max: [0.75, 1.0],
        width: 352,
        height: 444,
        nine_slice_border: [0, 0, 0, 0],
    };
    let atlas = GuiSpriteAtlas::from_packed_with_container_panels(
        2,
        1,
        vec![0; 8],
        Vec::new(),
        vec![(0, packed)],
    );
    let screen = state(0);
    let mut draw = UiDrawList::new();
    screen.render_with_atlas(800.0, 600.0, None, &atlas, &mut draw);
    let icons: Vec<_> = draw
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::ItemIcon { rect, item_id } if item_id.starts_with("@gui_uv:") => {
                Some((*rect, decode_gui_uv(item_id).expect("encoded gui uv")))
            }
            _ => None,
        })
        .collect();
    assert_eq!(icons.len(), 2);
    assert_eq!(icons[0].0.h, 35, "destination crop stays logical 1× height");
    assert_eq!(icons[1].0.h, 96);

    let dv = packed.uv_max[1] - packed.uv_min[1];
    let expect_v = |src_y: u32| packed.uv_min[1] + dv * (src_y as f32 / 444.0);
    let eps = 1e-6;
    assert!((icons[0].1.1[1] - expect_v(70)).abs() < eps);
    assert!((icons[1].1.0[1] - expect_v(252)).abs() < eps);
    assert!((icons[1].1.1[1] - expect_v(444)).abs() < eps);
}
