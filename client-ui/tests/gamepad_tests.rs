// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for the Controllable-class gamepad engine: radial deadzone math,
//! brand prompt glyphs and the Nintendo confirm-swap, hotbar wrap-around,
//! analog-trigger hysteresis, magnetic virtual-cursor navigation, D-pad
//! focus jumps, the 8-slice radial wheel, and the bounded rumble queue.

use megu_client_ui::draw::UiRect;
use megu_client_ui::gamepad::{
    build_context_hints, process_radial_stick, radial_slice_for_angle, AnalogMovementMode,
    BadgeShape, GamepadAction, GamepadAxis, GamepadBrand, GamepadButton, GamepadEvent,
    GamepadMapper, GamepadState, HintContext, InputModeTracker, InputSourceMode,
    RadialWheelState, RumbleKind, StickConfig, UiContext, CURSOR_SPEED_PX_PER_SEC,
    HOTBAR_SLOTS, RADIAL_ACTIONS, RUMBLE_QUEUE_MAX, SNAP_RADIUS_PX, TRIGGER_PRESS_THRESHOLD,
    TRIGGER_RELEASE_THRESHOLD, UI_SCROLL_TICKS_PER_SEC,
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
    // Xbox: letters + geometric menu glyphs + short D-pad arrows.
    assert_eq!(GamepadButton::South.prompt_label(GamepadBrand::Xbox), "A");
    assert_eq!(GamepadButton::East.prompt_label(GamepadBrand::Xbox), "B");
    assert_eq!(GamepadButton::Start.prompt_label(GamepadBrand::Xbox), "☰");
    assert_eq!(GamepadButton::Select.prompt_label(GamepadBrand::Xbox), "⧉");
    assert_eq!(GamepadButton::DpadUp.prompt_label(GamepadBrand::Xbox), "↑");
    assert_eq!(GamepadButton::DpadDown.prompt_label(GamepadBrand::Xbox), "↓");
    assert_eq!(GamepadButton::DpadLeft.prompt_label(GamepadBrand::Xbox), "←");
    assert_eq!(GamepadButton::DpadRight.prompt_label(GamepadBrand::Xbox), "→");

    // PlayStation: natural symbols, never English words.
    assert_eq!(GamepadButton::South.prompt_label(GamepadBrand::PlayStation), "✕");
    assert_eq!(GamepadButton::East.prompt_label(GamepadBrand::PlayStation), "○");
    assert_eq!(GamepadButton::West.prompt_label(GamepadBrand::PlayStation), "□");
    assert_eq!(GamepadButton::North.prompt_label(GamepadBrand::PlayStation), "△");
    assert_eq!(GamepadButton::DpadUp.prompt_label(GamepadBrand::PlayStation), "↑");

    // Nintendo positions: the physical south button is labeled B; the
    // Select glyph is U+2212, not an ASCII hyphen.
    assert_eq!(GamepadButton::South.prompt_label(GamepadBrand::Switch), "B");
    assert_eq!(GamepadButton::East.prompt_label(GamepadBrand::Switch), "A");
    assert_eq!(GamepadButton::Start.prompt_label(GamepadBrand::Switch), "+");
    assert_eq!(GamepadButton::Select.prompt_label(GamepadBrand::Switch), "−");
    assert_ne!(GamepadButton::Select.prompt_label(GamepadBrand::Switch), "-");
    assert_eq!(GamepadButton::Guide.prompt_label(GamepadBrand::Switch), "HOME");
    assert_eq!(GamepadButton::LeftStick.prompt_label(GamepadBrand::Switch), "LS");
    assert_eq!(GamepadButton::DpadRight.prompt_label(GamepadBrand::Switch), "→");

    // Steam Deck: ☰ menu, ⧉ view, STEAM guide — never "..." or
    // duplicated STEAM for Select.
    assert_eq!(GamepadButton::South.prompt_label(GamepadBrand::SteamDeck), "A");
    assert_eq!(GamepadButton::Start.prompt_label(GamepadBrand::SteamDeck), "☰");
    assert_eq!(GamepadButton::Select.prompt_label(GamepadBrand::SteamDeck), "⧉");
    assert_eq!(GamepadButton::Guide.prompt_label(GamepadBrand::SteamDeck), "STEAM");
    assert_ne!(GamepadButton::Start.prompt_label(GamepadBrand::SteamDeck), "...");
    assert_ne!(
        GamepadButton::Select.prompt_label(GamepadBrand::SteamDeck),
        GamepadButton::Guide.prompt_label(GamepadBrand::SteamDeck)
    );
}

#[test]
fn badge_specs_are_geometric_per_brand() {
    // Shapes follow the physical control.
    let badge = GamepadButton::South.badge_spec(GamepadBrand::Xbox);
    assert_eq!(badge.shape, BadgeShape::Circle);
    assert_eq!(badge.symbol, "A");
    assert!(badge.fill_rgba[1] > badge.fill_rgba[0], "A badge is green");
    assert_eq!(
        GamepadButton::LeftShoulder
            .badge_spec(GamepadBrand::Xbox)
            .shape,
        BadgeShape::Bumper
    );
    assert_eq!(
        GamepadButton::RightTrigger
            .badge_spec(GamepadBrand::Xbox)
            .shape,
        BadgeShape::Trigger
    );
    assert_eq!(
        GamepadButton::Start.badge_spec(GamepadBrand::Xbox).shape,
        BadgeShape::Pill
    );
    assert_eq!(
        GamepadButton::DpadUp.badge_spec(GamepadBrand::Xbox).shape,
        BadgeShape::DpadDir
    );

    // PlayStation face badges carry the four action colors.
    for (btn, dom) in [
        (GamepadButton::South, 2),  // ✕: blue-dominant
        (GamepadButton::East, 0),   // ○: red-dominant
        (GamepadButton::West, 0),   // □: pink (red) dominant
        (GamepadButton::North, 1),  // △: teal-green dominant
    ] {
        let b = btn.badge_spec(GamepadBrand::PlayStation);
        assert_eq!(b.shape, BadgeShape::Circle);
        assert!(
            b.fill_rgba[dom] >= b.fill_rgba[0].max(b.fill_rgba[2]) - 1,
            "{btn:?} badge {b:?} wrong dominant channel"
        );
    }

    // Switch: crisp charcoal + white everywhere.
    let b = GamepadButton::East.badge_spec(GamepadBrand::Switch);
    assert_eq!(b.symbol, "A");
    assert!(b.fill_rgba[0] < 60 && b.fg_rgba[0] > 200);

    // Steam Deck: dark slate + white.
    let b = GamepadButton::Guide.badge_spec(GamepadBrand::SteamDeck);
    assert_eq!(b.symbol, "STEAM");
    assert!(b.fill_rgba[0] < 80 && b.fg_rgba[0] > 200);
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

fn row_regions(cols: usize) -> Vec<HitRegion> {
    // Single row of 40x40 cells at 100px pitch: centers (50,50), (150,50), ...
    (0..cols)
        .map(|c| {
            HitRegion::new(
                WidgetId::ControlKey(c),
                UiRect::new(30 + c as i32 * 100, 30, 40, 40),
            )
        })
        .collect()
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

// ---------------------------------------------------------------------------
// D-pad auto-repeat, UI scroll, virtual keyboard, input tracker, 8-way
// ---------------------------------------------------------------------------

fn jumps(events: &[GamepadEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|e| match e {
            GamepadEvent::FocusJumped { region_index } => Some(*region_index),
            _ => None,
        })
        .collect()
}

#[test]
fn dpad_hold_repeats_after_delay_and_interval() {
    let regions = row_regions(6);
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let ctx = ui_ctx(&regions);
    let held = with_button(GamepadButton::DpadRight);
    const HOLD_DT: f32 = 0.1;

    // Press edge: immediate jump to region 0.
    let ev = mapper.update(&held, DT, Some(ctx));
    assert_eq!(jumps(&ev), vec![0]);

    // 0.016 + 0.3s held: still under the 0.35s delay — silent.
    for _ in 0..3 {
        let ev = mapper.update(&held, HOLD_DT, Some(ctx));
        assert!(jumps(&ev).is_empty(), "repeat fired before delay: {ev:?}");
    }
    // ~0.416s: first auto-repeat fires (next cell).
    let ev = mapper.update(&held, HOLD_DT, Some(ctx));
    assert_eq!(jumps(&ev), vec![1]);
    // ~0.516s: one interval (~0.09s) later — repeat #2.
    let ev = mapper.update(&held, HOLD_DT, Some(ctx));
    assert_eq!(jumps(&ev), vec![2]);
}

#[test]
fn dpad_repeat_resets_on_release() {
    let regions = row_regions(6);
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let ctx = ui_ctx(&regions);
    let held = with_button(GamepadButton::DpadRight);

    // Press → jump to 0; hold past the delay so a repeat lands on 1.
    assert_eq!(jumps(&mapper.update(&held, DT, Some(ctx))), vec![0]);
    for _ in 0..4 {
        mapper.update(&held, 0.1, Some(ctx));
    }
    assert_eq!(mapper.focused_region(), Some(1));

    // Releasing and pressing again jumps immediately (edge) — but the
    // repeat timer restarts, so three more held frames stay silent.
    mapper.update(&frame(), DT, None);
    let ev = mapper.update(&held, DT, Some(ctx));
    assert_eq!(jumps(&ev), vec![2]);
    for _ in 0..3 {
        let ev = mapper.update(&held, 0.1, Some(ctx));
        assert!(jumps(&ev).is_empty(), "timer failed to reset on release");
    }
}

#[test]
fn dpad_repeat_resets_on_direction_change() {
    let regions = grid_regions();
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let ctx = ui_ctx(&regions);

    // Hold right until a repeat has fired (focus reaches region 1).
    let held_r = with_button(GamepadButton::DpadRight);
    mapper.update(&held_r, DT, Some(ctx));
    for _ in 0..4 {
        mapper.update(&held_r, 0.1, Some(ctx));
    }
    assert_eq!(mapper.focused_region(), Some(1));

    // Switching to down while still inside the repeat rhythm jumps
    // immediately to the cell below — then the timer restarts.
    let held_d = with_button(GamepadButton::DpadDown);
    let ev = mapper.update(&held_d, DT, Some(ctx));
    assert_eq!(jumps(&ev), vec![4], "direction change must jump at once");
    for _ in 0..3 {
        let ev = mapper.update(&held_d, 0.1, Some(ctx));
        assert!(
            jumps(&ev).is_empty(),
            "timer failed to reset on direction change"
        );
    }
}

#[test]
fn right_stick_scrolls_ui_lists() {
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;

    let mut raw = frame();
    raw.set_axis(GamepadAxis::RightY, -1.0);
    let events = mapper.update(&raw, DT, None);
    let (dx, dy) = events
        .iter()
        .find_map(|e| match e {
            GamepadEvent::UiScroll { dx, dy } => Some((*dx, *dy)),
            _ => None,
        })
        .expect("stick deflection must emit UiScroll");
    assert_eq!(dx, 0.0);
    assert!(dy < 0.0, "stick up scrolls up: {dy}");
    // Full deflection for one 16ms frame at 6 ticks/s ≈ 0.096 ticks.
    assert!((dy.abs() - UI_SCROLL_TICKS_PER_SEC * DT).abs() < 1e-4);

    // Inside the deadzone: silence.
    raw.set_axis(GamepadAxis::RightY, 0.1);
    let events = mapper.update(&raw, DT, None);
    assert!(!events
        .iter()
        .any(|e| matches!(e, GamepadEvent::UiScroll { .. })));

    // Horizontal scroll too.
    let mut raw = frame();
    raw.set_axis(GamepadAxis::RightX, 1.0);
    let events = mapper.update(&raw, DT, None);
    assert!(events
        .iter()
        .any(|e| matches!(e, GamepadEvent::UiScroll { dx, .. } if *dx > 0.0)));
}

#[test]
fn confirm_on_text_field_requests_virtual_keyboard() {
    // Text field under the cursor → RequestVirtualKeyboard, no UiConfirm.
    let text_regions = vec![HitRegion::new(
        WidgetId::VideoFpsField,
        UiRect::new(0, 0, 50, 50),
    )];
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let events = mapper.update(
        &with_button(GamepadButton::South),
        DT,
        Some(ui_ctx(&text_regions)),
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, GamepadEvent::RequestVirtualKeyboard)));
    assert!(!has_action(&events, GamepadAction::UiConfirm));

    // Non-text region under the cursor → plain UiConfirm.
    let button_regions = vec![HitRegion::new(
        WidgetId::TitleSingleplayer,
        UiRect::new(0, 0, 50, 50),
    )];
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    let events = mapper.update(
        &with_button(GamepadButton::South),
        DT,
        Some(ui_ctx(&button_regions)),
    );
    assert!(has_action(&events, GamepadAction::UiConfirm));
    assert!(!events
        .iter()
        .any(|e| matches!(e, GamepadEvent::RequestVirtualKeyboard)));

    // Focused-region path: D-pad onto a text field, then confirm.
    let text_regions = vec![HitRegion::new(
        WidgetId::SingleplayerSeedField,
        UiRect::new(80, 30, 40, 40),
    )];
    let mut mapper = GamepadMapper::default();
    mapper.ui_mode = true;
    mapper.update(
        &with_button(GamepadButton::DpadRight),
        DT,
        Some(ui_ctx(&text_regions)),
    );
    assert_eq!(mapper.focused_region(), Some(0));
    let events = mapper.update(
        &with_button(GamepadButton::South),
        DT,
        Some(ui_ctx(&text_regions)),
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, GamepadEvent::RequestVirtualKeyboard)));
}

#[test]
fn input_mode_tracker_switches_seamlessly() {
    let mut t = InputModeTracker::default();
    assert_eq!(t.mode(), InputSourceMode::KeyboardMouse);

    // Any button activity flips to gamepad instantly.
    t.on_gamepad_state(&with_button(GamepadButton::South));
    assert_eq!(t.mode(), InputSourceMode::Gamepad);

    // Sub-threshold mouse jitter does not steal focus.
    t.on_mouse_motion(1.0, 0.0);
    assert_eq!(t.mode(), InputSourceMode::Gamepad);
    t.on_mouse_motion(f32::NAN, 0.0);
    assert_eq!(t.mode(), InputSourceMode::Gamepad);

    // Motion >= 4px flips back; so does any key or mouse click.
    t.on_mouse_motion(3.0, 3.0); // 4.24px
    assert_eq!(t.mode(), InputSourceMode::KeyboardMouse);
    t.on_gamepad_state(&with_button(GamepadButton::Start));
    t.on_keyboard_input();
    assert_eq!(t.mode(), InputSourceMode::KeyboardMouse);
    t.on_gamepad_state(&with_button(GamepadButton::Start));
    t.on_mouse_button();
    assert_eq!(t.mode(), InputSourceMode::KeyboardMouse);

    // Tiny stick drift (below the activity threshold) does not switch.
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, 0.05);
    t.on_gamepad_state(&raw);
    assert_eq!(t.mode(), InputSourceMode::KeyboardMouse);
    // Real deflection does.
    raw.set_axis(GamepadAxis::LeftX, 0.5);
    t.on_gamepad_state(&raw);
    assert_eq!(t.mode(), InputSourceMode::Gamepad);
}

#[test]
fn eight_way_snap_for_anticheat_servers() {
    let mut mapper = GamepadMapper::default();
    mapper.movement_mode = AnalogMovementMode::EightWayVanillaCompat;

    // Nearly-cardinal input snaps exactly onto the cardinal.
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, 1.0);
    raw.set_axis(GamepadAxis::LeftY, -0.1);
    let events = mapper.update(&raw, DT, None);
    let (fwd, strafe) = events
        .iter()
        .find_map(|e| match e {
            GamepadEvent::Move {
                forward, strafe, ..
            } => Some((*forward, *strafe)),
            _ => None,
        })
        .unwrap();
    assert_eq!(fwd, 0.0, "off-cardinal drift must snap to a cardinal");
    assert!(strafe > 0.9);

    // Near-diagonal snaps to a 45° diagonal: |fwd| == |strafe| > 0.
    raw.set_axis(GamepadAxis::LeftX, 0.7);
    raw.set_axis(GamepadAxis::LeftY, -0.7);
    let events = mapper.update(&raw, DT, None);
    let (fwd, strafe) = events
        .iter()
        .find_map(|e| match e {
            GamepadEvent::Move {
                forward, strafe, ..
            } => Some((*forward, *strafe)),
            _ => None,
        })
        .unwrap();
    assert!(fwd > 0.6 && strafe > 0.6 && (fwd - strafe).abs() < 1e-4);

    // Default mode keeps the full analog direction.
    let mut mapper = GamepadMapper::default();
    assert_eq!(mapper.movement_mode, AnalogMovementMode::Full360);
    let mut raw = frame();
    raw.set_axis(GamepadAxis::LeftX, 1.0);
    raw.set_axis(GamepadAxis::LeftY, -0.1);
    let events = mapper.update(&raw, DT, None);
    let (fwd, _) = events
        .iter()
        .find_map(|e| match e {
            GamepadEvent::Move {
                forward, strafe, ..
            } => Some((*forward, *strafe)),
            _ => None,
        })
        .unwrap();
    assert!(fwd > 0.0, "Full360 must preserve analog direction");
}

// ---------------------------------------------------------------------------
// Context hint bar & radial wheel render layout
// ---------------------------------------------------------------------------

#[test]
fn context_hints_are_brand_swap_and_language_aware() {
    // Gameplay EN/Xbox: first hint is A = Jump.
    let hints = build_context_hints(HintContext::Gameplay, GamepadBrand::Xbox, UiLang::En, true);
    assert_eq!(hints[0].badge.symbol, "A");
    assert_eq!(hints[0].label, "Jump");
    assert!(hints.iter().any(|h| h.label == "Use / Place"));
    assert!(hints.iter().any(|h| h.label == "Attack / Mine"));

    // Switch with swap ON: confirm badge is the east `A`; JA labels.
    let hints =
        build_context_hints(HintContext::Gameplay, GamepadBrand::Switch, UiLang::Ja, true);
    assert_eq!(hints[0].badge.symbol, "A");
    assert_eq!(hints[0].label, "ジャンプ");
    assert!(hints.iter().any(|h| h.label == "使う / 置く"));
    assert!(hints.iter().any(|h| h.label == "攻撃 / 採掘"));
    assert!(hints.iter().any(|h| h.label == "インベントリ"));

    // Swap OFF: confirm falls back to the south `B`.
    let hints =
        build_context_hints(HintContext::Gameplay, GamepadBrand::Switch, UiLang::Ja, false);
    assert_eq!(hints[0].badge.symbol, "B");

    // PlayStation badges carry the symbols, not English words.
    let hints =
        build_context_hints(HintContext::Gameplay, GamepadBrand::PlayStation, UiLang::En, true);
    assert_eq!(hints[0].badge.symbol, "✕");
    assert_eq!(hints[0].badge.shape, BadgeShape::Circle);

    // Inventory JA: quick-move / split / scroll / back.
    let hints =
        build_context_hints(HintContext::Inventory, GamepadBrand::Xbox, UiLang::Ja, true);
    assert!(hints
        .iter()
        .any(|h| h.label == "クイック移動" && h.badge.symbol == "Y"));
    assert!(hints.iter().any(|h| h.label == "半分に分ける"));
    assert!(hints.iter().any(|h| h.label == "スクロール"));
    assert!(hints.iter().any(|h| h.label == "戻る"));

    // Settings: tab hints and Close; TextInput is concise.
    let hints =
        build_context_hints(HintContext::Settings, GamepadBrand::Xbox, UiLang::Ja, true);
    assert!(hints.iter().any(|h| h.label == "前のタブ"));
    assert!(hints.iter().any(|h| h.label == "閉じる"));
    let hints = build_context_hints(HintContext::TextInput, GamepadBrand::Xbox, UiLang::En, true);
    assert_eq!(hints.len(), 2);

    // Every context produces a non-empty bar on every brand.
    for ctx in [
        HintContext::Gameplay,
        HintContext::Inventory,
        HintContext::Container,
        HintContext::Crafting,
        HintContext::PauseMenu,
        HintContext::Settings,
        HintContext::TextInput,
    ] {
        for brand in [
            GamepadBrand::Xbox,
            GamepadBrand::PlayStation,
            GamepadBrand::Switch,
            GamepadBrand::SteamDeck,
        ] {
            assert!(
                !build_context_hints(ctx, brand, UiLang::En, true).is_empty(),
                "{ctx:?}/{brand:?} produced no hints"
            );
        }
    }
}

#[test]
fn radial_layout_exposes_draw_ready_geometry() {
    let mut wheel = RadialWheelState {
        open: true,
        selected: Some(2),
    };
    let layout = wheel.layout(320.0, 180.0, 100.0, UiLang::En);
    assert!(layout.open);
    assert_eq!(layout.slices.len(), 8);
    assert_eq!((layout.center_x, layout.center_y, layout.radius), (320.0, 180.0, 100.0));

    // Slice i is centered at i*45°, spanning ±22.5°.
    for (i, s) in layout.slices.iter().enumerate() {
        assert_eq!(s.index, i);
        assert_eq!(s.action, RADIAL_ACTIONS[i]);
        assert!((s.angle_center_deg - i as f32 * 45.0).abs() < 1e-6);
        assert!((s.angle_start_deg - (s.angle_center_deg - 22.5)).abs() < 1e-6);
        assert!((s.angle_end_deg - (s.angle_center_deg + 22.5)).abs() < 1e-6);
        assert_eq!(s.highlighted, i == 2);
    }

    // Anchors sit on the wheel ring: slice 0 east, slice 2 north
    // (screen-y grows downward).
    let s0 = layout.slices[0];
    assert!((s0.anchor_x - (320.0 + 62.0)).abs() < 0.01);
    assert!((s0.anchor_y - 180.0).abs() < 0.01);
    let s2 = layout.slices[2];
    assert!((s2.anchor_x - 320.0).abs() < 0.01);
    assert!((s2.anchor_y - (180.0 - 62.0)).abs() < 0.01);

    // Labels localize; non-finite geometry sanitizes.
    assert_eq!(layout.slices[0].label, "Debug (F3)");
    let ja = wheel.layout(320.0, 180.0, 100.0, UiLang::Ja);
    assert_eq!(ja.slices[0].label, "デバッグ (F3)");
    let bad = wheel.layout(f32::NAN, 0.0, -5.0, UiLang::En);
    assert!(bad.slices.iter().all(|s| s.anchor_x.is_finite()));

    // Neutral selection highlights nothing.
    wheel.selected = None;
    let layout = wheel.layout(0.0, 0.0, 100.0, UiLang::En);
    assert!(layout.slices.iter().all(|s| !s.highlighted));
}
