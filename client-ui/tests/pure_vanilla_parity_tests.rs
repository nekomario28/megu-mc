// SPDX-License-Identifier: LGPL-3.0-or-later
//! Golden tests for the pure-vanilla UI parity pass: one canonical GUI
//! scale formula, the Create-World "Allow Commands" toggle's per-mode
//! vanilla defaults, container click parity (Shift+RMB quick-move and
//! double-click gather precedence), and the `gui_bridge` input helpers.

use megu_client_ui::container_screen::{menu_schema, ContainerScreenState, UiItemStack};
use megu_client_ui::gui_bridge::{GuiBridgeScreenEvent, GuiBridgeSlotAction};
use megu_client_ui::state::{
    Screen, SingleplayerGameMode, UiAction, UiInputEvent, UiState, WidgetId,
};
use megu_core::ui_contract::compute_gui_scale;

/// Render then click the center of `id`'s hit region.
fn click(ui: &mut UiState, id: WidgetId, w: u32, h: u32) -> Vec<UiAction> {
    ui.render(w, h);
    let r = ui
        .hit_regions
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no hit region for {id:?} on {:?}", ui.screen))
        .rect;
    ui.handle_input(UiInputEvent::Click {
        x: r.x + r.w as i32 / 2,
        y: r.y + r.h as i32 / 2,
    })
}

fn state(menu_id: u32) -> ContainerScreenState {
    ContainerScreenState::new(menu_schema(menu_id).unwrap(), 1, 0, "test menu")
}

fn stack(item_id: u32, count: u8) -> UiItemStack {
    UiItemStack::new(item_id, count).with_name(format!("Item {item_id}"))
}

#[test]
fn gui_scale_uses_the_one_canonical_formula() {
    let mut ui = UiState::new();
    for &(w, h) in &[
        (640u32, 360u32),
        (854, 480),
        (1280, 720),
        (1920, 1080),
        (3840, 2160),
    ] {
        ui.render(w, h);
        assert_eq!(ui.effective_scale(h), compute_gui_scale(w, h, 0), "{w}x{h}");
    }
    // Canonical spot checks: vanilla resolves scale 3 at 720p and 1 at
    // 640x360 — the old height/270 formula answered 2 at 720p.
    ui.render(1280, 720);
    assert_eq!(ui.effective_scale(720), 3);
    ui.render(640, 360);
    assert_eq!(ui.effective_scale(360), 1);
    // A fixed request still yields to the canonical 320x240 minimum.
    ui.ui_scale = 4;
    ui.render(640, 360);
    assert_eq!(ui.effective_scale(360), compute_gui_scale(640, 360, 4));
    ui.ui_scale = 0;
}

#[test]
fn create_world_allow_commands_follows_vanilla_mode_defaults() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    // Pin the scale the full create form fits without scrolling.
    ui.ui_scale = 2;
    ui.navigate(Screen::Singleplayer);

    // Survival default: cheats OFF.
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Survival);
    assert!(!ui.singleplayer.allow_commands);
    assert!(!ui.create_world_payload().allow_commands);

    // Cycle Survival -> Creative: the toggle re-syncs to the mode's
    // vanilla default (ON).
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Creative);
    assert!(ui.singleplayer.allow_commands);
    assert!(ui.create_world_payload().allow_commands);

    // Cycle Creative -> Hardcore: locked OFF, and the layout withholds
    // the toggle's hit region entirely.
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Hardcore);
    assert!(!ui.singleplayer.allow_commands);
    assert!(!ui.create_world_payload().allow_commands);
    ui.render(w, h);
    assert!(
        !ui.hit_regions
            .iter()
            .any(|r| r.id == WidgetId::SingleplayerAllowCommands),
        "Hardcore must not expose an Allow Commands hit region"
    );

    // Back on Survival the user can opt in; the explicit toggle marks
    // `allow_commands_overridden` so later mode cycling never re-syncs.
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Survival);
    click(&mut ui, WidgetId::SingleplayerAllowCommands, w, h);
    assert!(ui.singleplayer.allow_commands);
    assert!(ui.singleplayer.allow_commands_overridden);
    assert!(ui.create_world_payload().allow_commands);

    // Creative -> Hardcore -> Survival -> Creative leaves the user's
    // choice alone: Hardcore still forces it off, but returning to
    // Creative keeps the overridden value instead of restoring ON.
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert!(ui.singleplayer.allow_commands);
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Hardcore);
    assert!(!ui.singleplayer.allow_commands);
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Survival);
    assert!(!ui.singleplayer.allow_commands);
    click(&mut ui, WidgetId::SingleplayerGameMode, w, h);
    assert_eq!(ui.singleplayer.game_mode, SingleplayerGameMode::Creative);
    assert!(
        !ui.singleplayer.allow_commands,
        "an overridden Allow Commands must not re-sync to Creative's ON"
    );
    assert!(!ui.create_world_payload().allow_commands);
}

#[test]
fn shift_right_click_quick_moves_like_vanilla() {
    let mut screen = state(0);
    screen.set_slot(0, stack(3, 10));
    let before = screen.total_count();
    // Vanilla maps BOTH Shift+LMB and Shift+RMB to mode 1 (QUICK_MOVE);
    // the button id is still reported for the wire payload.
    let action = screen
        .click_with_shift(0, 1, true)
        .expect("shift+right-click quick move");
    assert_eq!(action.mode, 1);
    assert_eq!(action.button, 1);
    assert!(screen.slot(0).unwrap().is_empty());
    assert_eq!(screen.slot(9).unwrap().count, 10);
    assert_eq!(before, screen.total_count());
}

#[test]
fn second_left_click_gathers_matching_stacks_to_cursor() {
    let mut screen = state(0);
    let held = stack(3, 60)
        .with_lore(["shared lore"])
        .with_enchantment("unbreaking", 3);
    screen.set_cursor(Some(held.clone()));
    screen.set_slot(0, held.clone());
    screen.set_slot(1, stack(3, 5).with_lore(["shared lore"]).with_enchantment("unbreaking", 3));
    screen.set_slot(2, stack(7, 12));

    // First press merges four items into the full slot; the second —
    // inside vanilla's 300 ms window on the same slot+button — must
    // resolve to PICKUP_ALL (mode 6), not the nonempty-cursor drag arm.
    let first = screen.click_mode(0, 0, 0).expect("first click");
    assert_eq!(first.mode, 0);
    let gathered = screen.click_mode(0, 0, 0).expect("double-click gather");
    assert_eq!(gathered.mode, 6);

    let cursor = screen.cursor.as_ref().expect("cursor still holds items");
    assert_eq!(cursor.count, 64);
    // Component-bearing metadata survives the predicted moves verbatim.
    assert_eq!(cursor.display_name, "Item 3");
    assert_eq!(cursor.lore, vec!["shared lore".to_string()]);
    assert_eq!(cursor.enchantments.len(), 1);
    assert_eq!(screen.slot(0).unwrap().count, 56);
    assert_eq!(screen.slot(0).unwrap().display_name, "Item 3");
    // Non-matching stacks are never touched.
    assert_eq!(screen.slot(1).unwrap().count, 5);
    assert_eq!(screen.slot(2).unwrap().item_id, 7);
}

#[test]
fn gui_bridge_screen_event_helpers_build_vanilla_events() {
    assert_eq!(
        GuiBridgeScreenEvent::left_click(10, 20),
        GuiBridgeScreenEvent::MouseClick {
            x: 10,
            y: 20,
            button: 0
        }
    );
    assert_eq!(
        GuiBridgeScreenEvent::right_click(10, 20),
        GuiBridgeScreenEvent::MouseClick {
            x: 10,
            y: 20,
            button: 1
        }
    );
    assert_eq!(
        GuiBridgeScreenEvent::mouse_scroll(10, 20, -3),
        GuiBridgeScreenEvent::MouseScroll {
            x: 10,
            y: 20,
            delta: -3
        }
    );
    assert_eq!(
        GuiBridgeScreenEvent::key(65),
        GuiBridgeScreenEvent::KeyPress {
            key: 65,
            shift: false,
            ctrl: false,
            alt: false
        }
    );
    assert_eq!(
        GuiBridgeScreenEvent::left_click(7, 9).position(),
        Some((7, 9))
    );
    assert_eq!(GuiBridgeScreenEvent::key(65).position(), None);
}

#[test]
fn gui_bridge_slot_action_helpers_resolve_vanilla_wire_pairs() {
    // Shift held over either mouse button quick-moves; released it is a
    // plain pickup/place with the real button.
    assert_eq!(
        GuiBridgeSlotAction::from_slot_click(5, 0, false),
        GuiBridgeSlotAction::ClickSlot {
            slot: 5,
            button: 0
        }
    );
    assert_eq!(
        GuiBridgeSlotAction::from_slot_click(5, 1, false),
        GuiBridgeSlotAction::ClickSlot {
            slot: 5,
            button: 1
        }
    );
    for button in [0u8, 1, 2] {
        assert_eq!(
            GuiBridgeSlotAction::from_slot_click(5, button, true),
            GuiBridgeSlotAction::QuickMove { slot: 5 },
            "shift + button {button} must quick-move"
        );
    }
    assert_eq!(GuiBridgeSlotAction::left_click(9).click_wire(), (0, 0));
    assert_eq!(GuiBridgeSlotAction::right_click(9).click_wire(), (1, 0));
    assert_eq!(
        GuiBridgeSlotAction::QuickMove { slot: 9 }.click_wire(),
        (0, 1)
    );
    assert_eq!(
        GuiBridgeSlotAction::SwapHotbar {
            slot: 9,
            hotbar: 4
        }
        .click_wire(),
        (4, 2)
    );
    assert_eq!(GuiBridgeSlotAction::right_click(9).slot(), 9);
    assert_eq!(GuiBridgeSlotAction::QuickMove { slot: 9 }.slot(), 9);
}
