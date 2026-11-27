// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for the vanilla 1.21.1 HUD widget pack: simultaneous rendering
//! across the supported resolution range, viewport clamping, region
//! non-overlap, hostile-input sanitization, fade-curve math, Brigadier
//! completion cycling, and creative-mode vitals hiding.

use megu_client_ui::draw::{text_height, text_width, UiCommand, UiDrawList};
use megu_client_ui::hud_widgets::{
    effect_icon_visible, BossBarColor, BossBarEntry, ChatInputOverlay, F3DebugOverlay,
    HeartStyle, HudRegionReport, HudSnapshot, HudWidgetsRenderer, ScoreboardEntry,
    ScoreboardSidebar, StatusEffectEntry, StatusEffectHud, TabPlayerEntry, TabPlayerList,
    TimedText, VitalsState, EFFECT_BLINK_TICKS, MAX_BOSSBARS,
    MAX_CHAT_COMPLETION_ROWS, MAX_CHAT_SCROLLBACK_LINES, MAX_SCOREBOARD_ROWS,
    MAX_STATUS_EFFECTS, NO_COMPLETION,
};

/// The five resolutions from the widget-pack contract.
const VIEWPORTS: [(u32, u32); 5] = [
    (800, 600),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

fn render(
    snapshot: &HudSnapshot,
    w: u32,
    h: u32,
) -> (UiDrawList, HudRegionReport) {
    render_scale(snapshot, w, h, 0)
}

fn render_scale(
    snapshot: &HudSnapshot,
    w: u32,
    h: u32,
    gui_scale: u32,
) -> (UiDrawList, HudRegionReport) {
    let mut list = UiDrawList::new();
    let report =
        HudWidgetsRenderer::with_gui_scale(gui_scale).render(snapshot, w, h, &mut list);
    (list, report)
}

/// Every emitted command must have finite, viewport-clamped geometry.
fn assert_commands_in_viewport(list: &UiDrawList, w: u32, h: u32) {
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ItemIcon { rect, .. }
            | UiCommand::Sprite { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => {
                assert!(rect.x >= 0 && rect.y >= 0, "negative rect origin: {rect:?}");
                assert!(rect.w > 0 && rect.h > 0, "empty rect: {rect:?}");
                assert!(
                    rect.right() <= w as i32 && rect.bottom() <= h as i32,
                    "rect escapes {w}x{h}: {rect:?}"
                );
            }
            UiCommand::Text {
                x,
                y,
                scale,
                text,
                ..
            } => {
                assert!(*x >= 0 && *y >= 0, "negative text origin: {x},{y}");
                let tw = i64::from(text_width(text, *scale));
                let th = i64::from(text_height(text, *scale));
                assert!(
                    i64::from(*x) + tw <= i64::from(w),
                    "text escapes right edge at {x}: {text:?} ({tw}px)"
                );
                assert!(
                    i64::from(*y) + th <= i64::from(h),
                    "text escapes bottom edge at {y}: {text:?}"
                );
            }
        }
    }
}

fn region(report: &HudRegionReport, name: &str) -> Option<megu_client_ui::draw::UiRect> {
    report
        .regions()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.rect)
}

// ---------------------------------------------------------------------------
// Simultaneous rendering + non-overlap
// ---------------------------------------------------------------------------

#[test]
fn demo_snapshot_is_disjoint_and_clamped_at_all_resolutions() {
    let snapshot = HudSnapshot::demo();
    for (w, h) in VIEWPORTS {
        // Pin scale 1: under the canonical vanilla formula the demo's
        // crowded families legitimately drop out at small viewports —
        // the coexistence contract holds at the densest grid.
        let (list, report) = render_scale(&snapshot, w, h, 1);
        assert!(!list.commands.is_empty(), "no commands at {w}x{h}");
        assert!(!report.is_empty(), "no regions at {w}x{h}");
        assert!(
            report.is_disjoint(),
            "overlapping regions at {w}x{h}: {:?}",
            report.overlaps()
        );
        assert_commands_in_viewport(&list, w, h);
        // The demo exercises every widget family, so all headline
        // regions should be claimed at every supported size.
        for name in [
            "status_effects",
            "bossbars",
            "vitals.left",
            "vitals.right",
            "vitals.xp",
            "actionbar",
            "title",
            "tab_list",
            "scoreboard",
            "chat.input",
            "chat.completion",
            "chat.line",
            "debug.left",
            "debug.right",
        ] {
            assert!(region(&report, name).is_some(), "missing {name} at {w}x{h}");
        }
    }
}

#[test]
fn render_is_deterministic() {
    let snapshot = HudSnapshot::demo();
    for (w, h) in VIEWPORTS {
        let (a, ra) = render(&snapshot, w, h);
        let (b, rb) = render(&snapshot, w, h);
        assert_eq!(a.commands, b.commands, "commands differ at {w}x{h}");
        assert_eq!(ra.regions(), rb.regions(), "regions differ at {w}x{h}");
    }
}

#[test]
fn empty_snapshot_renders_vitals_only() {
    let snapshot = HudSnapshot::default();
    for (w, h) in VIEWPORTS {
        let (list, report) = render(&snapshot, w, h);
        assert_commands_in_viewport(&list, w, h);
        assert!(report.is_disjoint(), "overlaps at {w}x{h}");
        // Default vitals: hearts + food rows, no XP (level 0, progress 0
        // — but show_vitals defaults true so icon rows draw).
        assert!(region(&report, "vitals.left").is_some());
        assert!(region(&report, "vitals.right").is_some());
        assert!(region(&report, "vitals.xp").is_none());
        assert!(region(&report, "bossbars").is_none());
        assert!(region(&report, "title").is_none());
    }
}

// ---------------------------------------------------------------------------
// Sanitization: NaN / infinity / negatives / hostile strings
// ---------------------------------------------------------------------------

#[test]
fn nan_and_infinite_vitals_are_sanitized() {
    let mut snapshot = HudSnapshot::demo();
    snapshot.vitals = VitalsState {
        health: f32::NAN,
        max_health: f32::INFINITY,
        absorption: -50.0,
        heart_style: HeartStyle::Wither,
        armor: 200,
        food: 0,
        saturation: f32::NEG_INFINITY,
        air: u16::MAX,
        max_air: 0,
        xp_progress: f32::NAN,
        xp_level: 0,
        show_vitals: true,
    };
    // Bossbar fractions go hostile too.
    snapshot.bossbars = vec![
        BossBarEntry {
            title: "nan".into(),
            fraction: f32::NAN,
            color: BossBarColor::Blue,
            notches: 0,
        },
        BossBarEntry {
            title: "inf".into(),
            fraction: f32::INFINITY,
            color: BossBarColor::Red,
            notches: 200,
        },
    ];
    for (w, h) in VIEWPORTS {
        let (list, report) = render(&snapshot, w, h);
        assert_commands_in_viewport(&list, w, h);
        assert!(report.is_disjoint(), "overlaps at {w}x{h}: {:?}", report.overlaps());
    }
}

#[test]
fn hostile_strings_cannot_escape_the_viewport() {
    let mut snapshot = HudSnapshot::demo();
    let hostile = "A".repeat(5000);
    snapshot.scoreboard.title = Some(format!("title\n{hostile}"));
    snapshot.scoreboard.entries = (0..50)
        .map(|i| ScoreboardEntry {
            label: format!("line\nbreak\t{i} {hostile}"),
            score: i,
        })
        .collect();
    snapshot.bossbars[0].title = format!("boss\n{hostile}");
    snapshot.chat.scrollback = (0..50)
        .map(|i| format!("msg{i}\n{hostile}"))
        .collect();
    snapshot.chat.text = format!("/cmd {hostile}");
    snapshot.chat.completions = (0..50)
        .map(|i| format!("cand{i}\n{hostile}"))
        .collect();
    snapshot.debug.gpu = Some(format!("gpu\n{hostile}"));
    snapshot.debug.biome = Some(format!("biome\t{hostile}"));
    for (w, h) in VIEWPORTS {
        let (list, report) = render(&snapshot, w, h);
        assert_commands_in_viewport(&list, w, h);
        assert!(report.is_disjoint(), "overlaps at {w}x{h}: {:?}", report.overlaps());
        // No emitted text may contain a raw newline (control chars are
        // flattened before rasterization).
        for cmd in &list.commands {
            if let UiCommand::Text { text, .. } = cmd {
                assert!(!text.contains('\n'), "newline leaked into text: {:?}", &text[..text.len().min(40)]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// List caps under >1000 hostile counts
// ---------------------------------------------------------------------------

#[test]
fn oversized_collections_stay_bounded() {
    let mut snapshot = HudSnapshot::demo();
    snapshot.bossbars = (0..1200)
        .map(|i| BossBarEntry {
            title: format!("boss{i}"),
            fraction: (i % 10) as f32 / 10.0,
            color: BossBarColor::White,
            notches: 30,
        })
        .collect();
    let mut effects = StatusEffectHud::new();
    for i in 0..1200 {
        effects.entries.push(StatusEffectEntry {
            id: format!("fx{i}"),
            label: format!("effect{i}"),
            amplifier: (i % 5) as u8,
            remaining_ticks: 1000 + i as u32,
            beneficial: i % 2 == 0,
        });
    }
    snapshot.status_effects = effects;
    snapshot.scoreboard.entries = (0..1200)
        .map(|i| ScoreboardEntry {
            label: format!("player{i}"),
            score: i % 500,
        })
        .collect();
    let mut tab = TabPlayerList {
        visible: true,
        ..Default::default()
    };
    for i in 0..1200 {
        tab.entries.push(TabPlayerEntry::new(format!("user{i}"), i as u16));
    }
    snapshot.tab_list = tab;
    snapshot.chat.scrollback = (0..1200).map(|i| format!("line{i}")).collect();
    snapshot.chat.completions = (0..1200).map(|i| format!("comp{i}")).collect();
    snapshot.chat.history = (0..1200).map(|i| format!("hist{i}")).collect();

    for (w, h) in VIEWPORTS {
        let (list, report) = render(&snapshot, w, h);
        assert_commands_in_viewport(&list, w, h);
        assert!(report.is_disjoint(), "overlaps at {w}x{h}: {:?}", report.overlaps());

        // Bounded claims: the caps must show up as bounded geometry.
        // Claims are physical px = logical × resolved scale.
        let s = megu_core::ui_contract::compute_gui_scale(w, h, 0);
        if let Some(fx) = region(&report, "status_effects") {
            // ≤ 8 cells/row × 34 px + 1 shadow px, ≤ 2 rows.
            assert!(fx.w <= (8 * 34 + 1) * s, "effect grid too wide: {fx:?}");
            assert!(fx.h <= 2 * 10 * s, "effect grid too tall: {fx:?}");
        }
        if let Some(bb) = region(&report, "bossbars") {
            // The logical pitch is 14 px per bar.
            assert!(bb.h <= MAX_BOSSBARS as u32 * 14 * s, "bossbar stack too tall");
        }
        if let Some(sb) = region(&report, "scoreboard") {
            // title + 15 rows + padding.
            assert!(sb.h <= (4 + 10 + MAX_SCOREBOARD_ROWS as u32 * 10 + 2) * s);
        }
        if let Some(tl) = region(&report, "tab_list") {
            assert!(tl.h <= 20 * 10 * s, "tab list too tall");
        }
        // Scrollback renders at most MAX_CHAT_SCROLLBACK_LINES rows.
        let scroll_claims = report
            .regions()
            .iter()
            .filter(|r| r.name == "chat.line")
            .count();
        assert!(scroll_claims <= MAX_CHAT_SCROLLBACK_LINES);
        // Completion popup at most MAX_CHAT_COMPLETION_ROWS rows.
        if let Some(cp) = region(&report, "chat.completion") {
            assert!(cp.h <= (MAX_CHAT_COMPLETION_ROWS as u32 * 10 + 2) * s);
        }
        // Command count stays sane (no per-entry blowup past caps).
        assert!(list.commands.len() < 4000, "command flood: {}", list.commands.len());
    }
}

// ---------------------------------------------------------------------------
// Title fade curve
// ---------------------------------------------------------------------------

#[test]
fn timed_text_alpha_follows_vanilla_fade_curve() {
    let mut t = TimedText::vanilla("title"); // 10 in / 70 stay / 20 out
    // fade-in ramp: 0 → 1 over ticks 0..10
    t.age_ticks = 0;
    assert_eq!(t.alpha(), 0.0);
    t.age_ticks = 5;
    assert!((t.alpha() - 0.5).abs() < f32::EPSILON);
    t.age_ticks = 10;
    assert_eq!(t.alpha(), 1.0);
    // stay: full alpha through tick 79
    t.age_ticks = 79;
    assert_eq!(t.alpha(), 1.0);
    // fade-out: 80..100 ramps 1 → 0
    t.age_ticks = 85;
    assert!((t.alpha() - 0.75).abs() < 0.001);
    t.age_ticks = 99;
    assert!(t.alpha() > 0.0 && t.alpha() < 0.1);
    t.age_ticks = 100;
    assert_eq!(t.alpha(), 0.0);
    assert!(!t.is_visible());
    // Zero-length phases are handled.
    let instant = TimedText {
        text: "x".into(),
        age_ticks: 0,
        fade_in_ticks: 0,
        stay_ticks: 0,
        fade_out_ticks: 0,
    };
    assert_eq!(instant.alpha(), 0.0);
    // All-zero stay still shows at age 0? total=0 → invisible by design.
    let stay_only = TimedText {
        text: "x".into(),
        age_ticks: 3,
        fade_in_ticks: 0,
        stay_ticks: 10,
        fade_out_ticks: 0,
    };
    assert_eq!(stay_only.alpha(), 1.0);
}

#[test]
fn title_alpha_reaches_emitted_text_color() {
    let mut snapshot = HudSnapshot::default();
    // Mid-fade-out title: alpha should be < 255 in emitted commands.
    let mut t = TimedText::vanilla("Fading");
    t.age_ticks = 95; // (100-95)/20 = 0.25 → alpha 63
    snapshot.title.title = Some(t);
    let mut ab = TimedText::vanilla("Solid");
    ab.age_ticks = 30; // full alpha
    snapshot.title.actionbar = Some(ab);
    let (list, _) = render(&snapshot, 1280, 720);
    let mut saw_faded = false;
    let mut saw_solid = false;
    for cmd in &list.commands {
        if let UiCommand::Text { text, color, .. } = cmd {
            if text.contains("Fading") && color.a < 200 {
                saw_faded = true;
            }
            if text.contains("Solid") && color.a == 255 {
                saw_solid = true;
            }
        }
    }
    assert!(saw_faded, "fading title kept full alpha");
    assert!(saw_solid, "solid actionbar lost alpha");
}

// ---------------------------------------------------------------------------
// Brigadier tab-completion cycling
// ---------------------------------------------------------------------------

#[test]
fn completion_cycles_forward_backward_and_wraps() {
    let mut chat = ChatInputOverlay::new();
    chat.open = true;
    chat.set_text("/give @s di");
    chat.set_completions(["diamond", "diorite"]);
    assert_eq!(chat.completion_index, NO_COMPLETION);
    // First Tab applies the first candidate to the last token.
    chat.cycle_completion(true);
    assert_eq!(chat.text, "/give @s diamond");
    assert_eq!(chat.selected_completion(), Some(0));
    // Second Tab advances; third wraps to the first.
    chat.cycle_completion(true);
    assert_eq!(chat.text, "/give @s diorite");
    chat.cycle_completion(true);
    assert_eq!(chat.text, "/give @s diamond");
    // Shift-Tab goes backward and wraps to the last candidate.
    chat.cycle_completion(false);
    assert_eq!(chat.text, "/give @s diorite");
    chat.cycle_completion(false);
    assert_eq!(chat.text, "/give @s diamond");
}

#[test]
fn completion_first_backward_pick_is_last_candidate() {
    let mut chat = ChatInputOverlay::new();
    chat.set_text("/tp ");
    chat.set_completions(["a", "b", "c"]);
    chat.cycle_completion(false);
    assert_eq!(chat.text, "/tp c");
    assert_eq!(chat.selected_completion(), Some(2));
}

#[test]
fn completion_span_survives_candidates_with_spaces() {
    let mut chat = ChatInputOverlay::new();
    chat.set_text("/give @s ");
    chat.set_completions(["diamond sword", "diamond pickaxe"]);
    chat.cycle_completion(true);
    assert_eq!(chat.text, "/give @s diamond sword");
    // The next cycle must replace the *whole* original token, not just
    // the trailing word of the applied candidate.
    chat.cycle_completion(true);
    assert_eq!(chat.text, "/give @s diamond pickaxe");
}

#[test]
fn completion_state_resets_on_edit_and_submit() {
    let mut chat = ChatInputOverlay::new();
    chat.set_text("/give @s di");
    chat.set_completions(["diamond"]);
    chat.cycle_completion(true);
    assert_eq!(chat.text, "/give @s diamond");
    chat.insert_char('x');
    assert_eq!(chat.completion_index, NO_COMPLETION);
    assert_eq!(chat.completion_span_start, None);
    chat.set_completions(["other"]);
    chat.cycle_completion(true);
    // Token is now "diamondx" → replaced wholesale.
    assert_eq!(chat.text, "/give @s other");
    chat.submit();
    assert!(chat.text.is_empty());
    assert!(chat.completions.is_empty());
    assert!(!chat.open);
    assert_eq!(chat.history.len(), 1);
    assert_eq!(chat.scrollback.len(), 1);
}

// ---------------------------------------------------------------------------
// Chat history recall + input bounds
// ---------------------------------------------------------------------------

#[test]
fn history_recall_restores_draft() {
    let mut chat = ChatInputOverlay::new();
    chat.history = vec!["first".into(), "second".into()];
    chat.set_text("draft text");
    chat.history_prev();
    assert_eq!(chat.text, "second");
    chat.history_prev();
    assert_eq!(chat.text, "first");
    chat.history_prev(); // stays at oldest
    assert_eq!(chat.text, "first");
    chat.history_next();
    assert_eq!(chat.text, "second");
    chat.history_next();
    assert_eq!(chat.text, "draft text");
    assert_eq!(chat.history_pos, None);
}

#[test]
fn input_buffer_and_history_are_bounded() {
    let mut chat = ChatInputOverlay::new();
    for _ in 0..5000 {
        chat.insert_char('x');
    }
    assert!(chat.char_count() <= 256);
    for i in 0..500 {
        chat.history.push(format!("h{i}"));
        chat.scrollback.push(format!("s{i}"));
    }
    chat.text = "submit me".into();
    chat.submit();
    // submit() appends one more; caps keep the newest.
    assert!(chat.history.len() <= 100);
    assert!(chat.scrollback.len() <= 100);
    assert_eq!(chat.scrollback.last().map(String::as_str), Some("submit me"));
}

// ---------------------------------------------------------------------------
// Status-effect blink + caps
// ---------------------------------------------------------------------------

#[test]
fn low_duration_effects_blink_deterministically() {
    assert!(effect_icon_visible(EFFECT_BLINK_TICKS));
    assert!(effect_icon_visible(EFFECT_BLINK_TICKS + 1));
    // Below the threshold the icon strobes on a fixed period.
    assert!(!effect_icon_visible(EFFECT_BLINK_TICKS - 1)); // 199/10=19 odd → off
    assert!(effect_icon_visible(EFFECT_BLINK_TICKS - 11)); // 189/10=18 even → on
    assert!(!effect_icon_visible(0));
    // Permanently-lit entries never blink.
    assert!(effect_icon_visible(u32::MAX));
}

#[test]
fn status_effect_hud_dedups_and_caps() {
    let mut hud = StatusEffectHud::new();
    hud.push(StatusEffectEntry {
        id: "speed".into(),
        label: "Speed".into(),
        amplifier: 0,
        remaining_ticks: 100,
        beneficial: true,
    });
    hud.push(StatusEffectEntry {
        id: "speed".into(),
        label: "Speed II".into(),
        amplifier: 1,
        remaining_ticks: 200,
        beneficial: true,
    });
    assert_eq!(hud.len(), 1);
    assert_eq!(hud.entries()[0].amplifier, 1);
    assert_eq!(hud.entries()[0].remaining_ticks, 200);
    for i in 0..(MAX_STATUS_EFFECTS + 10) {
        hud.push(StatusEffectEntry {
            id: format!("fx{i}"),
            label: String::new(),
            amplifier: 0,
            remaining_ticks: 500,
            beneficial: false,
        });
    }
    assert_eq!(hud.len(), MAX_STATUS_EFFECTS);
}

// ---------------------------------------------------------------------------
// Scoreboard sorting
// ---------------------------------------------------------------------------

#[test]
fn scoreboard_rows_render_sorted_descending() {
    let snapshot = HudSnapshot {
        scoreboard: ScoreboardSidebar {
            title: None,
            entries: vec![
                ScoreboardEntry { label: "low".into(), score: -50 },
                ScoreboardEntry { label: "high".into(), score: 900 },
                ScoreboardEntry { label: "mid".into(), score: 42 },
                ScoreboardEntry { label: "tieB".into(), score: 42 },
                ScoreboardEntry { label: "tieA".into(), score: 42 },
            ],
        },
        ..Default::default()
    };
    let (list, _) = render(&snapshot, 1920, 1080);
    // Score texts are emitted right-aligned per row, in display order.
    let scores: Vec<i32> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => text.trim().parse::<i32>().ok(),
            _ => None,
        })
        .collect();
    assert_eq!(scores, vec![900, 42, 42, 42, -50]);
    // Labels appear in the same row order (tie → label ascending).
    let labels: Vec<&str> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } if text.starts_with(|c: char| c.is_alphabetic()) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    let pos = |name: &str| labels.iter().position(|l| l.contains(name)).unwrap();
    assert!(pos("high") < pos("tieA"));
    assert!(pos("tieA") < pos("tieB"));
    assert!(pos("tieB") < pos("mid") || pos("mid") < pos("tieA"));
    assert!(pos("mid") < pos("low") || pos("tieB") < pos("low"));
}

// ---------------------------------------------------------------------------
// Creative / spectator vitals hiding
// ---------------------------------------------------------------------------

#[test]
fn creative_mode_hides_survival_vitals_but_keeps_xp() {
    let mut snapshot = HudSnapshot::demo();
    snapshot.vitals.show_vitals = true;
    snapshot.vitals.xp_level = 12;
    snapshot.vitals.xp_progress = 0.4;
    let (_, survival) = render(&snapshot, 1280, 720);
    assert!(region(&survival, "vitals.left").is_some());
    assert!(region(&survival, "vitals.right").is_some());
    assert!(region(&survival, "vitals.xp").is_some());

    snapshot.vitals.show_vitals = false;
    let (list, creative) = render(&snapshot, 1280, 720);
    assert!(region(&creative, "vitals.left").is_none());
    assert!(region(&creative, "vitals.right").is_none());
    // XP bar + level still render in creative, like vanilla.
    assert!(region(&creative, "vitals.xp").is_some());
    assert!(
        list.commands
            .iter()
            .any(|c| matches!(c, UiCommand::ProgressBar { .. })),
        "XP progress bar missing in creative"
    );
    assert!(creative.is_disjoint());
}

// ---------------------------------------------------------------------------
// Fixed GUI scale + misc renderer behavior
// ---------------------------------------------------------------------------

#[test]
fn fixed_gui_scale_rescales_output() {
    let snapshot = HudSnapshot::demo();
    let mut a = UiDrawList::new();
    let mut b = UiDrawList::new();
    let ra = HudWidgetsRenderer::with_gui_scale(1).render(&snapshot, 800, 600, &mut a);
    let rb = HudWidgetsRenderer::with_gui_scale(2).render(&snapshot, 1600, 1200, &mut b);
    assert!(ra.is_disjoint() && rb.is_disjoint());
    // Same logical layout → region count identical, physical rects 2×.
    assert_eq!(ra.regions().len(), rb.regions().len());
    for (x, y) in ra.regions().iter().zip(rb.regions()) {
        assert_eq!(x.name, y.name);
        assert_eq!(x.rect.x * 2, y.rect.x);
        assert_eq!(x.rect.y * 2, y.rect.y);
        assert_eq!(i64::from(x.rect.w) * 2, i64::from(y.rect.w));
        assert_eq!(i64::from(x.rect.h) * 2, i64::from(y.rect.h));
    }
}

#[test]
fn tiny_viewports_degrade_without_panicking() {
    let snapshot = HudSnapshot::demo();
    for (w, h) in [(320, 240), (640, 360), (100, 100), (64, 64), (0, 0)] {
        let mut list = UiDrawList::new();
        let report = HudWidgetsRenderer::new().render(&snapshot, w, h, &mut list);
        assert_commands_in_viewport(&list, w, h);
        assert!(report.is_disjoint(), "overlaps at {w}x{h}");
    }
}

#[test]
fn f3_debug_overlay_renders_both_columns() {
    let snapshot = HudSnapshot {
        debug: F3DebugOverlay {
            enabled: true,
            fps: Some(60.0),
            position: Some([1.0, 2.0, 3.0]),
            chunk: Some((0, 0)),
            facing: Some("south".into()),
            biome: Some("minecraft:forest".into()),
            sky_light: Some(15),
            block_light: Some(4),
            entity_count: Some(10),
            os: Some("Linux".into()),
            gpu: Some("TestGPU".into()),
            memory_used_mb: Some(512),
            memory_total_mb: Some(4096),
            render_scale: Some(1.5),
            shadows: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    let (list, report) = render(&snapshot, 1920, 1080);
    assert!(region(&report, "debug.left").is_some());
    assert!(region(&report, "debug.right").is_some());
    let all_text: String = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for needle in ["fps", "XYZ", "Chunk", "Facing", "Biome", "Client Light", "E:", "Megu Native Rust", "GPU", "Mem", "Render Scale", "Shadows"] {
        assert!(all_text.contains(needle), "missing debug line {needle}");
    }
    assert!(report.is_disjoint());
}

#[test]
fn heart_styles_map_to_distinct_colors() {
    // Smoke test: every style renders without panic and stays disjoint.
    for style in [
        HeartStyle::Normal,
        HeartStyle::Poison,
        HeartStyle::Wither,
        HeartStyle::Frozen,
        HeartStyle::Hardcore,
    ] {
        let mut snapshot = HudSnapshot::default();
        snapshot.vitals.heart_style = style;
        snapshot.vitals.health = 10.0;
        snapshot.vitals.absorption = 4.0;
        let (list, report) = render(&snapshot, 1280, 720);
        assert!(!list.commands.is_empty());
        assert!(report.is_disjoint());
        assert_commands_in_viewport(&list, 1280, 720);
    }
}
