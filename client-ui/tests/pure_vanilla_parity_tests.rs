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

// ---------------------------------------------------------------------------
// Wave 6B: golden Vanilla 1.21.1 geometry (JAR-PARITY-14)
// ---------------------------------------------------------------------------
//
// Every number below is asserted against
// [`megu_client_ui::vanilla_metrics`], the single geometry authority the
// HUD, menu and screen builders share. A regression in any of them means
// a screen drifted away from the shipped 1.21.1 layout.

use megu_client_ui::creative_screen::CreativeInventoryState;
use megu_client_ui::death_screen::{
    DeathEnableDelay, DeathScreenState, DEATH_ENABLE_DELAY_TICKS,
};
use megu_client_ui::draw::{palette, text_height, UiColor, UiCommand, UiDrawList, UiRect};
use megu_client_ui::vanilla_metrics as vm;

/// The vanilla default ship size at GUI scale 1: 640×360 logical pixels.
const LW: u32 = 640;
const LH: u32 = 360;

fn hud_state() -> UiState {
    let mut ui = UiState::new();
    ui.ui_scale = 1;
    ui.screen = Screen::InGameHud;
    ui.hud.health = 20.0;
    ui.hud.max_health = 20.0;
    ui.hud.hunger = 20;
    ui.hud.air = 120;
    ui.hud.absorption = 0.0;
    ui
}

fn fills_of(list: &UiDrawList, w: u32, h: u32) -> Vec<UiRect> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::FillRect { rect, .. } if rect.w == w && rect.h == h => Some(*rect),
            _ => None,
        })
        .collect()
}

/// Fill and progress-bar rectangles of one size — the procedural bars use
/// `ProgressBar` while the flat sprites/backings use `FillRect`.
fn bars_of(list: &UiDrawList, w: u32, h: u32) -> Vec<UiRect> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::FillRect { rect, .. } | UiCommand::ProgressBar { rect, .. }
                if rect.w == w && rect.h == h =>
            {
                Some(*rect)
            }
            _ => None,
        })
        .collect()
}

fn texts_at(list: &UiDrawList, y: i32) -> Vec<(String, i32, UiColor, u32)> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::Text {
                x,
                y: ty,
                scale,
                color,
                text,
            } if *ty == y => Some((text.clone(), *x, *color, *scale)),
            _ => None,
        })
        .collect()
}

#[test]
fn vanilla_metrics_pin_the_1211_constants() {
    assert_eq!(vm::HOTBAR_WIDTH, 182);
    assert_eq!(vm::HOTBAR_HEIGHT, 22);
    assert_eq!(vm::HOTBAR_SLOT_PITCH, 20);
    assert_eq!(vm::HOTBAR_Y_OFFSET, 22);
    assert_eq!(vm::HOTBAR_SELECT_W, 24);
    assert_eq!(vm::HOTBAR_SELECT_H, 23);
    assert_eq!(vm::OFFHAND_W, 29);
    assert_eq!(vm::OFFHAND_H, 24);
    assert_eq!(vm::VITALS_HALF_SPAN, 91);
    assert_eq!(vm::VITALS_BOTTOM_ROW_Y_OFFSET, 39);
    assert_eq!(vm::VITALS_UPPER_ROW_Y_OFFSET, 49);
    assert_eq!(vm::XP_BAR_WIDTH, 182);
    assert_eq!(vm::XP_BAR_HEIGHT, 5);
    assert_eq!(vm::XP_BAR_Y_OFFSET, 29);
    assert_eq!(vm::BOSS_BAR_WIDTH, 182);
    assert_eq!(vm::BOSS_BAR_HEIGHT, 5);
    assert_eq!(vm::BOSS_BAR_START_Y, 12);
    assert_eq!(vm::BOSS_BAR_PITCH, 19);
    assert_eq!(vm::TITLE_ROW_STEP, 24);
    assert_eq!(vm::BUTTON_H, 20);
    assert_eq!(vm::PAUSE_ALIGN_Y, 0.25);
    assert_eq!(vm::DEATH_TITLE_Y, 30);
    assert_eq!(vm::DEATH_CAUSE_Y, 85);
    assert_eq!(vm::DEATH_SCORE_Y, 100);
    assert_eq!(vm::DEATH_BTN_W, 200);
    assert_eq!(vm::CREATIVE_IMAGE_W, 195);
    assert_eq!(vm::CREATIVE_IMAGE_H, 136);
    assert_eq!(vm::CREATIVE_GRID_X, 9);
    assert_eq!(vm::CREATIVE_GRID_Y, 18);
    assert_eq!(vm::CREATIVE_HOTBAR_X, 9);
    assert_eq!(vm::CREATIVE_HOTBAR_Y, 112);
    assert_eq!(vm::FONT_LINE_HEIGHT, 9);
    assert_eq!(DEATH_ENABLE_DELAY_TICKS, 20);
    // `h / 4 + 48` / `h / 4 + 72` / `h / 4 + 96`.
    assert_eq!(vm::title_top(LH as i32), 138);
    assert_eq!(vm::death_btn1_y(LH as i32), 162);
    assert_eq!(vm::death_btn2_y(LH as i32), 186);
}

#[test]
fn hud_hotbar_is_182_by_22_at_the_vanilla_pitch() {
    let mut ui = hud_state();
    for (i, slot) in ui.hud.hotbar.iter_mut().enumerate() {
        *slot = Some(format!("S{i}"));
    }
    ui.hud.offhand = Some("Shield".into());
    let list = ui.render(LW, LH);

    // Nine 20×20 cells on the `h - 22` row, starting at `w/2 - 91`.
    let mut cells: Vec<i32> = fills_of(&list, 20, 20)
        .into_iter()
        .filter(|r| r.y == 338)
        .map(|r| r.x)
        .collect();
    cells.sort_unstable();
    assert_eq!(cells.len(), 9, "expected nine vanilla hotbar cells");
    assert_eq!(cells[0], 229, "strip must start at w/2 - 91");
    for pair in cells.windows(2) {
        assert_eq!(pair[1] - pair[0], 20, "vanilla 20px cell pitch");
    }
    // Nine 20px cells span 180 of the 182px strip, which therefore ends
    // exactly at `w/2 + 91`.
    assert_eq!(cells[8] + 20, 409);
    assert_eq!(229 + vm::HOTBAR_WIDTH, 411);

    // Offhand frame: 29 wide, flush with the strip, on the `h - 23` row.
    let offhand = fills_of(&list, 29, 23);
    assert_eq!(
        offhand,
        vec![UiRect::new(200, 337, 29, 23)],
        "offhand frame must sit at w/2 - 120 with its clipped 23px height"
    );
    assert_eq!(vm::offhand_left_x(LW as i32), 200);

    // The selection highlight is the 24×23 frame one pixel of overhang
    // around its cell; the procedural path marks the selected cell border.
    assert_eq!(vm::hotbar_select_x(LW as i32, 0), 228);
    assert_eq!(vm::hotbar_select_y(LH as i32), 337);
    assert_eq!((vm::HOTBAR_SELECT_W, vm::HOTBAR_SELECT_H), (24, 23));
}

#[test]
fn hud_vitals_straddle_the_vanilla_half_span() {
    let mut ui = hud_state();
    let list = ui.render(LW, LH);

    // Hearts on the `h - 39` row, left-anchored at `w/2 - 91`.
    let hearts = fills_of(&list, 8, 8);
    assert!(
        hearts.contains(&UiRect::new(229, 321, 8, 8)),
        "first heart cell must be at (w/2 - 91, h - 39): {hearts:?}"
    );
    // Hunger on the same row, right-anchored at `w/2 + 91`.
    assert!(
        hearts
            .iter()
            .any(|r| r.y == 321 && r.x + r.w as i32 == 411),
        "hunger must end at w/2 + 91: {hearts:?}"
    );
    // Armor and air share the row one line above.
    assert!(
        hearts.iter().any(|r| r.y == 311 && r.x == 229),
        "armor must be at (w/2 - 91, h - 49): {hearts:?}"
    );
    assert!(
        hearts
            .iter()
            .any(|r| r.y == 311 && r.x + r.w as i32 == 411),
        "air must end at w/2 + 91 on the h - 49 row: {hearts:?}"
    );
}

#[test]
fn hud_pass_paints_the_182_by_5_xp_bar_and_green_level() {
    let mut ui = hud_state();
    ui.hud.xp_progress = 0.5;
    ui.hud.xp_level = 42;
    let list = ui.render(LW, LH);

    // Bar frame: 182×5 at (w/2 - 91, h - 29).
    assert!(
        fills_of(&list, 182, 5)
            .iter()
            .any(|r| *r == UiRect::new(229, 331, 182, 5)),
        "experience bar frame missing at (w/2 - 91, h - 29)"
    );
    // Fill crops against the 183-pixel source span: 0.5 * 183 = 91.
    let fill = fills_of(&list, 91, 5);
    assert_eq!(
        fill,
        vec![UiRect::new(229, 331, 91, 5)],
        "XP fill must be (progress * 183) px inside the 182 px frame"
    );
    assert_eq!(vm::xp_bar_fill(0.5), 91);
    // Green level text with its dark drop shadow, on the hearts row.
    let level = texts_at(&list, 321);
    let green = level
        .iter()
        .find(|(text, ..)| text == "42")
        .unwrap_or_else(|| panic!("level text missing: {level:?}"));
    assert_eq!(green.2, UiColor::rgb(0x80, 0xff, 0x20));
    assert_eq!(green.1 + text_width_px("42", green.3) / 2, 320);
    let shadow = texts_at(&list, 322);
    assert!(
        shadow
            .iter()
            .any(|(text, x, color, _)| text == "42" && *color != green.2 && *x == green.1 + 1),
        "level text needs a 1px drop shadow: {shadow:?}"
    );

    // Creative and spectator carry no experience: the bar and its level
    // number disappear entirely.
    for mode in [1, 3] {
        let mut gated = hud_state();
        gated.hud.game_mode = mode;
        gated.hud.xp_progress = 0.5;
        gated.hud.xp_level = 42;
        let gated_list = gated.render(LW, LH);
        assert!(
            !fills_of(&gated_list, 182, 5)
                .iter()
                .any(|r| r.y == 331),
            "game mode {mode} must not draw the experience bar"
        );
        assert!(
            texts_at(&gated_list, 321).is_empty(),
            "game mode {mode} must not draw the experience level"
        );
    }
}

fn text_width_px(text: &str, scale: u32) -> i32 {
    megu_client_ui::draw::text_width(text, scale) as i32
}

#[test]
fn hud_bossbars_start_at_12_on_a_19px_pitch_without_a_cap() {
    let mut ui = hud_state();
    ui.hud.bossbars = (0..5)
        .map(|i| (format!("Boss {i}"), 0.5))
        .collect();
    let list = ui.render(LW, LH);

    let bars = bars_of(&list, 182, 5);
    for index in 0..5 {
        let y = 12 + index * 19;
        assert!(
            bars.iter().any(|r| r.y == y && r.x == 229),
            "bossbar {index} must be 182×5 at (w/2 - 91, {y}): {bars:?}"
        );
    }
    // Five bars all fit above `h / 3 = 120`, so the old three-bar cap is
    // gone; a bar whose top edge reaches the cutoff is dropped instead.
    assert_eq!(vm::visible_boss_bars(5, LH as i32), 5);
    assert_eq!(vm::boss_bar_y(4), 88);
    assert_eq!(vm::visible_boss_bars(9, LH as i32), 6);
    assert_eq!(vm::boss_bar_y(6), 126);
}

#[test]
fn title_column_is_anchored_at_h_quarter_plus_48() {
    let mut ui = UiState::new();
    ui.ui_scale = 1;
    ui.screen = Screen::Title;
    let list = ui.render(LW, LH);

    let singleplayer = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::TitleSingleplayer)
        .expect("title button")
        .rect;
    assert_eq!(singleplayer, UiRect::new(220, 138, 200, 20));
    // The version footer hugs the bottom-left corner in white.
    let footer = list
        .commands
        .iter()
        .find_map(|cmd| match cmd {
            UiCommand::Text { x, y, color, text, .. } if text == "Megu 1.21.1" => {
                Some((*x, *y, *color))
            }
            _ => None,
        })
        .expect("version footer");
    assert_eq!(footer, (2, 350, palette::TEXT));
}

#[test]
fn pause_grid_uses_the_vanilla_quarter_vertical_alignment() {
    let mut ui = UiState::new();
    ui.ui_scale = 1;
    ui.screen = Screen::Pause;
    ui.render(LW, LH);

    // 4 rows of 20 px with 4 px gaps = 92; (360 - 92) * 0.25 = 67.
    let back = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::PauseBackToGame)
        .expect("pause back")
        .rect;
    assert_eq!(back.y, 67);
    assert_eq!(back, UiRect::new(218, 67, 204, 20));
    let quit = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::PauseSaveQuit)
        .expect("pause quit")
        .rect;
    assert_eq!(quit.y, 67 + 3 * 24, "bottom row one 24px step lower");
}

#[test]
fn death_screen_matches_the_vanilla_rows_and_locks_for_20_ticks() {
    let mut ui = UiState::new();
    ui.ui_scale = 1;
    ui.navigate(Screen::InGameHud);
    ui.show_death(DeathScreenState {
        cause_message: Some("Tester was slain by Creeper".into()),
        score: 340,
        hardcore: false,
        respawn_cooldown_ticks: 0,
    });
    let list = ui.render(LW, LH);

    // Title at y = 30 (2× white), cause at 85, score at 100.
    let title = texts_at(&list, 30);
    let title = title
        .iter()
        .find(|(text, ..)| text == "You died!")
        .unwrap_or_else(|| panic!("death title missing: {title:?}"));
    assert_eq!(title.2, palette::TEXT);
    assert_eq!(title.3, 2);
    assert!(texts_at(&list, 85)
        .iter()
        .any(|(text, ..)| text == "Tester was slain by Creeper"));
    assert!(texts_at(&list, 100)
        .iter()
        .any(|(text, ..)| text == "Score: 340"));

    // Anti-misclick lock: no respawn button until 20 ticks have passed,
    // while the Title Screen escape is always live.
    assert!(!ui.death_buttons_enabled());
    assert!(
        !ui.hit_regions.iter().any(|r| r.id == WidgetId::DeathRespawn),
        "respawn must stay locked for the first 20 ticks"
    );
    let title_screen = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::DeathTitleScreen)
        .expect("title screen button")
        .rect;
    assert_eq!(title_screen, UiRect::new(220, 186, 200, 20));

    for _ in 0..DEATH_ENABLE_DELAY_TICKS {
        ui.tick_death();
    }
    ui.render(LW, LH);
    let respawn = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::DeathRespawn)
        .expect("respawn button after the delay")
        .rect;
    assert_eq!(respawn, UiRect::new(220, 162, 200, 20));

    // The delay is a local countdown in ticks, independent of the
    // server-provided respawn cooldown.
    let mut delay = DeathEnableDelay::new();
    assert_eq!(delay.remaining(), 20);
    delay.tick();
    assert_eq!(delay.remaining(), 19);
    assert_eq!(delay.ticks(), 1);
    assert!(!delay.enabled());
}

#[test]
fn death_screen_hardcore_offers_spectate_world_in_the_primary_slot() {
    let mut ui = UiState::new();
    ui.ui_scale = 1;
    ui.show_death(DeathScreenState {
        hardcore: true,
        ..DeathScreenState::default()
    });
    let list = ui.render(LW, LH);
    assert!(
        list.commands.iter().any(|cmd| matches!(
            cmd,
            UiCommand::Text { text, y, .. } if text == "Spectate World" && *y == 162 + 6
        )),
        "hardcore primary button must read Spectate World"
    );
    assert!(
        texts_at(&list, 30)
            .iter()
            .any(|(text, ..)| text == "Game over!"),
        "hardcore title must be Game over!"
    );
    // No spectate implementation exists, so the slot stays disabled and
    // registers no hit region.
    assert!(
        !ui.hit_regions
            .iter()
            .any(|r| r.id == WidgetId::DeathRespawn),
        "a disabled Spectate World slot must not be clickable"
    );
    for _ in 0..DEATH_ENABLE_DELAY_TICKS {
        ui.tick_death();
    }
    ui.render(LW, LH);
    assert!(
        !ui.hit_regions
            .iter()
            .any(|r| r.id == WidgetId::DeathRespawn),
        "hardcore never offers a respawn"
    );
}

#[test]
fn creative_panel_is_195_by_136_with_the_vanilla_grid_and_hotbar() {
    let creative = CreativeInventoryState::default();
    let panel = creative.panel_rect(LW, LH);
    assert_eq!(panel, UiRect::new(222, 112, 195, 136));
    // Item grid at (9, 18), 18 px cells.
    assert_eq!(
        creative.catalog_slot_rect(0, LW, LH),
        Some(UiRect::new(231, 130, 18, 18))
    );
    assert_eq!(
        creative.catalog_slot_rect(44, LW, LH),
        Some(UiRect::new(375, 202, 18, 18))
    );
    // Player hotbar row at (9, 112).
    assert_eq!(
        creative.hotbar_slot_rect(0, LW, LH),
        Some(UiRect::new(231, 224, 18, 18))
    );
    assert_eq!(
        creative.hotbar_slot_rect(8, LW, LH),
        Some(UiRect::new(375, 224, 18, 18))
    );
    // The destroy well sits right of the hotbar row, inside the panel.
    let trash = creative.trash_slot_rect(LW, LH);
    assert_eq!(trash, UiRect::new(395, 224, 18, 18));
    assert!(!trash.intersects(UiRect::new(375, 224, 18, 18)));
    assert!(trash.right() <= panel.right());
    // Every interactive rect stays inside the panel it is drawn in.
    for tab in megu_client_ui::creative_screen::CreativeTab::ALL {
        let r = creative.tab_rect(tab, LW, LH).expect("tab rect");
        assert!(r.y + r.h as i32 <= panel.y, "tab strips hang above the panel");
    }
    let search = creative.search_box_rect(LW, LH);
    assert!(search.y >= panel.y && search.bottom() <= panel.y + 18);
    assert!(search.x >= panel.x && search.right() <= panel.right());
}

#[test]
fn font_line_advance_is_nine_pixels_for_stacked_lines() {
    assert_eq!(text_height("A", 1), 8, "single line stays one glyph tall");
    assert_eq!(text_height("A\nB", 1), 8 + 9);
    assert_eq!(text_height("A\nB\nC", 1), 8 + 2 * 9);
    assert_eq!(text_height("A\nB", 2), (8 + 9) * 2);
    assert_eq!(text_height("", 1), 8);
}
