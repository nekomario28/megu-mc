// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for the Controllable-class gamepad engine: radial deadzone math,
//! brand prompt glyphs and the Nintendo confirm-swap, hotbar wrap-around,
//! analog-trigger hysteresis, magnetic virtual-cursor navigation, D-pad
//! focus jumps, the 8-slice radial wheel, and the bounded rumble queue.

use megu_client_ui::draw::UiRect;
use megu_client_ui::gamepad::{
    process_radial_stick, radial_slice_for_angle, GamepadAction, GamepadAxis, GamepadBrand,
    GamepadButton, GamepadEvent, GamepadMapper, GamepadState, RadialWheelState, RumbleKind,
    StickConfig, UiContext, CURSOR_SPEED_PX_PER_SEC, HOTBAR_SLOTS, RUMBLE_QUEUE_MAX,
    SNAP_RADIUS_PX, TRIGGER_PRESS_THRESHOLD, TRIGGER_RELEASE_THRESHOLD,
};
use megu_client_ui::state::{HitRegion, UiLang, WidgetId};

const DT: f32 = 0.016;

fn frame() -> GamepadState {
    GamepadState::default()
}

fn with_button(btn: GamepadButton) -> GamepadState {
    let mut s = frame();
    s.set_button(btn, true);
    s
}

/// Press `btn` for one frame, then release for one frame; returns the
/// events emitted by the press frame.
fn tap(mapper: &mut GamepadMapper, btn: GamepadButton) -> Vec<GamepadEvent> {
    let events = mapper.update(&with_button(btn), DT, None);
    mapper.update(&frame(), DT, None); // release frame
    events
}

fn has_action(events: &[GamepadEvent], want: GamepadAction) -> bool {
    events
        .iter()
        .any(|e| matches!(e, GamepadEvent::ActionPressed(a) if *a == want))
}

// ---------------------------------------------------------------------------
// Radial deadzone / stick processing
// ---------------------------------------------------------------------------

#[test]
fn deadzone_outputs_zero_inside() {
    let cfg = StickConfig::default();
    assert_eq!(process_radial_stick(0.0, 0.0, &cfg), (0.0, 0.0));
    // Full deflection on one axis but inside the radial deadzone.
    assert_eq!(
        process_radial_stick(cfg.inner_deadzone * 0.9, 0.0, &cfg),
        (0.0, 0.0)
    );
    // Diagonal: each component below the deadzone, magnitude also below.
    let d = cfg.inner_deadzone * 0.6;
    assert_eq!(process_radial_stick(d, d, &cfg), (0.0, 0.0));
}

#[test]
fn deadzone_ramps_smoothly_and_monotonically() {
    let cfg = StickConfig {
        exponent: 1.0,
        ..StickConfig::default()
    };
    let mut prev = 0.0f32;
    let mut step = cfg.inner_deadzone + 0.01;
    while step <= 1.0 {
        let (x, y) = process_radial_stick(step, 0.0, &cfg);
        let mag = x.hypot(y);
        assert!(
            mag >= prev,
            "output must be non-decreasing: mag {step} -> {mag} < {prev}"
        );
        prev = mag;
        step += 0.05;
    }
    // Just above the deadzone the output is a small fraction, not a snap
    // to a cardinal or a jump to a large value.
    let (x, _) = process_radial_stick(cfg.inner_deadzone + 0.02, 0.0, &cfg);
    assert!(x > 0.0 && x < 0.2, "edge-of-deadzone output was {x}");
    // Full deflection saturates at exactly 1.
    let (x, _) = process_radial_stick(1.0, 0.0, &cfg);
    assert!((x - 1.0).abs() < 1e-6);
}

#[test]
fn stick_processing_fails_closed_on_non_finite() {
    let cfg = StickConfig::default();
    for (x, y) in [
        (f32::NAN, 0.5),
        (0.5, f32::NAN),
        (f32::INFINITY, 0.0),
        (0.0, f32::NEG_INFINITY),
        (f32::NAN, f32::NAN),
    ] {
        assert_eq!(process_radial_stick(x, y, &cfg), (0.0, 0.0));
    }
    // A hostile raw frame produces a zero Move and no Look, never a panic.
    let mut mapper = GamepadMapper::default();
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, f32::NAN);
    raw.set_axis(GamepadAxis::RightY, f32::INFINITY);
    let events = mapper.update(&raw, DT, None);
    let moved = events.iter().any(|e| {
        matches!(e, GamepadEvent::Move { forward, strafe, .. }
            if *forward != 0.0 || *strafe != 0.0)
    });
    assert!(!moved, "NaN stick produced movement: {events:?}");
    assert!(!events.iter().any(|e| matches!(e, GamepadEvent::Look { .. })));
}

#[test]
fn exponent_and_invert_are_applied() {
    let base = StickConfig {
        exponent: 1.0,
        ..StickConfig::default()
    };
    let curved = StickConfig {
        exponent: 3.0,
        ..base
    };
    // Half-way deflection: the steeper curve outputs strictly less.
    let (lin, _) = process_radial_stick(0.6, 0.0, &base);
    let (exp, _) = process_radial_stick(0.6, 0.0, &curved);
    assert!(exp < lin && exp > 0.0, "exponent curve: {exp} vs {lin}");
    // Y-invert mirrors the vertical output only.
    let inv = StickConfig {
        invert_y: true,
        ..base
    };
    let (x, y) = process_radial_stick(0.0, -0.8, &inv);
    assert!((x - 0.0).abs() < 1e-6 && y > 0.0);
}

#[test]
fn right_stick_drives_camera_look() {
    let mut mapper = GamepadMapper::default();
    let mut raw = frame();
    raw.set_axis(GamepadAxis::RightX, 1.0);
    let events = mapper.update(&raw, 0.1, None);
    let look = events
        .iter()
        .find_map(|e| match e {
            GamepadEvent::Look {
                yaw_delta_deg,
                pitch_delta_deg,
            } => Some((*yaw_delta_deg, *pitch_delta_deg)),
            _ => None,
        })
        .expect("full right-stick deflection must emit Look");
    assert!(look.0 > 0.0, "rightward stick must turn right (yaw+)");
    assert_eq!(look.1, 0.0);
}

#[test]
fn left_stick_drives_movement_with_sprint_click() {
    let mut mapper = GamepadMapper::default();
    let mut raw = with_button(GamepadButton::LeftStick);
    raw.set_axis(GamepadAxis::LeftY, -1.0); // stick up = forward
    raw.set_axis(GamepadAxis::LeftX, 0.3);
    let events = mapper.update(&raw, DT, None);
    let mv = events
        .iter()
        .find_map(|e| match e {
            GamepadEvent::Move {
                forward,
                strafe,
                sprint,
            } => Some((*forward, *strafe, *sprint)),
            _ => None,
        })
        .expect("Move is emitted every gameplay frame");
    assert!(mv.0 > 0.9, "stick up is forward: {mv:?}");
    assert!(mv.1 > 0.0, "stick right is strafe right: {mv:?}");
    assert!(mv.2, "L3 click must report sprint");
}

// ---------------------------------------------------------------------------
// Prompts, bilingual names, confirm-swap
// ---------------------------------------------------------------------------

#[test]
fn per_brand_prompt_labels() {
    assert_eq!(GamepadButton::South.prompt_label(GamepadBrand::Xbox), "A");
    assert_eq!(GamepadButton::East.prompt_label(GamepadBrand::Xbox), "B");
    assert_eq!(
        GamepadButton::South.prompt_label(GamepadBrand::PlayStation),
        "Cross"
    );
    assert_eq!(
        GamepadButton::East.prompt_label(GamepadBrand::PlayStation),
        "Circle"
    );
    assert_eq!(
        GamepadButton::West.prompt_label(GamepadBrand::PlayStation),
        "Square"
    );
    assert_eq!(
        GamepadButton::North.prompt_label(GamepadBrand::PlayStation),
        "Triangle"
    );
    // Nintendo positions: the physical south button is labeled B.
    assert_eq!(GamepadButton::South.prompt_label(GamepadBrand::Switch), "B");
    assert_eq!(GamepadButton::East.prompt_label(GamepadBrand::Switch), "A");
    assert_eq!(
        GamepadButton::South.prompt_label(GamepadBrand::SteamDeck),
        "A"
    );
    assert_eq!(
        GamepadButton::Select.prompt_label(GamepadBrand::SteamDeck),
        "STEAM"
    );
}

#[test]
fn action_names_are_bilingual() {
    assert_eq!(GamepadAction::Jump.name(UiLang::En), "Jump");
    assert_eq!(GamepadAction::Jump.name(UiLang::Ja), "ジャンプ");
    assert_ne!(
        GamepadAction::UiQuickMove.name(UiLang::En),
        GamepadAction::UiQuickMove.name(UiLang::Ja)
    );
}

#[test]
fn brand_detection_uses_usb_ids_then_name() {
    assert_eq!(GamepadBrand::detect(0x045E, 0, ""), GamepadBrand::Xbox);
    assert_eq!(GamepadBrand::detect(0x054C, 0, ""), GamepadBrand::PlayStation);
    assert_eq!(GamepadBrand::detect(0x057E, 0, ""), GamepadBrand::Switch);
    assert_eq!(GamepadBrand::detect(0x28DE, 0, ""), GamepadBrand::SteamDeck);
    assert_eq!(
        GamepadBrand::detect(0, 0, "Sony DualSense Wireless"),
        GamepadBrand::PlayStation
    );
    assert_eq!(
        GamepadBrand::detect(0, 0, "Nintendo Switch Pro Controller"),
        GamepadBrand::Switch
    );
    assert_eq!(GamepadBrand::detect(0, 0, "mystery pad"), GamepadBrand::Xbox);
}

#[test]
fn switch_confirm_swap_vs_xbox() {
    let mut xbox = GamepadMapper::new(GamepadBrand::Xbox);
    xbox.ui_mode = true;
    assert_eq!(xbox.confirm_button(), GamepadButton::South);
    assert!(has_action(
        &tap(&mut xbox, GamepadButton::South),
        GamepadAction::UiConfirm
    ));
    assert!(has_action(
        &tap(&mut xbox, GamepadButton::East),
        GamepadAction::UiCancel
    ));

    let mut switch = GamepadMapper::new(GamepadBrand::Switch);
    switch.ui_mode = true;
    assert_eq!(switch.confirm_button(), GamepadButton::East);
    // Physical east (`A`) confirms, physical south (`B`) cancels.
    assert!(has_action(
        &tap(&mut switch, GamepadButton::East),
        GamepadAction::UiConfirm
    ));
    assert!(has_action(
        &tap(&mut switch, GamepadButton::South),
        GamepadAction::UiCancel
    ));

    // Swap disabled: Nintendo falls back to positional semantics.
    let mut switch = GamepadMapper::new(GamepadBrand::Switch);
    switch.nintendo_confirm_swap = false;
    switch.ui_mode = true;
    assert_eq!(switch.confirm_button(), GamepadButton::South);
    assert!(has_action(
        &tap(&mut switch, GamepadButton::South),
        GamepadAction::UiConfirm
    ));

    // The swap also drives gameplay binds (east `A` = jump on Nintendo).
    let mut gp = GamepadMapper::new(GamepadBrand::Switch);
    assert!(has_action(
        &tap(&mut gp, GamepadButton::East),
        GamepadAction::Jump
    ));
    assert!(has_action(
        &tap(&mut gp, GamepadButton::South),
        GamepadAction::DropOrBack
    ));
}

// ---------------------------------------------------------------------------
// Hotbar wrap-around and trigger hysteresis
// ---------------------------------------------------------------------------

#[test]
fn hotbar_bumpers_wrap_around() {
    let mut mapper = GamepadMapper::default();
    assert_eq!(mapper.hotbar_slot(), 0);
    // LB from slot 0 wraps to slot 8.
    let events = tap(&mut mapper, GamepadButton::LeftShoulder);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, GamepadEvent::HotbarChanged(8))),
        "LB at slot 0 must wrap to 8: {events:?}"
    );
    assert_eq!(mapper.hotbar_slot(), HOTBAR_SLOTS - 1);
    // RB wraps 8 back to 0.
    tap(&mut mapper, GamepadButton::RightShoulder);
    assert_eq!(mapper.hotbar_slot(), 0);
    // Walk the whole wheel forward and back.
    for want in 1..HOTBAR_SLOTS {
        tap(&mut mapper, GamepadButton::RightShoulder);
        assert_eq!(mapper.hotbar_slot(), want);
    }
    tap(&mut mapper, GamepadButton::RightShoulder);
    assert_eq!(mapper.hotbar_slot(), 0);
}

#[test]
fn trigger_hysteresis_prevents_flicker() {
    let mut mapper = GamepadMapper::default();
    let mut raw = frame();
    const {
        assert!(TRIGGER_PRESS_THRESHOLD > TRIGGER_RELEASE_THRESHOLD);
    }

    // Rising edge past the press threshold fires exactly once.
    raw.set_axis(GamepadAxis::LeftTrigger, TRIGGER_PRESS_THRESHOLD + 0.02);
    let events = mapper.update(&raw, DT, None);
    assert!(has_action(&events, GamepadAction::UseItem));

    // Hovering inside the hysteresis band must not re-fire or release.
    for v in [
        TRIGGER_PRESS_THRESHOLD - 0.01,
        TRIGGER_RELEASE_THRESHOLD + 0.01,
        TRIGGER_PRESS_THRESHOLD + 0.01,
    ] {
        raw.set_axis(GamepadAxis::LeftTrigger, v);
        let events = mapper.update(&raw, DT, None);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, GamepadEvent::ActionPressed(_)
                    | GamepadEvent::ActionReleased(_))),
            "hysteresis band value {v} emitted an event: {events:?}"
        );
    }

    // Falling below the release threshold releases — once.
    raw.set_axis(GamepadAxis::LeftTrigger, TRIGGER_RELEASE_THRESHOLD - 0.01);
    let events = mapper.update(&raw, DT, None);
    assert!(events
        .iter()
        .any(|e| matches!(e, GamepadEvent::ActionReleased(GamepadAction::UseItem))));
    let events = mapper.update(&raw, DT, None);
    assert!(events
        .iter()
        .all(|e| !matches!(e, GamepadEvent::ActionReleased(_))));
}

// ---------------------------------------------------------------------------
// UI navigation: virtual cursor + D-pad focus jumps
// ---------------------------------------------------------------------------

fn grid_regions() -> Vec<HitRegion> {
    // 3x3 grid, 40x40 cells at 100px pitch: centers (50,50), (150,50), ...
    let mut regions = Vec::new();
    for row in 0..3usize {
        for col in 0..3usize {
            regions.push(HitRegion::new(
                WidgetId::ControlKey(row * 3 + col),
                UiRect::new(30 + col as i32 * 100, 30 + row as i32 * 100, 40, 40),
            ));
        }
    }
    regions
}

fn ui_ctx(regions: &[HitRegion]) -> UiContext<'_> {
    UiContext {
        hit_regions: regions,
        screen_w: 640.0,
        screen_h: 360.0,
    }
}

#[test]
fn virtual_cursor_moves_and_clamps_to_screen() {
    let regions = grid_regions();
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, 1.0);

    // Full deflection moves at cursor speed (no snap target in reach of
    // the origin).
    let events = mapper.update(&raw, DT, Some(ui_ctx(&regions)));
    let (x, _) = mapper.cursor();
    let expected = CURSOR_SPEED_PX_PER_SEC * DT;
    assert!(
        (x - expected).abs() < 1e-4,
        "free cursor should move {expected}px, got {x} ({events:?})"
    );

    // Pushing up-left from the origin clamps into the screen corner.
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, -1.0);
    raw.set_axis(GamepadAxis::LeftY, -1.0);
    for _ in 0..10 {
        mapper.update(&raw, DT, Some(ui_ctx(&regions)));
    }
    let (x, y) = mapper.cursor();
    assert_eq!((x, y), (0.0, 0.0));

    // UI mode emits cursor events, not locomotion.
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, 1.0);
    let events = mapper.update(&raw, DT, Some(ui_ctx(&regions)));
    assert!(!events
        .iter()
        .any(|e| matches!(e, GamepadEvent::Move { .. })));
}

#[test]
fn virtual_cursor_snaps_magnetically_over_regions() {
    // One slot at screen center (200,200): drive diagonally until the
    // cursor sits inside its snap radius up-left of the center.
    let regions = vec![HitRegion::new(
        WidgetId::ControlKey(0),
        UiRect::new(180, 180, 40, 40),
    )];
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;

    {
        let mut raw = frame();
        raw.set_axis(GamepadAxis::LeftX, 1.0);
        raw.set_axis(GamepadAxis::LeftY, 1.0);
        while mapper.cursor().0 < 190.0 {
            mapper.update(&raw, DT, Some(ui_ctx(&regions)));
        }
    }
    let (start_x, start_y) = mapper.cursor();
    let dist = (200.0 - start_x).hypot(200.0 - start_y);
    assert!(
        dist < SNAP_RADIUS_PX,
        "cursor must start inside the snap radius: {dist}px from center"
    );

    // Push straight UP. A free cursor would keep x fixed and travel the
    // full cursor speed; the magnet must drag x toward the center and
    // cover strictly less ground.
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftY, -1.0);
    mapper.update(&raw, DT, Some(ui_ctx(&regions)));
    let (x, _) = mapper.cursor();
    assert!(
        x > start_x,
        "magnetic pull must drag the cursor toward the slot center \
         even though the stick pushes straight up ({start_x} -> {x})"
    );

    let free_dy = CURSOR_SPEED_PX_PER_SEC * DT;
    let dy = start_y - mapper.cursor().1;
    assert!(
        dy > 0.0 && dy < free_dy,
        "cursor should slow over the slot: moved {dy}, free move {free_dy}"
    );
}

#[test]
fn dpad_focus_jump_walks_the_grid() {
    let regions = grid_regions();
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let ctx = ui_ctx(&regions);

    // From the origin, D-pad right lands on the nearest in-cone region:
    // index 0 at center (50,50).
    let events = mapper.update(&with_button(GamepadButton::DpadRight), DT, Some(ctx));
    assert!(events.iter().any(|e| matches!(
        e,
        GamepadEvent::FocusJumped { region_index: 0 }
    )));
    assert_eq!(mapper.focused_region(), Some(0));
    assert_eq!(mapper.cursor(), (50.0, 50.0));
    mapper.update(&frame(), DT, None);

    // Right again walks to (150,50), then down to (150,150).
    mapper.update(&with_button(GamepadButton::DpadRight), DT, Some(ctx));
    assert_eq!(mapper.focused_region(), Some(1));
    mapper.update(&frame(), DT, None);
    mapper.update(&with_button(GamepadButton::DpadDown), DT, Some(ctx));
    assert_eq!(mapper.focused_region(), Some(4));
    assert_eq!(mapper.cursor(), (150.0, 150.0));
    mapper.update(&frame(), DT, None);

    // No region exists further right in the bottom row's cone from a
    // middle-cell start? (250,150) IS right of (150,150) — walks there.
    mapper.update(&with_button(GamepadButton::DpadRight), DT, Some(ctx));
    assert_eq!(mapper.focused_region(), Some(5));
    mapper.update(&frame(), DT, None);

    // From the right edge nothing further right exists: no jump.
    let events = mapper.update(&with_button(GamepadButton::DpadRight), DT, Some(ctx));
    assert!(!events
        .iter()
        .any(|e| matches!(e, GamepadEvent::FocusJumped { .. })));
}

#[test]
fn ui_mode_maps_container_buttons() {
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    assert!(has_action(
        &tap(&mut mapper, GamepadButton::North),
        GamepadAction::UiQuickMove
    ));
    assert!(has_action(
        &tap(&mut mapper, GamepadButton::West),
        GamepadAction::UiSplitStack
    ));
    assert!(has_action(
        &tap(&mut mapper, GamepadButton::LeftShoulder),
        GamepadAction::UiTabPrev
    ));
    assert!(has_action(
        &tap(&mut mapper, GamepadButton::RightShoulder),
        GamepadAction::UiTabNext
    ));
}

// ---------------------------------------------------------------------------
// Radial wheel
// ---------------------------------------------------------------------------

#[test]
fn radial_slices_cover_all_360_degrees() {
    // Exact sector selection: slice i owns [i*45 - 22.5, i*45 + 22.5).
    assert_eq!(radial_slice_for_angle(0.0), 0);
    assert_eq!(radial_slice_for_angle(22.4), 0);
    assert_eq!(radial_slice_for_angle(22.5), 1);
    assert_eq!(radial_slice_for_angle(67.4), 1);
    assert_eq!(radial_slice_for_angle(67.5), 2);
    assert_eq!(radial_slice_for_angle(90.0), 2);
    assert_eq!(radial_slice_for_angle(180.0), 4);
    assert_eq!(radial_slice_for_angle(270.0), 6);
    assert_eq!(radial_slice_for_angle(337.4), 7);
    // The seam wraps: 337.5° and negative angles land on slice 0.
    assert_eq!(radial_slice_for_angle(337.5), 0);
    assert_eq!(radial_slice_for_angle(-10.0), 0);
    assert_eq!(radial_slice_for_angle(360.0), 0);
    assert_eq!(radial_slice_for_angle(-90.0), 6);
    // Every angle lands on a valid slice.
    let mut deg = 0.0;
    while deg < 360.0 {
        assert!(radial_slice_for_angle(deg) < 8);
        deg += 0.5;
    }
}

#[test]
fn radial_wheel_tracks_stick_angle_and_neutral() {
    let mut wheel = RadialWheelState {
        open: true,
        selected: None,
    };
    // Stick right = east = slice 0; stick up = north = slice 2.
    wheel.update(1.0, 0.0, 0.2);
    assert_eq!(wheel.selected, Some(0));
    wheel.update(0.0, -1.0, 0.2);
    assert_eq!(wheel.selected, Some(2));
    // Inside the deadzone the selection neutralizes.
    wheel.update(0.1, 0.1, 0.2);
    assert_eq!(wheel.selected, None);
    assert_eq!(wheel.selected_action(), None);
    // Non-finite input also neutralizes.
    wheel.update(f32::NAN, -1.0, 0.2);
    assert_eq!(wheel.selected, None);
}

#[test]
fn select_opens_wheel_right_stick_selects_release_confirms() {
    let mut mapper = GamepadMapper::default();

    // Hold Select: wheel opens.
    let events = mapper.update(&with_button(GamepadButton::Select), DT, None);
    assert!(events
        .iter()
        .any(|e| matches!(e, GamepadEvent::RadialOpened)));
    assert!(mapper.radial().open);

    // Aim the right stick up while Select is held: selection lands on
    // the north slice and no Look event leaks through.
    let mut raw = with_button(GamepadButton::Select);
    raw.set_axis(GamepadAxis::RightY, -1.0);
    let events = mapper.update(&raw, DT, None);
    assert_eq!(mapper.radial().selected, Some(2));
    assert!(!events.iter().any(|e| matches!(e, GamepadEvent::Look { .. })));

    // Release: the chosen slice is reported and the wheel closes.
    let events = mapper.update(&frame(), DT, None);
    assert!(events.iter().any(
        |e| matches!(e, GamepadEvent::RadialClosed { selected: Some(2) })
    ));
    assert!(!mapper.radial().open);
}

// ---------------------------------------------------------------------------
// Rumble queue
// ---------------------------------------------------------------------------

#[test]
fn rumble_respects_toggle_strength_and_bounds() {
    let mut mapper = GamepadMapper::default();

    // Disabled: nothing is queued.
    mapper.rumble_enabled = false;
    mapper.emit_rumble(RumbleKind::Damage);
    assert_eq!(mapper.pending_rumble(), 0);
    assert_eq!(mapper.poll_rumble(), None);

    // Enabled at half strength: amplitudes scale and stay in 0..=1.
    mapper.rumble_enabled = true;
    mapper.rumble_strength = 0.5;
    mapper.emit_rumble(RumbleKind::CriticalHit);
    let cmd = mapper.poll_rumble().expect("command must be queued");
    assert!((cmd.low_freq - 0.5).abs() < 1e-6);
    assert!((cmd.high_freq - 0.5).abs() < 1e-6);
    assert_eq!(cmd.duration_ms, 200);

    // Over-unity strength clamps: never emits > 1.0.
    mapper.rumble_strength = 2.0;
    mapper.emit_rumble(RumbleKind::CriticalHit);
    let cmd = mapper.poll_rumble().unwrap();
    assert!(cmd.low_freq <= 1.0 && cmd.high_freq <= 1.0);

    // Zero strength is silently dropped; NaN strength fails closed too.
    for bad in [0.0, f32::NAN] {
        mapper.rumble_strength = bad;
        mapper.emit_rumble(RumbleKind::BlockBreak);
        assert_eq!(mapper.poll_rumble(), None);
    }

    // The queue is bounded.
    mapper.rumble_strength = 1.0;
    for _ in 0..(RUMBLE_QUEUE_MAX * 3) {
        mapper.emit_rumble(RumbleKind::BlockBreak);
    }
    assert!(mapper.pending_rumble() <= RUMBLE_QUEUE_MAX);
}
