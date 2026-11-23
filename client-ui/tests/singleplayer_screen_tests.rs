// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! The Singleplayer world select & "Create New World" screen: the Title
//! `Singleplayer` button navigates here instead of launching a
//! hardcoded world, discovered `<gameDir>/saves/` rows launch verbatim,
//! and the create row emits a `PlayWorld` with the parsed seed.

use megu_client_ui::draw::UiCommand;
use megu_client_ui::state::{
    seed_from_text, Screen, UiAction, UiInputEvent, UiKey, UiState, WidgetId,
};

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

fn key(ui: &mut UiState, k: UiKey) -> Vec<UiAction> {
    ui.handle_input(UiInputEvent::KeyPressed(k))
}

fn type_str(ui: &mut UiState, s: &str) {
    for c in s.chars() {
        ui.handle_input(UiInputEvent::CharTyped(c));
    }
}

#[test]
fn title_singleplayer_navigates_to_world_select() {
    let mut ui = UiState::new();
    let acts = click(&mut ui, WidgetId::TitleSingleplayer, 1280, 720);
    assert_eq!(ui.screen, Screen::Singleplayer);
    assert_eq!(acts, vec![UiAction::OpenScreen(Screen::Singleplayer)]);
}

#[test]
fn world_row_click_plays_that_world() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.singleplayer.worlds = vec!["Alpha".into(), "Beta".into(), "Gamma".into()];
    ui.navigate(Screen::Singleplayer);

    assert_eq!(
        click(&mut ui, WidgetId::SingleplayerWorld(0), w, h),
        vec![UiAction::PlayWorld {
            name: "Alpha".to_string(),
            seed: None
        }]
    );
    assert_eq!(
        click(&mut ui, WidgetId::SingleplayerWorld(2), w, h),
        vec![UiAction::PlayWorld {
            name: "Gamma".to_string(),
            seed: None
        }]
    );
}

#[test]
fn create_and_play_uses_field_name_and_parsed_seed() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.navigate(Screen::Singleplayer);

    // Default: "New World", no seed.
    let acts = click(&mut ui, WidgetId::SingleplayerCreate, w, h);
    assert_eq!(
        acts,
        vec![UiAction::PlayWorld {
            name: "New World".to_string(),
            seed: None
        }]
    );

    // Edited name is trimmed; integer seed parses verbatim.
    ui.singleplayer.new_name = "  My World  ".to_string();
    ui.singleplayer.seed_text = " -42 ".to_string();
    let acts = click(&mut ui, WidgetId::SingleplayerCreate, w, h);
    assert_eq!(
        acts,
        vec![UiAction::PlayWorld {
            name: "My World".to_string(),
            seed: Some(-42)
        }]
    );
}

#[test]
fn fields_accept_typing_and_enter_advances_then_creates() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.navigate(Screen::Singleplayer);

    click(&mut ui, WidgetId::SingleplayerNameField, w, h);
    assert!(ui.world_name_field_active());
    assert!(!ui.world_seed_field_active());
    ui.singleplayer.new_name.clear();
    type_str(&mut ui, "Shaded Vale");
    assert_eq!(ui.singleplayer.new_name, "Shaded Vale");

    // Enter in the name field steps down to the seed field.
    assert_eq!(key(&mut ui, UiKey::Enter), Vec::<UiAction>::new());
    assert!(!ui.world_name_field_active());
    assert!(ui.world_seed_field_active());
    type_str(&mut ui, "123");
    assert_eq!(ui.singleplayer.seed_text, "123");

    // Enter in the seed field is "Create & Play".
    let acts = key(&mut ui, UiKey::Enter);
    assert_eq!(
        acts,
        vec![UiAction::PlayWorld {
            name: "Shaded Vale".to_string(),
            seed: Some(123)
        }]
    );

    // Backspace edits the focused field.
    click(&mut ui, WidgetId::SingleplayerSeedField, w, h);
    key(&mut ui, UiKey::Backspace);
    assert_eq!(ui.singleplayer.seed_text, "12");
}

#[test]
fn back_and_escape_leave_world_select() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.navigate(Screen::Singleplayer);
    assert_eq!(click(&mut ui, WidgetId::SingleplayerBack, w, h), vec![UiAction::Back]);
    assert_eq!(ui.screen, Screen::Title);

    ui.navigate(Screen::Singleplayer);
    assert_eq!(key(&mut ui, UiKey::Escape), vec![UiAction::Back]);
    assert_eq!(ui.screen, Screen::Title);
}

#[test]
fn pager_scrolls_through_long_world_lists() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.singleplayer.worlds = (0..40).map(|i| format!("World {i:02}")).collect();
    ui.navigate(Screen::Singleplayer);
    ui.render(w, h);
    assert!(ui
        .hit_regions
        .iter()
        .any(|r| r.id == WidgetId::SingleplayerListDown));
    let shown: Vec<usize> = ui
        .hit_regions
        .iter()
        .filter_map(|r| match r.id {
            WidgetId::SingleplayerWorld(i) => Some(i),
            _ => None,
        })
        .collect();
    let visible = shown.len();
    assert!(visible > 0 && visible < 40);

    click(&mut ui, WidgetId::SingleplayerListDown, w, h);
    assert_eq!(ui.singleplayer.scroll, 1);
    let shown: Vec<usize> = {
        ui.render(w, h);
        ui.hit_regions
            .iter()
            .filter_map(|r| match r.id {
                WidgetId::SingleplayerWorld(i) => Some(i),
                _ => None,
            })
            .collect()
    };
    assert_eq!(shown.first().copied(), Some(1));
    assert_eq!(shown.len(), visible);

    click(&mut ui, WidgetId::SingleplayerListUp, w, h);
    assert_eq!(ui.singleplayer.scroll, 0);
}

#[test]
fn seed_from_text_blank_integer_and_string_hash() {
    assert_eq!(seed_from_text(""), None);
    assert_eq!(seed_from_text("   "), None);
    assert_eq!(seed_from_text("0"), Some(0));
    assert_eq!(seed_from_text("-7"), Some(-7));
    assert_eq!(seed_from_text("9223372036854775807"), Some(i64::MAX));
    // Java "a".hashCode() == 97, "abc".hashCode() == 96354 — the vanilla
    // string-seed rule for non-numeric input.
    assert_eq!(seed_from_text("a"), Some(97));
    assert_eq!(seed_from_text("abc"), Some(96354));
    assert_eq!(seed_from_text(" -5 "), Some(-5));
}

#[test]
fn title_status_renders_on_title_and_world_select() {
    let mut ui = UiState::new();
    ui.title_status = Some("Singleplayer \"Old World\": server exited".to_string());
    for screen in [Screen::Title, Screen::Singleplayer] {
        ui.screen = screen;
        let draw = ui.render(1280, 720);
        assert!(
            draw.commands.iter().any(|cmd| matches!(
                cmd,
                UiCommand::Text { text, .. } if text.contains("server exited")
            )),
            "{screen:?}: title_status did not render"
        );
    }
}

#[test]
fn empty_world_list_shows_hint_and_no_rows() {
    let mut ui = UiState::new();
    ui.navigate(Screen::Singleplayer);
    let draw = ui.render(1280, 720);
    assert!(!ui
        .hit_regions
        .iter()
        .any(|r| matches!(r.id, WidgetId::SingleplayerWorld(_))));
    assert!(draw.commands.iter().any(|cmd| matches!(
        cmd,
        UiCommand::Text { text, .. } if text.contains("No saved worlds")
    )));
}
