// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client::protocol::{ClientPlayEvent, PlayEvent};
use megu_client_gpu::live_session::LiveSession;

#[test]
fn win_game_is_consumed_once_and_game_mode_updates_hud() {
    let mut session = LiveSession::offline_with_null_audio();
    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 3,
            value: 1.0,
        }))
        .unwrap();
    assert_eq!(session.hud.game_mode, 1);

    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 4,
            value: 1.0,
        }))
        .unwrap();
    assert!(session.take_pending_end_credits());
    assert!(!session.take_pending_end_credits());
}

#[test]
fn remote_block_break_overlay_tracks_stages_and_clear() {
    let mut session = LiveSession::offline_with_null_audio();
    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::BlockDestruction {
            breaker_eid: 7,
            pos: [1, 64, -2],
            stage: 4,
        }))
        .unwrap();
    assert_eq!(session.active_block_break_overlay(), Some(([1, 64, -2], 4)));

    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::BlockDestruction {
            breaker_eid: 7,
            pos: [1, 64, -2],
            stage: 10,
        }))
        .unwrap();
    assert_eq!(session.active_block_break_overlay(), None);
}

#[test]
fn gameplay_frame_projects_overlay_and_ui_click_is_safe_without_pose() {
    let mut session = LiveSession::offline_with_null_audio();
    session.tick_gameplay_frame(0.05);
    let overlay = session.gameplay_screen_overlay();
    assert_eq!(overlay.hurt_alpha, 0.0);
    session.play_ui_click();
}
