// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_gpu::window::{WindowInputEvent, WindowInputRouter};
use megu_kernel::debug_chords::{
    DebugChordAction, DebugTarget, F3Chord, GameMode, PlayerDebugInfo, ALL_F3_CHORDS,
};
use winit::event::ElementState;
use winit::keyboard::KeyCode;

fn press(router: &mut WindowInputRouter, key: KeyCode) -> Vec<WindowInputEvent> {
    router.route_key(key, ElementState::Pressed, false)
}

fn release(router: &mut WindowInputRouter, key: KeyCode) -> Vec<WindowInputEvent> {
    router.route_key(key, ElementState::Released, false)
}

#[test]
fn f3_g_h_b_and_help_consume_f3_without_toggling_base_overlay() {
    let mut router = WindowInputRouter::default();
    for (key, flag) in [
        (KeyCode::KeyG, "borders"),
        (KeyCode::KeyH, "tooltips"),
        (KeyCode::KeyB, "hitboxes"),
    ] {
        assert!(press(&mut router, KeyCode::F3).is_empty());
        assert!(matches!(
            press(&mut router, key).as_slice(),
            [WindowInputEvent::DebugChord(
                DebugChordAction::ChatFeedback(_)
            )]
        ));
        assert!(router.f3_chord_consumed);
        assert!(release(&mut router, KeyCode::F3).is_empty());
        assert!(!router.debug_chords.show_debug_overlay);
        match flag {
            "borders" => assert!(router.debug_chords.show_chunk_borders),
            "tooltips" => assert!(router.debug_chords.advanced_tooltips),
            "hitboxes" => assert!(router.debug_chords.show_hitboxes),
            _ => unreachable!(),
        }
    }

    press(&mut router, KeyCode::F3);
    let result = press(&mut router, KeyCode::KeyQ);
    let [WindowInputEvent::DebugChord(DebugChordAction::HelpLines(lines))] = result.as_slice()
    else {
        panic!("F3+Q must emit help, never drop an item");
    };
    assert_eq!(lines.len(), 15);
    assert_eq!(
        lines,
        &ALL_F3_CHORDS
            .iter()
            .map(|chord| chord.help_line().to_owned())
            .collect::<Vec<_>>()
    );
    assert!(release(&mut router, KeyCode::F3).is_empty());

    assert!(press(&mut router, KeyCode::F3).is_empty());
    assert_eq!(
        release(&mut router, KeyCode::F3),
        vec![WindowInputEvent::SetDebugOverlayVisible(true)]
    );
    assert!(router.debug_chords.show_debug_overlay);
    press(&mut router, KeyCode::F3);
    assert_eq!(
        release(&mut router, KeyCode::F3),
        vec![WindowInputEvent::SetDebugOverlayVisible(false)]
    );
}

#[test]
fn every_vanilla_f3_chord_routes_to_debug_kernel() {
    let chords = [
        (KeyCode::KeyA, F3Chord::A),
        (KeyCode::KeyB, F3Chord::B),
        (KeyCode::KeyC, F3Chord::C),
        (KeyCode::KeyD, F3Chord::D),
        (KeyCode::F4, F3Chord::F4),
        (KeyCode::KeyG, F3Chord::G),
        (KeyCode::KeyH, F3Chord::H),
        (KeyCode::KeyI, F3Chord::I),
        (KeyCode::KeyN, F3Chord::N),
        (KeyCode::KeyP, F3Chord::P),
        (KeyCode::KeyQ, F3Chord::Q),
        (KeyCode::KeyT, F3Chord::T),
        (KeyCode::Digit1, F3Chord::Key1),
        (KeyCode::Digit2, F3Chord::Key2),
        (KeyCode::Digit3, F3Chord::Key3),
    ];
    assert_eq!(chords.len(), ALL_F3_CHORDS.len());
    for ((key, chord), expected_chord) in chords.into_iter().zip(ALL_F3_CHORDS) {
        assert_eq!(chord, expected_chord);
        let mut router = WindowInputRouter::default();
        press(&mut router, KeyCode::F3);
        let result = press(&mut router, key);
        assert!(
            matches!(result.as_slice(), [WindowInputEvent::DebugChord(_)]),
            "F3+{key:?} must emit DebugChord"
        );
        assert!(router.f3_chord_consumed);
        assert!(release(&mut router, KeyCode::F3).is_empty());
        assert_eq!(router.active_hotbar_slot, 0);
        assert!(!router.f3_held);
    }

    let mut router = WindowInputRouter::default();
    press(&mut router, KeyCode::F3);
    press(&mut router, KeyCode::Digit1);
    assert!(router.debug_chords.pie_chart);
    assert!(router.debug_chords.show_debug_overlay);
    assert!(release(&mut router, KeyCode::F3).is_empty());
    press(&mut router, KeyCode::F3);
    press(&mut router, KeyCode::Digit2);
    assert!(router.debug_chords.fps_tps_chart);
    press(&mut router, KeyCode::Digit3);
    assert!(router.debug_chords.net_chart);
    assert!(!router.debug_chords.fps_tps_chart);
}

#[test]
fn f3_context_and_switcher_finish_are_supported_without_a_window() {
    let player = PlayerDebugInfo {
        dimension: "minecraft:overworld",
        position: [1.0, 64.0, 2.0],
        yaw_deg: 90.0,
        pitch_deg: 5.0,
        gamemode: GameMode::Survival,
    };
    let mut router = WindowInputRouter::default();
    press(&mut router, KeyCode::F3);
    assert!(matches!(
        router.route_key_with_context(
            KeyCode::KeyC,
            ElementState::Pressed,
            false,
            Some(player),
            None,
            false,
        ).as_slice(),
        [WindowInputEvent::DebugChord(DebugChordAction::ClipboardCommand(command))]
            if command.contains("/execute in minecraft:overworld run tp")
    ));

    assert!(matches!(
        router.route_key_with_context(
            KeyCode::KeyI,
            ElementState::Pressed,
            false,
            Some(player),
            Some(DebugTarget::Block {
                position: [1, 64, 2],
                block_state: "minecraft:stone",
                nbt: None,
            }),
            false,
        ).as_slice(),
        [WindowInputEvent::DebugChord(DebugChordAction::ClipboardCommand(command))]
            if command.starts_with("/setblock 1 64 2 minecraft:stone")
    ));
    assert!(matches!(
        router
            .route_key_with_context(
                KeyCode::F4,
                ElementState::Pressed,
                false,
                Some(player),
                None,
                true,
            )
            .as_slice(),
        [WindowInputEvent::DebugChord(
            DebugChordAction::ChatFeedback(_)
        )]
    ));
    assert_eq!(
        router.route_key_with_context(
            KeyCode::F3,
            ElementState::Released,
            false,
            Some(player),
            None,
            true,
        ),
        vec![WindowInputEvent::DebugChord(
            DebugChordAction::SendGameModeCommand(GameMode::Creative)
        )]
    );
    assert!(!router.debug_chords.show_debug_overlay);
    assert_eq!(router.debug_chords.gamemode_switcher, None);
}

#[test]
fn all_gameplay_bindings_emit_exact_events() {
    let mut router = WindowInputRouter::default();
    let digits = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    for (slot, key) in digits.into_iter().enumerate() {
        assert_eq!(
            press(&mut router, key),
            vec![WindowInputEvent::SelectHotbar(slot as u8)]
        );
        assert_eq!(router.active_hotbar_slot, slot as u8);
        assert!(release(&mut router, key).is_empty());
    }

    for (key, expected) in [
        (KeyCode::KeyF, WindowInputEvent::SwapOffhand),
        (
            KeyCode::KeyQ,
            WindowInputEvent::DropItem {
                entire_stack: false,
            },
        ),
        (KeyCode::KeyE, WindowInputEvent::OpenInventory),
        (
            KeyCode::KeyT,
            WindowInputEvent::OpenChat {
                slash_prefill: false,
            },
        ),
        (
            KeyCode::Slash,
            WindowInputEvent::OpenChat {
                slash_prefill: true,
            },
        ),
        (KeyCode::KeyR, WindowInputEvent::SortInventory),
        (KeyCode::KeyL, WindowInputEvent::OpenAdvancements),
    ] {
        assert_eq!(press(&mut router, key), vec![expected]);
        assert!(release(&mut router, key).is_empty());
    }
    assert_eq!(
        router.route_key(KeyCode::KeyQ, ElementState::Pressed, true),
        vec![WindowInputEvent::DropItem { entire_stack: true }]
    );
    assert_eq!(
        press(&mut router, KeyCode::Tab),
        vec![WindowInputEvent::SetTabListVisible(true)]
    );
    assert!(router.tab_list_held);
    assert_eq!(
        release(&mut router, KeyCode::Tab),
        vec![WindowInputEvent::SetTabListVisible(false)]
    );
    assert!(!router.tab_list_held);
    assert!(release(&mut router, KeyCode::Tab).is_empty());
}

#[test]
fn f3_priority_and_focus_loss_preserve_other_key_paths() {
    let mut router = WindowInputRouter::default();
    for key in [KeyCode::KeyQ, KeyCode::KeyT, KeyCode::Digit3] {
        press(&mut router, KeyCode::F3);
        assert!(matches!(
            press(&mut router, key).as_slice(),
            [WindowInputEvent::DebugChord(_)]
        ));
        assert!(release(&mut router, KeyCode::F3).is_empty());
    }
    assert!(press(&mut router, KeyCode::KeyW).is_empty());
    assert!(press(&mut router, KeyCode::Space).is_empty());
    assert!(press(&mut router, KeyCode::ShiftLeft).is_empty());
    assert!(press(&mut router, KeyCode::F5).is_empty());
    assert!(press(&mut router, KeyCode::KeyC).is_empty());
    assert!(press(&mut router, KeyCode::Escape).is_empty());

    press(&mut router, KeyCode::Tab);
    press(&mut router, KeyCode::F3);
    assert_eq!(
        router.release_held_keys(),
        Some(WindowInputEvent::SetTabListVisible(false))
    );
    assert!(!router.f3_held);
    assert!(!router.tab_list_held);
    assert!(release(&mut router, KeyCode::F3).is_empty());
}
