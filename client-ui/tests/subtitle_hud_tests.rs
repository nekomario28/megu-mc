// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for the spatial subtitle HUD: dedup/capacity/TTL semantics,
//! directional formatting, fade-out, and viewport/hotbar non-overlap.

use megu_client_ui::draw::{text_width, UiColor, UiCommand, UiDrawList, UiRect};
use megu_client_ui::layout::{layout_subtitle_hud, layout_subtitle_hud_with_scratch};
use megu_client_ui::state::{HudState, Screen, UiState};
use megu_client_ui::subtitle_hud::{
    SubtitleArrow, SubtitleHudState, DEFAULT_TTL_SECS, MAX_SUBTITLE_ENTRIES,
};
use megu_client_ui::vanilla_metrics as vm;

fn state_with(entries: &[(&str, &str, SubtitleArrow)]) -> SubtitleHudState {
    let mut s = SubtitleHudState::new();
    for (id, label, arrow) in entries {
        s.push(*id, *label, *arrow, 3.0);
    }
    s
}

// ---------------------------------------------------------------------------
// State semantics
// ---------------------------------------------------------------------------

#[test]
fn push_deduplicates_by_id_and_refreshes() {
    let mut s = SubtitleHudState::new();
    s.push("zombie", "Zombie groans", SubtitleArrow::Left, 2.0);
    s.tick(1.0);
    s.push("zombie", "Zombie groans", SubtitleArrow::Right, 5.0);
    assert_eq!(s.len(), 1);
    let e = &s.entries()[0];
    assert_eq!(e.id, "zombie");
    assert_eq!(e.arrow, SubtitleArrow::Right);
    assert_eq!(e.ttl_secs, 5.0);
    assert_eq!(e.max_ttl_secs, 5.0);
    assert_eq!(e.count, 2);
}

#[test]
fn push_is_bounded_and_evicts_oldest() {
    let mut s = SubtitleHudState::new();
    for i in 0..(MAX_SUBTITLE_ENTRIES + 3) {
        s.push(format!("id{i}"), format!("Label {i}"), SubtitleArrow::Center, 3.0);
    }
    assert_eq!(s.len(), MAX_SUBTITLE_ENTRIES);
    assert_eq!(s.entries()[0].id, "id3");
    assert_eq!(s.entries()[MAX_SUBTITLE_ENTRIES - 1].id, "id8");
}

#[test]
fn push_refresh_moves_entry_to_newest() {
    let mut s = SubtitleHudState::new();
    s.push("a", "A", SubtitleArrow::Center, 3.0);
    s.push("b", "B", SubtitleArrow::Center, 3.0);
    s.push("c", "C", SubtitleArrow::Center, 3.0);
    s.push("a", "A2", SubtitleArrow::Left, 3.0);
    assert_eq!(s.len(), 3);
    assert_eq!(s.entries()[0].id, "b");
    assert_eq!(s.entries()[2].id, "a");
    assert_eq!(s.entries()[2].label, "A2");
    assert_eq!(s.entries()[2].arrow, SubtitleArrow::Left);
    assert_eq!(s.entries()[2].count, 2);
}

#[test]
fn push_sanitizes_duration_and_label() {
    let mut s = SubtitleHudState::new();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.5] {
        s.push("bad", "X", SubtitleArrow::Center, bad);
        assert_eq!(s.entries()[0].ttl_secs, DEFAULT_TTL_SECS);
        assert_eq!(s.entries()[0].max_ttl_secs, DEFAULT_TTL_SECS);
        s.clear();
    }
    s.push("long", "x".repeat(200), SubtitleArrow::Center, 3.0);
    let label = &s.entries()[0].label;
    assert!(label.chars().count() <= 40);
    assert!(label.ends_with("..."));
    s.push("ctrl", "a\nb\tc", SubtitleArrow::Center, 3.0);
    assert!(!s.entries()[1].label.chars().any(char::is_control));
}

#[test]
fn tick_expires_deterministically_and_ignores_bad_dt() {
    let mut s = state_with(&[("a", "A", SubtitleArrow::Center)]);
    s.push("b", "B", SubtitleArrow::Center, 1.0);
    for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5, 0.0] {
        s.tick(dt);
        assert_eq!(s.len(), 2, "dt={dt} must be a no-op");
    }
    s.tick(0.5);
    assert_eq!(s.len(), 2);
    assert!((s.entries()[1].ttl_secs - 0.5).abs() < 1e-6);
    s.tick(0.5);
    assert_eq!(s.len(), 1);
    assert_eq!(s.entries()[0].id, "a");
    s.tick(10.0);
    assert!(s.is_empty());
    // Same input sequence → same state.
    let mut s1 = state_with(&[("a", "A", SubtitleArrow::Center)]);
    let mut s2 = state_with(&[("a", "A", SubtitleArrow::Center)]);
    for _ in 0..5 {
        s1.tick(0.2);
        s2.tick(0.2);
    }
    assert_eq!(s1, s2);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn subtitle_bounds(commands: &[UiCommand]) -> Vec<UiRect> {
    let mut out = Vec::new();
    for cmd in commands {
        match cmd {
            UiCommand::FillRect { rect, .. } => out.push(*rect),
            UiCommand::Text {
                x, y, scale, text, ..
            } => out.push(UiRect::new(
                *x,
                *y,
                text_width(text, *scale),
                8 * *scale,
            )),
            _ => {}
        }
    }
    out
}

fn union(bounds: &[UiRect]) -> Option<UiRect> {
    let mut it = bounds.iter();
    let first = *it.next()?;
    let mut u = first;
    for r in it {
        let x0 = u.x.min(r.x);
        let y0 = u.y.min(r.y);
        let x1 = u.right().max(r.right());
        let y1 = u.bottom().max(r.bottom());
        u = UiRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32);
    }
    Some(u)
}

fn hotbar_rect(w: u32, h: u32) -> UiRect {
    // Mirrors hud(): effective_scale = (h/270).clamp(1,4) for ui_scale=0.
    // Vanilla 1.21.1 hotbar geometry lives in `vanilla_metrics`: the 29 px
    // offhand frame at `w/2 - 120` flush with the 182×22 strip at `h - 22`,
    // whose last row the viewport clips.
    let s = (h / 270).clamp(1, 4) as i32;
    let lw = w as i32 / s;
    let lh = h as i32 / s;
    let (width, height) =
        (vm::HOTBAR_BLOCK_WIDTH, vm::OFFHAND_H - 1);
    let x = lw / 2 - vm::VITALS_HALF_SPAN - vm::OFFHAND_W;
    let y = lh - vm::OFFHAND_Y_OFFSET;
    UiRect::new(x * s, y * s, width as u32 * s as u32, height as u32 * s as u32)
}

#[test]
fn render_formats_directional_markers() {
    let s = state_with(&[
        ("l", "LeftSound", SubtitleArrow::Left),
        ("c", "CenterSound", SubtitleArrow::Center),
        ("r", "RightSound", SubtitleArrow::Right),
        ("b", "BehindSound", SubtitleArrow::Behind),
    ]);
    let mut list = UiDrawList::new();
    s.render(1280.0, 720.0, &mut list);
    assert!(!list.is_empty());
    let texts: Vec<&str> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"< LeftSound"), "{texts:?}");
    assert!(texts.contains(&"CenterSound"), "{texts:?}");
    assert!(texts.contains(&"RightSound >"), "{texts:?}");
    assert!(texts.contains(&"v BehindSound"), "{texts:?}");
    // Newest row sits above the oldest: "b" was pushed last → topmost.
    let ys: Vec<i32> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { y, text, .. } if text.contains("Sound") => Some(*y),
            _ => None,
        })
        .collect();
    assert_eq!(ys.len(), 4);
    assert!(ys[0] < ys[3], "newest row should be topmost: {ys:?}");
}

#[test]
fn render_fades_out_in_final_half_second() {
    let fresh = state_with(&[("a", "Fresh", SubtitleArrow::Center)]);
    let mut dying = state_with(&[("b", "Dying", SubtitleArrow::Center)]);
    dying.tick(2.9); // ttl = 3.0 - 2.9 = 0.1 < FADE_TAIL_SECS
    let mut lf = UiDrawList::new();
    let mut ld = UiDrawList::new();
    fresh.render(1280.0, 720.0, &mut lf);
    dying.render(1280.0, 720.0, &mut ld);
    let alpha_of = |list: &UiDrawList| -> u8 {
        list.commands
            .iter()
            .find_map(|c| match c {
                UiCommand::Text { color, .. } => Some(color.a),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(alpha_of(&lf), 255);
    assert!(alpha_of(&ld) < 255);
    assert!(alpha_of(&ld) > 0);
    dying.tick(0.2);
    let mut le = UiDrawList::new();
    dying.render(1280.0, 720.0, &mut le);
    assert!(le.is_empty());
}

#[test]
fn render_respects_viewport_and_hotbar_across_resolutions() {
    for (w, h) in [
        (800u32, 600u32),
        (1280, 720),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ] {
        let mut s = state_with(&[
            ("a", "Zombie groans", SubtitleArrow::Left),
            ("b", "Creeper Hisses", SubtitleArrow::Center),
            ("c", "Skeleton shoots", SubtitleArrow::Right),
            ("d", "Witch giggles", SubtitleArrow::Behind),
            ("e", "Enderman teleports", SubtitleArrow::Left),
            ("f", "A deliberately quite long subtitle label here", SubtitleArrow::Center),
        ]);
        s.tick(0.1);
        let mut list = UiDrawList::new();
        s.render(w as f32, h as f32, &mut list);
        assert!(!list.is_empty(), "{w}x{h}");
        let bounds = subtitle_bounds(&list.commands);
        assert!(!bounds.is_empty());
        let panel = union(&bounds).unwrap();
        // Strictly inside the viewport.
        assert!(panel.x >= 0 && panel.y >= 0, "{w}x{h}: {panel:?}");
        assert!(panel.right() <= w as i32 && panel.bottom() <= h as i32, "{w}x{h}: {panel:?}");
        // Zero overlap with the bottom-center hotbar.
        let hb = hotbar_rect(w, h);
        assert!(!panel.intersects(hb), "{w}x{h}: panel {panel:?} vs hotbar {hb:?}");
        // Deterministic: same input → same commands.
        let mut again = UiDrawList::new();
        s.render(w as f32, h as f32, &mut again);
        assert_eq!(list.commands, again.commands);
    }
}

#[test]
fn render_handles_degenerate_viewports() {
    let s = state_with(&[("a", "A", SubtitleArrow::Center)]);
    for (w, h) in [(0.0, 0.0), (-1.0, 100.0), (f32::NAN, 100.0), (100.0, f32::INFINITY)] {
        let mut list = UiDrawList::new();
        s.render(w, h, &mut list);
        assert!(list.is_empty(), "{w}x{h}");
    }
    // Tiny but valid: still inside viewport or empty.
    let mut list = UiDrawList::new();
    s.render(160.0, 120.0, &mut list);
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. } => {
                assert!(rect.x >= 0 && rect.y >= 0 && rect.right() <= 160 && rect.bottom() <= 120);
            }
            UiCommand::Text { x, y, scale, text, .. } => {
                assert!(*x >= 0 && *y >= 0);
                assert!(*x + text_width(text, *scale) as i32 <= 160);
                assert!(*y + 8 * *scale as i32 <= 120);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// HUD layout integration
// ---------------------------------------------------------------------------

fn hostile_ui(subtitles_on: bool) -> UiState {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud = HudState::demo();
    ui.hud.absorption = 8.0;
    ui.hud.air = 120;
    ui.hud.actionbar = "A long actionbar message that must be clipped".into();
    ui.hud.chat_lines = (0..6)
        .map(|i| format!("<P{i}> {}", "chat line that is quite long indeed ".repeat(3)))
        .collect();
    ui.hud.show_player_list = true;
    ui.hud.players = (0..12).map(|i| (format!("Player{i}"), 20)).collect();
    ui.hud.bossbars = vec![("A rather long boss bar name".into(), 0.5)];
    ui.hud.magic.max_mana = 100.0;
    ui.hud.magic.mana = 40.0;
    ui.settings.qol.show_subtitles = subtitles_on;
    ui
}

fn spatial_subtitles() -> SubtitleHudState {
    state_with(&[
        ("a", "Zombie groans", SubtitleArrow::Left),
        ("b", "Creeper Hisses", SubtitleArrow::Center),
        ("c", "Skeleton shoots", SubtitleArrow::Right),
        ("d", "Witch giggles behind", SubtitleArrow::Behind),
        ("e", "A rather long spatial subtitle label", SubtitleArrow::Left),
    ])
}

#[test]
fn layout_subtitle_hud_gates_on_setting_and_emptiness() {
    for (w, h) in [(1280u32, 720u32), (640, 360)] {
        let subs = spatial_subtitles();
        // Disabled → nothing appended, false returned.
        let mut ui = hostile_ui(false);
        let mut out = ui.render(w, h);
        let before = out.commands.len();
        assert!(!layout_subtitle_hud(&ui, &subs, w, h, &mut out));
        assert_eq!(out.commands.len(), before);
        // Enabled but empty state → nothing.
        let mut ui = hostile_ui(true);
        let mut out = ui.render(w, h);
        let before = out.commands.len();
        assert!(!layout_subtitle_hud(&ui, &SubtitleHudState::new(), w, h, &mut out));
        assert_eq!(out.commands.len(), before);
        // Enabled + live cues → rows appended.
        let mut out = ui.render(w, h);
        let before = out.commands.len();
        assert!(layout_subtitle_hud(&ui, &subs, w, h, &mut out));
        assert!(out.commands.len() > before);
    }
}

#[test]
fn scratch_layout_matches_legacy_and_reuses_occupancy_capacity() {
    let (w, h) = (1280, 720);
    let mut ui = hostile_ui(true);
    let subs = spatial_subtitles();
    let mut expected = ui.render(w, h);
    let mut actual = ui.render(w, h);
    assert!(layout_subtitle_hud(&ui, &subs, w, h, &mut expected));

    let mut occupied = Vec::new();
    assert!(layout_subtitle_hud_with_scratch(
        &ui,
        &subs,
        w,
        h,
        &mut actual,
        &mut occupied,
    ));
    assert_eq!(actual, expected);
    assert!(!occupied.is_empty());
    let capacity = occupied.capacity();

    let mut next = ui.render(w, h);
    assert!(layout_subtitle_hud_with_scratch(
        &ui,
        &subs,
        w,
        h,
        &mut next,
        &mut occupied,
    ));
    assert_eq!(next, expected);
    assert_eq!(occupied.capacity(), capacity);
}

#[test]
fn layout_subtitle_hud_never_overlaps_hud_chrome() {
    for (w, h) in [
        (800u32, 600u32),
        (1280, 720),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ] {
        let mut ui = hostile_ui(true);
        ui.hud.subtitles = vec![
            megu_client_ui::state::HudSubtitle {
                label: "Legacy zombie groans".into(),
                direction: megu_client_ui::state::SubtitleDirection::Left,
            },
            megu_client_ui::state::HudSubtitle {
                label: "Legacy creeper".into(),
                direction: megu_client_ui::state::SubtitleDirection::Behind,
            },
        ];
        let subs = spatial_subtitles();
        let mut out = ui.render(w, h);
        let hud_cmds = out.commands.len();
        assert!(layout_subtitle_hud(&ui, &subs, w, h, &mut out));
        // Everything the overlay added must stay inside the viewport and
        // clear the hotbar.
        let panel = union(&subtitle_bounds(&out.commands[hud_cmds..]))
            .expect("overlay emitted nothing");
        assert!(panel.x >= 0 && panel.y >= 0 && panel.right() <= w as i32 && panel.bottom() <= h as i32,
            "{w}x{h}: {panel:?}");
        let hb = hotbar_rect(w, h);
        assert!(!panel.intersects(hb), "{w}x{h}: {panel:?} vs {hb:?}");
        // And the translucent row backings must not sit on top of any
        // rgba(0,0,0,110) backing the HUD itself drew (chat/subtitles/
        // minimap share that exact color).
        let hud_fills: Vec<UiRect> = out.commands[..hud_cmds]
            .iter()
            .filter_map(|c| match c {
                UiCommand::FillRect { rect, color } if *color == UiColor::rgba(0, 0, 0, 110) => {
                    Some(*rect)
                }
                _ => None,
            })
            .collect();
        for cmd in &out.commands[hud_cmds..] {
            if let UiCommand::FillRect { rect, .. } = cmd {
                for f in &hud_fills {
                    assert!(
                        !rect.intersects(*f),
                        "{w}x{h}: subtitle backing {rect:?} overlaps HUD backing {f:?}"
                    );
                }
            }
        }
    }
}
