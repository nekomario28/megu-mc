// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Tests for the `hud.scoreboard`, `hud.title`, and `hud.jade_looking_at`
//! manifest features: the state models in `state.rs`/`magic_hud.rs` and
//! their draw-list output through the real `UiState::render` HUD path.

use megu_client_ui::draw::{UiCommand, UiDrawList};
use megu_client_ui::hud_widgets::ScoreboardEntry;
use megu_client_ui::layout::{
    layout_looking_at, layout_scoreboard_sidebar, layout_title_overlay,
};
use megu_client_ui::magic_hud::LookingAtBlockState;
use megu_client_ui::state::{
    ScoreboardState, Screen, TitleOverlayState, UiState, SCOREBOARD_SIDEBAR_MAX_ROWS,
};

const VIEW: (u32, u32) = (1280, 720);

fn hud_ui() -> UiState {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui
}

fn texts(list: &UiDrawList) -> Vec<&str> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn has_text(list: &UiDrawList, needle: &str) -> bool {
    texts(list).iter().any(|t| t.contains(needle))
}

fn fill_count(list: &UiDrawList) -> usize {
    list.commands
        .iter()
        .filter(|cmd| matches!(cmd, UiCommand::FillRect { .. }))
        .count()
}

fn populated_scoreboard() -> ScoreboardState {
    ScoreboardState {
        objective_title: Some("Scores".to_string()),
        visible: true,
        entries: vec![
            ScoreboardEntry {
                label: "Steve".to_string(),
                score: 12,
            },
            ScoreboardEntry {
                label: "Alex".to_string(),
                score: 40,
            },
            ScoreboardEntry {
                label: "Megu".to_string(),
                score: 7,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// ScoreboardState model
// ---------------------------------------------------------------------------

#[test]
fn scoreboard_state_sorts_and_caps_at_vanilla_limit() {
    let mut sb = ScoreboardState {
        objective_title: Some("Race".to_string()),
        visible: true,
        entries: (0..20)
            .map(|i| ScoreboardEntry {
                label: format!("P{i:02}"),
                score: i,
            })
            .collect(),
    };
    let sorted = sb.sorted_entries();
    assert_eq!(sorted.len(), SCOREBOARD_SIDEBAR_MAX_ROWS);
    assert_eq!(sorted[0].label, "P19");
    assert_eq!(sorted[14].label, "P05");
    // Ties break by label ascending.
    sb.entries = vec![
        ScoreboardEntry {
            label: "b".to_string(),
            score: 5,
        },
        ScoreboardEntry {
            label: "a".to_string(),
            score: 5,
        },
        ScoreboardEntry {
            label: "c".to_string(),
            score: 9,
        },
    ];
    let sorted = sb.sorted_entries();
    assert_eq!(
        sorted.iter().map(|e| e.label.as_str()).collect::<Vec<_>>(),
        vec!["c", "a", "b"]
    );
}

#[test]
fn scoreboard_hidden_or_empty_emits_nothing() {
    let mut ui = hud_ui();
    let mut out = UiDrawList::new();

    // Default state: hidden flag off.
    assert!(!ui.hud.scoreboard.should_render());
    assert!(!layout_scoreboard_sidebar(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());

    // Visible but no content.
    ui.hud.scoreboard.visible = true;
    assert!(!layout_scoreboard_sidebar(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());

    // Content but hidden again.
    ui.hud.scoreboard = populated_scoreboard();
    ui.hud.scoreboard.visible = false;
    assert!(!layout_scoreboard_sidebar(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());
}

#[test]
fn scoreboard_populated_draws_panel_title_and_sorted_rows() {
    let mut ui = hud_ui();
    ui.hud.scoreboard = populated_scoreboard();
    let mut out = UiDrawList::new();
    assert!(layout_scoreboard_sidebar(&ui, VIEW.0, VIEW.1, &mut out));

    // Panel chrome + one row per entry + the objective title.
    assert!(fill_count(&out) >= 1);
    assert!(out
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::BorderRect { .. })));
    for label in ["Scores", "Steve", "Alex", "Megu"] {
        assert!(has_text(&out, label), "missing {label}");
    }
    // Scores render right-aligned as their own text commands.
    assert!(texts(&out).iter().any(|t| t.trim() == "40"));
    // Sorted score-descending: "Alex" (40) row precedes "Steve" (12).
    let pos = |needle: &str| {
        out.commands
            .iter()
            .position(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains(needle)))
            .unwrap()
    };
    assert!(pos("Alex") < pos("Steve"));
    assert!(pos("Steve") < pos("Megu"));
}

#[test]
fn scoreboard_over_cap_draws_only_top_15() {
    let mut ui = hud_ui();
    ui.hud.scoreboard = ScoreboardState {
        objective_title: None,
        visible: true,
        entries: (0..20)
            .map(|i| ScoreboardEntry {
                label: format!("P{i:02}"),
                score: i,
            })
            .collect(),
    };
    let mut out = UiDrawList::new();
    assert!(layout_scoreboard_sidebar(&ui, VIEW.0, VIEW.1, &mut out));
    // Highest scores are P19..P05; the five lowest (P00..P04) drop off.
    assert!(has_text(&out, "P19"));
    assert!(has_text(&out, "P05"));
    for dropped in ["P00", "P01", "P02", "P03", "P04"] {
        assert!(!has_text(&out, dropped), "{dropped} should be capped");
    }
    // Exactly 15 entry rows were emitted.
    let rows = texts(&out)
        .iter()
        .filter(|t| t.starts_with('P') && t.len() == 3)
        .count();
    assert_eq!(rows, SCOREBOARD_SIDEBAR_MAX_ROWS);
}

// ---------------------------------------------------------------------------
// TitleOverlayState model + fade progression
// ---------------------------------------------------------------------------

#[test]
fn title_overlay_tick_advances_vanilla_phases() {
    let mut title = TitleOverlayState::vanilla();
    assert_eq!(title.fade_in_ticks, 10);
    assert_eq!(title.stay_ticks, 70);
    assert_eq!(title.fade_out_ticks, 20);
    assert_eq!(title.total_ticks(), 100);
    assert!(!title.is_active());

    title.show_vanilla("Victory", "You won");
    assert!(title.is_active());
    let overlay = title.overlay().expect("active overlay");
    assert_eq!(overlay.title.as_ref().unwrap().alpha(), 0.0);

    // Fade-in: alpha ramps 0 -> 1 over the first 10 ticks.
    for _ in 0..5 {
        title.tick();
    }
    let t = title.overlay().unwrap().title.unwrap();
    assert!((t.alpha() - 0.5).abs() < 1e-6);

    // Stay: full alpha through the hold window.
    for _ in 0..70 {
        title.tick();
    }
    assert_eq!(title.age_ticks, 75);
    assert_eq!(title.overlay().unwrap().title.unwrap().alpha(), 1.0);

    // Fade-out: ramps back to 0 across the last 20 ticks.
    for _ in 0..20 {
        title.tick();
    }
    assert_eq!(title.age_ticks, 95);
    assert!(title.is_active());
    for _ in 0..5 {
        title.tick();
    }
    assert_eq!(title.age_ticks, 100);
    assert!(!title.is_active());
    assert!(title.overlay().is_none());
}

#[test]
fn title_overlay_expired_or_empty_emits_nothing() {
    let mut ui = hud_ui();
    let mut out = UiDrawList::new();
    assert!(!layout_title_overlay(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());

    ui.hud.title_overlay.show_vanilla("Gone", "");
    for _ in 0..100 {
        ui.hud.title_overlay.tick();
    }
    assert!(!layout_title_overlay(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());
}

#[test]
fn title_overlay_draws_centered_title_only_while_visible() {
    let mut ui = hud_ui();
    ui.hud
        .title_overlay
        .show_vanilla("Victory", "Subtitle here");
    let mut out = UiDrawList::new();

    // age 0: still inside fade-in but alpha == 0 -> nothing drawn.
    assert!(!layout_title_overlay(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(!has_text(&out, "Victory"));

    // Mid fade-in: title + subtitle appear, centered (scale-2 title).
    for _ in 0..5 {
        ui.hud.title_overlay.tick();
    }
    out.clear();
    assert!(layout_title_overlay(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(has_text(&out, "Victory"));
    assert!(has_text(&out, "Subtitle here"));
    // The big title is emitted at the largest text scale (scale-2
    // logical × the widget pack's GUI scale); the subtitle stays small.
    let (title_x, title_scale, title_alpha) = out
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::Text {
                x,
                scale,
                color,
                text,
                ..
            } if text.contains("Victory") => Some((*x, *scale, color.a)),
            _ => None,
        })
        .max_by_key(|(_, scale, _)| *scale)
        .expect("title text");
    assert!(title_scale >= 2, "title scale {title_scale}");
    // Centered horizontally around the viewport middle (shadow copies
    // share the same x; pick the max-scale emit).
    let w = megu_client_ui::draw::text_width("Victory", title_scale) as i32;
    assert!(
        (title_x - (VIEW.0 as i32 / 2 - w / 2)).abs() <= 4,
        "title x {title_x} w {w}"
    );
    // Mid-fade alpha: strictly between transparent and opaque.
    assert!(title_alpha > 0 && title_alpha < 255, "alpha {title_alpha}");

    // Fully expired: no title commands at all.
    for _ in 0..95 {
        ui.hud.title_overlay.tick();
    }
    out.clear();
    assert!(!layout_title_overlay(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(!has_text(&out, "Victory"));
}

// ---------------------------------------------------------------------------
// LookingAtBlockState
// ---------------------------------------------------------------------------

fn populated_looking_at() -> LookingAtBlockState {
    LookingAtBlockState {
        enabled: true,
        block_id: "minecraft:oak_log".to_string(),
        display_name: "Oak Log".to_string(),
        position: Some([12, 64, -30]),
        mod_name: Some("minecraft".to_string()),
        tool_hint: Some("Needs axe".to_string()),
        extra_lines: vec!["Axis: X".to_string()],
    }
}

#[test]
fn looking_at_disabled_or_empty_emits_nothing() {
    let mut ui = hud_ui();
    let mut out = UiDrawList::new();

    // Populated but disabled (the default).
    ui.hud.looking_at = populated_looking_at();
    ui.hud.looking_at.enabled = false;
    assert!(!ui.hud.looking_at.is_visible());
    assert!(!layout_looking_at(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());

    // Enabled but no target.
    ui.hud.looking_at = LookingAtBlockState {
        enabled: true,
        ..Default::default()
    };
    assert!(!layout_looking_at(&ui, VIEW.0, VIEW.1, &mut out));
    assert!(out.is_empty());
}

#[test]
fn looking_at_populated_draws_panel_with_block_name() {
    let mut ui = hud_ui();
    ui.hud.looking_at = populated_looking_at();
    let mut out = UiDrawList::new();
    assert!(layout_looking_at(&ui, VIEW.0, VIEW.1, &mut out));

    assert!(fill_count(&out) >= 1);
    assert!(has_text(&out, "Oak Log"));
    assert!(has_text(&out, "minecraft"));
    assert!(has_text(&out, "12 64 -30"));
    assert!(has_text(&out, "Needs axe"));
    assert!(has_text(&out, "Axis: X"));

    // Falls back to the registry id when no display name is set.
    ui.hud.looking_at.display_name.clear();
    let mut out2 = UiDrawList::new();
    assert!(layout_looking_at(&ui, VIEW.0, VIEW.1, &mut out2));
    assert!(has_text(&out2, "minecraft:oak_log"));
}

// ---------------------------------------------------------------------------
// Composition through the real in-game HUD render path
// ---------------------------------------------------------------------------

#[test]
fn all_three_features_compose_through_ui_state_render() {
    let mut ui = hud_ui();
    ui.hud.scoreboard = populated_scoreboard();
    ui.hud
        .title_overlay
        .show_vanilla("Victory", "Subtitle here");
    for _ in 0..30 {
        ui.hud.title_overlay.tick();
    }
    ui.hud.looking_at = populated_looking_at();

    let list = ui.render(VIEW.0, VIEW.1);
    assert!(has_text(&list, "Scores"));
    assert!(has_text(&list, "Alex"));
    assert!(has_text(&list, "Victory"));
    assert!(has_text(&list, "Subtitle here"));
    assert!(has_text(&list, "Oak Log"));
    assert!(has_text(&list, "Needs axe"));

    // Every emitted command stays inside the viewport.
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => {
                assert!(rect.x >= 0 && rect.y >= 0);
                assert!(rect.right() <= VIEW.0 as i32);
                assert!(rect.bottom() <= VIEW.1 as i32);
            }
            UiCommand::Text { x, y, text, .. } => {
                assert!(*x >= 0 && *y >= 0);
                assert!(*x + megu_client_ui::draw::text_width(text, 1) as i32 <= VIEW.0 as i32);
            }
        }
    }

    // And an unpopulated HUD emits none of the feature markers.
    let mut bare = hud_ui();
    let bare_list = bare.render(VIEW.0, VIEW.1);
    for marker in ["Scores", "Alex", "Victory", "Oak Log", "Needs axe"] {
        assert!(!has_text(&bare_list, marker), "{marker} leaked");
    }
}

#[test]
fn features_do_not_render_off_the_ingame_hud_screen() {
    let mut ui = UiState::new(); // Screen::Title by default
    ui.hud.scoreboard = populated_scoreboard();
    ui.hud.title_overlay.show_vanilla("Victory", "");
    for _ in 0..30 {
        ui.hud.title_overlay.tick();
    }
    ui.hud.looking_at = populated_looking_at();
    let list = ui.render(VIEW.0, VIEW.1);
    for marker in ["Scores", "Victory", "Oak Log"] {
        assert!(!has_text(&list, marker), "{marker} on title screen");
    }
}
