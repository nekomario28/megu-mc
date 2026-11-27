// SPDX-License-Identifier: LGPL-3.0-or-later
//! GUI-scale parity: every surface that used to carry an ad-hoc
//! `height / 270` or `min(h / 360, w / 320)` formula now resolves through
//! the canonical [`megu_core::ui_contract::compute_gui_scale`]. These
//! tests pin that contract across 720p/1080p/1440p/4K and explicit
//! scales `1..=4` for the state-driven screens, and verify the HUD
//! overlays emit text at the canonical auto scale.

use megu_client_ui::block_inspector_hud::{BlockInspectorHudLayout, BlockInspectorTelemetry};
use megu_client_ui::draw::UiCommand;
use megu_client_ui::magic_hud::MagicHudState;
use megu_client_ui::mod_manager::ModManagerState;
use megu_client_ui::recipe_browser::RecipeBrowserState;
use megu_client_ui::subtitle_hud::{SubtitleArrow, SubtitleHudState};
use megu_core::ui_contract::compute_gui_scale;

const RESOLUTIONS: [(u32, u32); 4] = [
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

#[test]
fn recipe_browser_effective_scale_matches_canonical_formula() {
    let mut state = RecipeBrowserState::default();
    for requested in 0..=4u8 {
        state.ui_scale = requested;
        for &(w, h) in &RESOLUTIONS {
            assert_eq!(
                state.effective_scale(w, h),
                compute_gui_scale(w, h, u32::from(requested)),
                "recipe_browser {w}x{h} ui_scale={requested}"
            );
        }
    }
}

#[test]
fn mod_manager_effective_scale_matches_canonical_formula() {
    let mut state = ModManagerState::default();
    for requested in 0..=4u8 {
        state.ui_scale = requested;
        for &(w, h) in &RESOLUTIONS {
            assert_eq!(
                state.effective_scale(w, h),
                compute_gui_scale(w, h, u32::from(requested)),
                "mod_manager {w}x{h} ui_scale={requested}"
            );
        }
    }
}

/// Every `UiCommand::Text` carries the GUI scale it was emitted at —
/// a direct probe of the scale each overlay resolved internally.
fn emitted_text_scales(commands: &[UiCommand]) -> Vec<u32> {
    commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::Text { scale, .. } => Some(*scale),
            _ => None,
        })
        .collect()
}

fn magic_state() -> MagicHudState {
    MagicHudState {
        mana: 40.0,
        max_mana: 100.0,
        ..Default::default()
    }
}

fn inspector_telemetry() -> BlockInspectorTelemetry {
    BlockInspectorTelemetry {
        block_id: "acme_tech:millstone".into(),
        display_name: Some("Millstone".into()),
        rpm: 32.0,
        stress_impact: 16.0,
        network_stress_used: 128.0,
        network_stress_capacity: 256.0,
        overstressed: false,
        processing_item: Some("minecraft:wheat".into()),
        processing_progress: Some(0.5),
        fluid_mb: Some(("minecraft:water".into(), 500, 1000)),
        extra_metrics: Vec::new(),
    }
}

fn subtitle_state() -> SubtitleHudState {
    let mut state = SubtitleHudState::new();
    state.push("a", "LeftSound", SubtitleArrow::Left, 3.0);
    state.push("b", "CenterSound", SubtitleArrow::Center, 3.0);
    state
}

#[test]
fn magic_hud_emits_at_canonical_auto_scale() {
    let state = magic_state();
    for &(w, h) in &RESOLUTIONS {
        let expected = compute_gui_scale(w, h, 0);
        let list = megu_client_ui::magic_hud::build_magic_hud_draw_list(&state, w, h);
        let scales = emitted_text_scales(&list.commands);
        assert!(!scales.is_empty(), "magic HUD emitted no text at {w}x{h}");
        assert!(
            scales.iter().all(|&s| s == expected),
            "magic HUD text scale {scales:?} != canonical {expected} at {w}x{h}"
        );
    }
}

#[test]
fn block_inspector_hud_emits_at_canonical_auto_scale() {
    let telemetry = inspector_telemetry();
    for &(w, h) in &RESOLUTIONS {
        let expected = compute_gui_scale(w, h, 0);
        let list = BlockInspectorHudLayout::build(&telemetry, w, h, "en_us");
        let scales = emitted_text_scales(&list.commands);
        assert!(
            !scales.is_empty(),
            "block inspector HUD emitted no text at {w}x{h}"
        );
        assert!(
            scales.iter().all(|&s| s == expected),
            "block inspector HUD text scale {scales:?} != canonical {expected} at {w}x{h}"
        );
    }
}

#[test]
fn subtitle_hud_emits_at_canonical_auto_scale() {
    let state = subtitle_state();
    for &(w, h) in &RESOLUTIONS {
        let expected = compute_gui_scale(w, h, 0);
        let mut list = megu_client_ui::draw::UiDrawList::new();
        state.render(w as f32, h as f32, &mut list);
        let scales = emitted_text_scales(&list.commands);
        assert!(!scales.is_empty(), "subtitle HUD emitted no text at {w}x{h}");
        assert!(
            scales.iter().all(|&s| s == expected),
            "subtitle HUD text scale {scales:?} != canonical {expected} at {w}x{h}"
        );
    }
}
