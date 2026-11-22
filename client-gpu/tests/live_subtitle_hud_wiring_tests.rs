// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Live subtitle wiring: drained spatial audio cues reach
//! `SubtitleHudState` and the in-game HUD draw list through
//! `record_frame_and_refresh_minihud` + `UiState::render`.

use megu_client::protocol::{ClientPlayEvent, PlayEvent, TeleportSync, WireSoundEvent};
use megu_client_gpu::live_session::LiveSession;
use megu_ui::draw::UiCommand;
use megu_ui::state::{Screen, UiState};
use megu_ui::subtitle_hud::SubtitleArrow;

fn teleport_session() -> LiveSession {
    let mut session = LiveSession::offline_with_null_audio();
    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
            x: -2.0,
            y: 2.0,
            z: 8.0,
            yaw: 0.0,
            pitch: 0.0,
            flags: 0,
            teleport_id: 1,
        })))
        .unwrap();
    session
}

fn sound_event() -> WireSoundEvent {
    WireSoundEvent {
        name: Some("minecraft:block.stone.break".to_string()),
        registry_id: None,
        category: 1,
        position: Some([18.0, 3.62, 8.0]),
        entity_id: None,
        volume: 1.0,
        pitch: 1.0,
        seed: 0,
        fixed_range: None,
    }
}

fn playing_ui() -> UiState {
    let mut ui = UiState::new();
    ui.navigate(Screen::InGameHud);
    ui.settings.qol.show_subtitles = true;
    ui
}

#[test]
fn drained_spatial_cues_reach_subtitle_hud_state() {
    let mut session = teleport_session();
    session.ingest_sound_event(sound_event());
    session.pump_audio_frames(480);

    let mut ui = playing_ui();
    // Mirror the window loop: clone the live HUD snapshot, then refresh.
    ui.hud = session.hud.clone();
    session.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);

    assert_eq!(ui.hud.subtitle_hud.len(), 1, "spatial cue feeds the HUD");
    let entry = &ui.hud.subtitle_hud.entries()[0];
    assert_eq!(entry.id, "minecraft:block.stone.break");
    assert_eq!(entry.label, "block stone break");
    // Source sits to the listener's left at yaw 0.
    assert_eq!(entry.arrow, SubtitleArrow::Left);
    assert_eq!(entry.ttl_secs, entry.max_ttl_secs);
    assert_eq!(entry.count, 1);
}

#[test]
fn repeat_sound_dedups_onto_one_row_and_ttl_ticks_down() {
    let mut session = teleport_session();
    session.ingest_sound_event(sound_event());
    session.ingest_sound_event(sound_event());
    session.pump_audio_frames(480);

    let mut ui = playing_ui();
    ui.hud = session.hud.clone();
    session.record_frame_and_refresh_minihud(0.5, &mut ui);
    assert_eq!(ui.hud.subtitle_hud.len(), 1);
    assert_eq!(ui.hud.subtitle_hud.entries()[0].count, 2);
    let first_ttl = ui.hud.subtitle_hud.entries()[0].ttl_secs;

    session.record_frame_and_refresh_minihud(1.0, &mut ui);
    let entry = &ui.hud.subtitle_hud.entries()[0];
    assert!(entry.ttl_secs < first_ttl, "frame clock drains the row");
    assert!((entry.ttl_secs - (first_ttl - 1.0)).abs() < 1e-4);
}

#[test]
fn subtitle_rows_appear_in_live_hud_draw_list() {
    let mut session = teleport_session();
    let mut ui = playing_ui();
    ui.hud = session.hud.clone();
    session.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);
    let baseline = ui.render(1280, 720);

    session.ingest_sound_event(sound_event());
    session.pump_audio_frames(480);
    session.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);
    let with_cue = ui.render(1280, 720);

    assert!(with_cue.commands.len() > baseline.commands.len());
    let has_subtitle_text = with_cue.commands.iter().any(|cmd| match cmd {
        UiCommand::Text { text, .. } => text.contains("block stone break"),
        _ => false,
    });
    assert!(
        has_subtitle_text,
        "subtitle row text lands in the draw list"
    );
}

#[test]
fn subtitles_stay_hidden_when_opt_in_is_off() {
    let mut session = teleport_session();
    session.ingest_sound_event(sound_event());
    session.pump_audio_frames(480);

    let mut ui = UiState::new();
    ui.navigate(Screen::InGameHud);
    ui.hud = session.hud.clone();
    session.record_frame_and_refresh_minihud(1.0 / 60.0, &mut ui);
    assert_eq!(ui.hud.subtitle_hud.len(), 1, "state still tracks cues");

    let list = ui.render(1280, 720);
    let has_subtitle_text = list.commands.iter().any(|cmd| match cmd {
        UiCommand::Text { text, .. } => text.contains("block stone break"),
        _ => false,
    });
    assert!(!has_subtitle_text, "opt-in gate keeps rows undrawn");
}
