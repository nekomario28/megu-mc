// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_ui::container_screen::{menu_schema, ContainerScreenState, ALL_VANILLA_MENUS};
use megu_client_ui::draw::{text_height, text_width, UiCommand, UiDrawList, UiRect};
use megu_client_ui::hud_widgets::{HudSnapshot, HudWidgetsRenderer};

const RESOLUTIONS: [(u32, u32); 4] = [(854, 480), (1280, 720), (1920, 1080), (3840, 2160)];

fn overlaps(a: UiRect, b: UiRect) -> bool {
    a.intersects(b)
}

fn command_rect(command: &UiCommand) -> Option<UiRect> {
    match command {
        UiCommand::FillRect { rect, .. }
        | UiCommand::BorderRect { rect, .. }
        | UiCommand::ProgressBar { rect, .. }
        | UiCommand::ItemIcon { rect, .. }
        | UiCommand::Sprite { rect, .. } => Some(*rect),
        UiCommand::Text {
            x, y, scale, text, ..
        } => Some(UiRect::new(
            *x,
            *y,
            text_width(text, *scale),
            text_height(text, *scale),
        )),
    }
}

#[test]
fn all_vanilla_workstation_menus_layout_and_click_deterministically() {
    assert_eq!(ALL_VANILLA_MENUS.len(), 25);
    for (menu_id, schema) in ALL_VANILLA_MENUS.iter().enumerate() {
        assert_eq!(
            menu_schema(menu_id as u32).map(|s| s.name),
            Some(schema.name)
        );
        let mut baseline_action = None;
        let mut baseline_button = None;
        for (width, height) in RESOLUTIONS {
            let viewport_w = width;
            let viewport_h = height;
            let mut state = ContainerScreenState::new(schema, 7, 19, schema.name);
            let layout = state.layout(viewport_w as f32, viewport_h as f32);
            for slot in &layout.slots {
                assert!(
                    slot.rect.x >= 0 && slot.rect.y >= 0,
                    "{} slot {} starts outside viewport",
                    schema.name,
                    slot.index
                );
                assert!(
                    slot.rect.right() <= viewport_w as i32
                        && slot.rect.bottom() <= viewport_h as i32,
                    "{} slot {} exceeds viewport",
                    schema.name,
                    slot.index
                );
            }
            for (index, a) in layout.slots.iter().enumerate() {
                for b in layout.slots.iter().skip(index + 1) {
                    assert!(
                        !overlaps(a.rect, b.rect),
                        "{} slots {} and {} overlap",
                        schema.name,
                        a.index,
                        b.index
                    );
                }
            }
            for button in &layout.buttons {
                assert!(
                    button.rect.x >= 0 && button.rect.y >= 0,
                    "{} button {} starts outside viewport",
                    schema.name,
                    button.id
                );
                assert!(
                    button.rect.right() <= viewport_w as i32
                        && button.rect.bottom() <= viewport_h as i32,
                    "{} button {} exceeds viewport",
                    schema.name,
                    button.id
                );
                assert!(
                    !layout
                        .slots
                        .iter()
                        .any(|slot| overlaps(slot.rect, button.rect)),
                    "{} button {} overlaps a slot",
                    schema.name,
                    button.id
                );
            }
            for (index, a) in layout.buttons.iter().enumerate() {
                for b in layout.buttons.iter().skip(index + 1) {
                    assert!(
                        !overlaps(a.rect, b.rect),
                        "{} buttons {} and {} overlap",
                        schema.name,
                        a.id,
                        b.id
                    );
                }
            }

            let mut draw = UiDrawList::new();
            state.render(viewport_w as f32, viewport_h as f32, &mut draw);
            assert!(
                !draw.is_empty(),
                "{} emitted no visual primitives",
                schema.name
            );
            let generic_commands =
                4 + layout.slots.len() * 2 + layout.bars.len() + layout.buttons.len() * 3;
            assert!(
                draw.len() > generic_commands,
                "{} emitted no workstation-specific visual primitive",
                schema.name
            );
            for command in &draw.commands {
                if let Some(rect) = command_rect(command) {
                    assert!(
                        rect.x >= 0
                            && rect.y >= 0
                            && rect.right() <= viewport_w as i32
                            && rect.bottom() <= viewport_h as i32,
                        "{} emitted an out-of-bounds draw command: {command:?}",
                        schema.name
                    );
                }
            }

            let action = state
                .click(0, 0)
                .expect("slot-zero click should be deterministic");
            if let Some(expected) = &baseline_action {
                assert_eq!(
                    &action, expected,
                    "{} click changed by resolution",
                    schema.name
                );
            } else {
                baseline_action = Some(action);
            }

            let button_id = schema
                .buttons
                .first()
                .map(|button| if button.dynamic { 0 } else { button.id });
            let button = button_id.and_then(|id| state.button_click(id));
            if button_id.is_some() {
                assert!(
                    button.is_some(),
                    "{} button click was rejected",
                    schema.name
                );
            }
            if let Some(expected) = baseline_button {
                assert_eq!(
                    button, expected,
                    "{} button click changed by resolution",
                    schema.name
                );
            } else {
                baseline_button = Some(button);
            }
        }
    }
}

#[test]
fn hud_widgets_fit_every_resolution_and_gui_scale() {
    let snapshot = HudSnapshot::demo();
    for (width, height) in RESOLUTIONS {
        for scale in 1..=4 {
            let renderer = HudWidgetsRenderer::with_gui_scale(scale);
            let mut draw = UiDrawList::new();
            let report = renderer.render(&snapshot, width, height, &mut draw);
            assert!(!draw.is_empty(), "HUD emitted no commands at scale {scale}");
            for region in report.regions() {
                assert!(
                    region.rect.x >= 0 && region.rect.y >= 0,
                    "{} starts outside viewport",
                    region.name
                );
                assert!(
                    region.rect.right() <= width as i32 && region.rect.bottom() <= height as i32,
                    "{} exceeds viewport at scale {scale}",
                    region.name
                );
            }
            for (index, a) in report.regions().iter().enumerate() {
                for b in report.regions().iter().skip(index + 1) {
                    assert!(
                        !overlaps(a.rect, b.rect),
                        "HUD regions {} and {} overlap at scale {scale}",
                        a.name,
                        b.name
                    );
                }
            }
        }
    }
}
