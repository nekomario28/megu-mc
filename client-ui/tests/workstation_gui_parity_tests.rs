// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_ui::container_screen::{
    menu_schema, ContainerScreenState, MerchantOfferView, WorkstationAction, ALL_VANILLA_MENUS,
};
use megu_client_ui::draw::{text_height, text_width, UiCommand, UiDrawList, UiRect};
use megu_client_ui::hud_widgets::{HudAuxiliaryState, HudSnapshot, HudWidgetsRenderer};

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

#[test]
fn mount_jump_and_attack_indicators_fit_every_gui_scale() {
    let state = HudAuxiliaryState {
        mount_health: 13.0,
        mount_max_health: 20.0,
        jump_charge: 0.75,
        attack_cooldown: 0.6,
        damage_flash: true,
    };
    for (width, height) in RESOLUTIONS {
        for scale in 1..=4 {
            let renderer = HudWidgetsRenderer::with_gui_scale(scale);
            let mut draw = UiDrawList::new();
            let report = renderer.render_auxiliary(&state, width, height, &mut draw);
            assert_eq!(report.regions().len(), 3);
            for region in report.regions() {
                assert!(
                    region.rect.x >= 0
                        && region.rect.y >= 0
                        && region.rect.right() <= width as i32
                        && region.rect.bottom() <= height as i32,
                    "{} exceeds viewport at scale {scale}",
                    region.name
                );
            }
            for (index, a) in report.regions().iter().enumerate() {
                for b in report.regions().iter().skip(index + 1) {
                    assert!(
                        !overlaps(a.rect, b.rect),
                        "{} and {} overlap at scale {scale}",
                        a.name,
                        b.name
                    );
                }
            }
        }
    }
}

#[test]
fn anvil_rename_focus_typing_and_backspace_emit_typed_actions() {
    let schema = menu_schema(8).unwrap();
    let mut state = ContainerScreenState::new(schema, 2, 3, "Anvil");
    let field = state.rename_input_rect(854.0, 480.0).unwrap();
    assert_eq!(state.click_at(field.x + 2, field.y + 2, 854.0, 480.0), None);
    assert!(state.rename_focused);
    assert_eq!(
        state.handle_rename_char('S'),
        Some(WorkstationAction::RenameItem("S".into()))
    );
    assert_eq!(
        state.handle_rename_char('w'),
        Some(WorkstationAction::RenameItem("Sw".into()))
    );
    assert_eq!(
        state.handle_rename_backspace(),
        Some(WorkstationAction::RenameItem("S".into()))
    );
    assert_eq!(
        state.set_rename_text("Sword"),
        Some(WorkstationAction::RenameItem("Sword".into()))
    );
    let mut draw = UiDrawList::new();
    state.render(854.0, 480.0, &mut draw);
    assert!(draw
        .commands
        .iter()
        .any(|c| matches!(c, UiCommand::Text { text, .. } if text.starts_with("Sword"))));
}

#[test]
fn beacon_selection_is_tier_gated_and_confirm_emits_selection() {
    let schema = menu_schema(9).unwrap();
    let mut state = ContainerScreenState::new(schema, 2, 3, "Beacon");
    let p = state.panel_rect(854.0, 480.0);
    let point = |x: i32, y: i32| (p.x + 4 + x + 2, p.y + 4 + y + 2);
    let (x, y) = point(24, 54);
    state.click_at(x, y, 854.0, 480.0);
    assert_eq!(
        state.beacon_primary, None,
        "tier 1 must remain locked at pyramid level 0"
    );
    state.set_property(0, 3);
    state.click_at(x, y, 854.0, 480.0);
    assert_eq!(state.beacon_primary, Some(1));
    let (x, y) = point(24, 108);
    state.click_at(x, y, 854.0, 480.0);
    assert_eq!(
        state.beacon_secondary, None,
        "tier 4 regeneration must remain locked at level 3"
    );
    state.set_property(0, 4);
    state.click_at(x, y, 854.0, 480.0);
    assert_eq!(state.beacon_secondary, Some(10));
    let (x, y) = point(158, 72);
    assert_eq!(
        state.click_at(x, y, 854.0, 480.0),
        Some(WorkstationAction::SetBeacon {
            primary: Some(1),
            secondary: Some(10)
        })
    );
}

#[test]
fn merchant_sidebar_scrolls_selects_and_renders_discount_and_stock_state() {
    let schema = menu_schema(19).unwrap();
    let mut state = ContainerScreenState::new(schema, 2, 3, "Merchant");
    let offers: Vec<_> = (0..10)
        .map(|i| MerchantOfferView {
            input1: "minecraft:emerald".into(),
            input2: Some("minecraft:book".into()),
            output: "minecraft:enchanted_book".into(),
            original_cost: 20,
            effective_cost: if i == 3 { 12 } else { 20 },
            uses: 0,
            max_uses: 12,
            out_of_stock: i == 3,
        })
        .collect();
    state.set_merchant_offers(offers);
    state.scroll_merchant(3);
    assert_eq!(state.merchant_scroll_offset, 3);
    let p = state.panel_rect(854.0, 480.0);
    assert_eq!(
        state.click_at(p.x + 4 + 10, p.y + 4 + 20, 854.0, 480.0),
        Some(WorkstationAction::SelectTrade(3))
    );
    assert_eq!(state.selected_trade_index, Some(3));
    let mut draw = UiDrawList::new();
    state.render(854.0, 480.0, &mut draw);
    assert!(draw.commands.iter().any(|c| matches!(c, UiCommand::Text { text, color, .. } if text == "20>12" && *color == megu_client_ui::draw::palette::ACCENT)));
    assert!(draw.commands.iter().any(|c| matches!(c, UiCommand::Text { text, color, .. } if text == "X" && *color == megu_client_ui::draw::palette::DANGER)));
}

#[test]
fn all_menus_fit_and_keep_hitboxes_disjoint_at_all_requested_scales() {
    for schema in &ALL_VANILLA_MENUS {
        for (width, height) in RESOLUTIONS {
            for scale in 1..=4 {
                let mut state = ContainerScreenState::new(schema, 7, 19, schema.name);
                if schema.menu_id == 19 {
                    state.set_merchant_offers(
                        (0..9)
                            .map(|_| MerchantOfferView {
                                input1: "minecraft:emerald".into(),
                                input2: None,
                                output: "minecraft:bread".into(),
                                original_cost: 1,
                                effective_cost: 1,
                                uses: 0,
                                max_uses: 12,
                                out_of_stock: false,
                            })
                            .collect(),
                    );
                }
                let logical_w = (width as f32 / scale as f32).ceil() as i32;
                let logical_h = (height as f32 / scale as f32).ceil() as i32;
                let layout = state.layout_scaled(width as f32, height as f32, scale as f32);
                assert!(
                    layout.panel.x >= 0
                        && layout.panel.y >= 0
                        && layout.panel.right() <= logical_w
                        && layout.panel.bottom() <= logical_h,
                    "{} panel at {}x{} scale {}",
                    schema.name,
                    width,
                    height,
                    scale
                );
                for slot in &layout.slots {
                    assert!(
                        slot.rect.x >= 0
                            && slot.rect.y >= 0
                            && slot.rect.right() <= logical_w
                            && slot.rect.bottom() <= logical_h,
                        "{} slot at scale {}",
                        schema.name,
                        scale
                    );
                }
                for button in &layout.buttons {
                    assert!(
                        button.rect.x >= 0
                            && button.rect.y >= 0
                            && button.rect.right() <= logical_w
                            && button.rect.bottom() <= logical_h,
                        "{} button at scale {}",
                        schema.name,
                        scale
                    );
                    assert!(
                        !layout.slots.iter().any(|s| overlaps(s.rect, button.rect)),
                        "{} button {} overlaps slot at scale {}",
                        schema.name,
                        button.id,
                        scale
                    );
                }
                for (i, a) in layout.slots.iter().enumerate() {
                    for b in layout.slots.iter().skip(i + 1) {
                        assert!(
                            !overlaps(a.rect, b.rect),
                            "{} slots overlap at scale {}",
                            schema.name,
                            scale
                        );
                    }
                }
                for (i, a) in layout.buttons.iter().enumerate() {
                    for b in layout.buttons.iter().skip(i + 1) {
                        assert!(
                            !overlaps(a.rect, b.rect),
                            "{} buttons overlap at scale {}",
                            schema.name,
                            scale
                        );
                    }
                }
                let mut draw = UiDrawList::new();
                state.render_scaled(width as f32, height as f32, scale as f32, &mut draw);
                for command in &draw.commands {
                    if let Some(rect) = command_rect(command) {
                        assert!(
                            rect.x >= 0
                                && rect.y >= 0
                                && rect.right() <= logical_w
                                && rect.bottom() <= logical_h,
                            "{} draw command outside at scale {}: {command:?}",
                            schema.name,
                            scale
                        );
                    }
                }
            }
        }
    }
}
