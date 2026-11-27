// SPDX-License-Identifier: LGPL-3.0-or-later
//! Rendering and geometry checks for the Create Engineer's Goggles overlay.

use megu_client_ui::create_goggles_hud::{GogglesHudLayout, GogglesTargetTelemetry};
use megu_client_ui::draw::{glyph_is_covered, text_width, UiColor, UiCommand, UiRect};

fn target(block_id: &str) -> GogglesTargetTelemetry {
    GogglesTargetTelemetry {
        block_id: block_id.into(),
        display_name: None,
        rpm: 32.0,
        stress_impact: 16.0,
        network_stress_used: 128.0,
        network_stress_capacity: 256.0,
        overstressed: false,
        processing_item: None,
        processing_progress: None,
        fluid_mb: None,
    }
}

fn text_commands(commands: &[UiCommand]) -> Vec<&str> {
    commands
        .iter()
        .filter_map(|command| match command {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn command_bounds(command: &UiCommand) -> Option<UiRect> {
    match command {
        UiCommand::FillRect { rect, .. }
        | UiCommand::BorderRect { rect, .. }
        | UiCommand::ItemIcon { rect, .. }
        | UiCommand::Sprite { rect, .. }
        | UiCommand::ProgressBar { rect, .. } => Some(*rect),
        UiCommand::Text {
            x, y, scale, text, ..
        } => Some(UiRect::new(*x, *y, text_width(text, *scale), 8 * *scale)),
    }
}

fn hotbar_rect(width: u32, height: u32) -> UiRect {
    // Vanilla 1.21.1: the 29 px offhand frame at `w/2 - 120` is flush with
    // the 182×22 strip at `h - 22`, and the viewport clips its last row.
    let scale = (height / 270).clamp(1, 4) as i32;
    let logical_width = width as i32 / scale;
    let logical_height = height as i32 / scale;
    let hotbar_width = megu_client_ui::vanilla_metrics::HOTBAR_BLOCK_WIDTH;
    let x = (logical_width / 2 - 91 - 29) * scale;
    let y = (logical_height - 23) * scale;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + hotbar_width * scale).min(width as i32);
    let y1 = (y + 23 * scale).min(height as i32);
    UiRect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

#[test]
fn normal_kinetic_target_draws_bilingual_readable_telemetry() {
    let target = target("create:water_wheel");
    for (locale, expected_speed) in [
        ("en_us", "Speed: 32.0 RPM CW"),
        ("ja_jp", "スピード: 32.0 RPM CW"),
    ] {
        let list = GogglesHudLayout::build(&target, 640, 360, locale);
        assert!(!list.is_empty());
        let texts = text_commands(&list.commands);
        assert!(
            texts.iter().any(|line| line.contains(expected_speed)),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|line| line.contains("128 / 256 SU (50%)")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|line| line.contains("Water Wheel") || line.contains("ミズグルマ")),
            "{texts:?}"
        );
        assert!(texts
            .iter()
            .flat_map(|line| line.chars())
            .all(glyph_is_covered));
    }
}

#[test]
fn negative_rpm_preserves_direction_and_overstress_is_red() {
    let mut target = target("create:water_wheel");
    target.rpm = -32.0;
    target.overstressed = true;
    target.network_stress_used = 300.0;
    let list = GogglesHudLayout::build(&target, 1280, 720, "en_us");
    let texts = text_commands(&list.commands);
    assert!(
        texts.iter().any(|line| line.contains("32.0 RPM CCW")),
        "{texts:?}"
    );
    assert!(texts.contains(&"OVERSTRESSED!"), "{texts:?}");
    assert!(list.commands.iter().any(|command| match command {
        UiCommand::Text { text, color, .. } =>
            text == "OVERSTRESSED!" && *color == megu_client_ui::draw::palette::DANGER,
        _ => false,
    }));
}

#[test]
fn press_progress_and_basin_fluid_gauge_are_included() {
    let mut press = target("create:mechanical_press");
    press.processing_item = Some("Iron Ingot".into());
    press.processing_progress = Some(0.65);
    let press_list = GogglesHudLayout::build(&press, 1280, 720, "en_us");
    let texts = text_commands(&press_list.commands);
    assert!(
        texts
            .iter()
            .any(|line| line.contains("Processing: Iron Ingot 65%")),
        "{texts:?}"
    );
    assert!(press_list.commands.iter().any(|command| matches!(command, UiCommand::ProgressBar { fraction, fill, .. } if (*fraction - 0.65).abs() < 0.001 && *fill == UiColor::rgb(85, 170, 220))));

    let mut basin = target("create:basin");
    basin.fluid_mb = Some(("Water".into(), 500, 1000));
    let basin_list = GogglesHudLayout::build(&basin, 640, 360, "en_us");
    assert!(text_commands(&basin_list.commands)
        .iter()
        .any(|line| line.contains("Water: 500 / 1000 mB")));
}

#[test]
fn compact_viewports_keep_every_emitted_command_in_bounds() {
    for (width, height) in [(640, 360), (320, 180), (160, 120)] {
        let mut telemetry = target("create:mechanical_press");
        telemetry.overstressed = true;
        telemetry.processing_item = Some("Iron Ingot".into());
        telemetry.processing_progress = Some(0.65);
        telemetry.fluid_mb = Some(("Water".into(), 500, 1000));
        let list = GogglesHudLayout::build(&telemetry, width, height, "en_us");
        assert!(!list.is_empty(), "{width}x{height}");
        let center = UiRect::new((width / 2) as i32 - 4, (height / 2) as i32 - 4, 8, 8);
        let hotbar = hotbar_rect(width, height);
        for command in &list.commands {
            if let Some(bounds) = command_bounds(command) {
                assert!(
                    bounds.x >= 0 && bounds.y >= 0,
                    "{width}x{height}: {bounds:?}"
                );
                assert!(
                    bounds.right() <= width as i32,
                    "{width}x{height}: {bounds:?}"
                );
                assert!(
                    bounds.bottom() <= height as i32,
                    "{width}x{height}: {bounds:?}"
                );
                assert!(
                    !bounds.intersects(center),
                    "crosshair overlap {width}x{height}: {bounds:?}"
                );
                assert!(
                    !bounds.intersects(hotbar),
                    "hotbar overlap {width}x{height}: {bounds:?} vs {hotbar:?}"
                );
            }
            if let UiCommand::Text { text, .. } = command {
                assert!(
                    text.chars().all(glyph_is_covered),
                    "uncovered glyph in {text:?}"
                );
            }
        }
    }
}
