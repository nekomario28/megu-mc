// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Camera/input and built-in QoL settings, including session history.

use mc_spine_client_ui::state::{
    Screen, SettingKey, SettingsTab, UiAction, UiInputEvent, UiModal, UiSettings, UiState, WidgetId,
};

fn edits() -> [(SettingKey, fn(&mut UiSettings)); 7] {
    [
        (SettingKey::FovDeg, |s| s.set_fov_deg(105)),
        (SettingKey::MouseSensitivityPct, |s| {
            s.set_mouse_sensitivity_pct(180)
        }),
        (SettingKey::ZoomDivisorTenths, |s| {
            s.set_zoom_divisor_tenths(75)
        }),
        (SettingKey::QolLookingAtHud, |s| {
            s.qol_looking_at_hud = false
        }),
        (SettingKey::QolMouseTweaks, |s| s.qol_mouse_tweaks = false),
        (SettingKey::QolInventorySort, |s| {
            s.qol_inventory_sort = false
        }),
        (SettingKey::ZshCommandBar, |s| s.zsh_command_bar = false),
    ]
}

fn opened_settings() -> UiState {
    let mut ui = UiState::new();
    ui.navigate(Screen::Settings(SettingsTab::Video));
    ui
}

fn confirm_reset(ui: &mut UiState) -> Vec<UiAction> {
    ui.render(1280, 720);
    let rect = ui
        .hit_regions
        .iter()
        .find(|region| region.id == WidgetId::ModalOk)
        .expect("reset confirmation button")
        .rect;
    ui.handle_input(UiInputEvent::Click {
        x: rect.x + rect.w as i32 / 2,
        y: rect.y + rect.h as i32 / 2,
    })
}

#[test]
fn defaults_and_effective_values() {
    let s = UiSettings::default();
    assert_eq!(s.fov_deg, 70);
    assert_eq!(s.mouse_sensitivity_pct, 100);
    assert_eq!(s.zoom_divisor_tenths, 40);
    assert_eq!(s.fov_f32(), 70.0);
    assert_eq!(s.mouse_sensitivity_f32(), 1.0);
    assert_eq!(s.zoom_divisor_f32(), 4.0);
    assert!(s.qol_looking_at_hud);
    assert!(s.qol_mouse_tweaks);
    assert!(s.qol_inventory_sort);
    assert!(s.zsh_command_bar);

    assert!(s.builtin_looking_at_hud_active(false));
    assert!(s.builtin_mouse_tweaks_active(false));
    assert!(s.builtin_inventory_sort_active(false));
    assert!(!s.builtin_looking_at_hud_active(true));
    assert!(!s.builtin_mouse_tweaks_active(true));
    assert!(!s.builtin_inventory_sort_active(true));
}

#[test]
fn camera_input_setters_clamp_both_bounds_and_normalize() {
    let mut s = UiSettings::default();
    s.set_fov_deg(0);
    s.set_mouse_sensitivity_pct(0);
    s.set_zoom_divisor_tenths(0);
    assert_eq!(
        (s.fov_deg, s.mouse_sensitivity_pct, s.zoom_divisor_tenths),
        (30, 10, 15)
    );
    assert_eq!(s.fov_f32(), 30.0);
    assert!((s.mouse_sensitivity_f32() - 0.10).abs() < 1e-6);
    assert_eq!(s.zoom_divisor_f32(), 1.5);

    s.set_fov_deg(u16::MAX);
    s.set_mouse_sensitivity_pct(u16::MAX);
    s.set_zoom_divisor_tenths(u16::MAX);
    assert_eq!(
        (s.fov_deg, s.mouse_sensitivity_pct, s.zoom_divisor_tenths),
        (110, 200, 100)
    );
    assert_eq!(s.fov_f32(), 110.0);
    assert_eq!(s.mouse_sensitivity_f32(), 2.0);
    assert_eq!(s.zoom_divisor_f32(), 10.0);

    // Reading effective values remains safe when callers directly write public fields.
    s.fov_deg = 0;
    s.mouse_sensitivity_pct = u16::MAX;
    s.zoom_divisor_tenths = 0;
    assert_eq!(s.fov_f32(), 30.0);
    assert_eq!(s.mouse_sensitivity_f32(), 2.0);
    assert_eq!(s.zoom_divisor_f32(), 1.5);
}

#[test]
fn all_seven_fields_track_dirty_and_revert_individually() {
    let mut ui = opened_settings();
    let opened = ui.settings_tx.opened_snapshot.clone();
    assert!(!ui.is_session_dirty());

    for (key, edit) in edits() {
        assert_eq!(ui.edit_settings(edit), vec![UiAction::SettingsChanged]);
        assert!(ui.is_setting_dirty(key), "{key:?} should be dirty");
    }
    assert!(ui.is_session_dirty());
    let edited = ui.settings.clone();
    assert_eq!(ui.settings_tx.undo_stack.len(), 7);
    assert_eq!(ui.settings_tx.undo_stack[0], opened);

    for (key, _) in edits() {
        assert_eq!(
            ui.revert_single_setting(key),
            vec![UiAction::SettingsChanged]
        );
        assert!(!ui.is_setting_dirty(key), "{key:?} should be reverted");
        assert!(ui.revert_single_setting(key).is_empty());
    }
    assert_eq!(ui.settings, opened);
    assert!(!ui.is_session_dirty());

    for _ in 0..7 {
        assert_eq!(ui.undo_settings(), vec![UiAction::SettingsChanged]);
    }
    assert_eq!(ui.settings, edited);
    for (key, _) in edits() {
        assert!(ui.is_setting_dirty(key));
    }

    for _ in 0..7 {
        assert_eq!(ui.redo_settings(), vec![UiAction::SettingsChanged]);
    }
    assert_eq!(ui.settings, opened);
}

#[test]
fn undo_redo_and_confirmed_reset_restore_all_seven_values() {
    let mut ui = opened_settings();
    let opened = ui.settings.clone();

    for (_, edit) in edits() {
        ui.edit_settings(edit);
    }
    let edited = ui.settings.clone();

    for _ in 0..7 {
        assert_eq!(ui.undo_settings(), vec![UiAction::SettingsChanged]);
    }
    assert_eq!(ui.settings, opened);
    assert!(!ui.is_session_dirty());
    assert!(ui.undo_settings().is_empty());

    for _ in 0..7 {
        assert_eq!(ui.redo_settings(), vec![UiAction::SettingsChanged]);
    }
    assert_eq!(ui.settings, edited);
    assert!(ui.redo_settings().is_empty());

    assert!(ui.request_reset_defaults().is_empty());
    assert_eq!(ui.modal, Some(UiModal::ConfirmResetDefaults));
    assert_eq!(ui.settings, edited);
    assert_eq!(confirm_reset(&mut ui), vec![UiAction::SettingsChanged]);
    assert_eq!(ui.settings, UiSettings::default());

    assert_eq!(ui.undo_settings(), vec![UiAction::SettingsChanged]);
    assert_eq!(ui.settings, edited);
    assert_eq!(ui.redo_settings(), vec![UiAction::SettingsChanged]);
    assert_eq!(ui.settings, opened);
}

#[test]
fn no_op_edits_preserve_history_and_a_new_edit_clears_redo() {
    let mut ui = opened_settings();
    assert!(ui.edit_settings(|s| s.set_fov_deg(70)).is_empty());
    assert!(ui.settings_tx.undo_stack.is_empty());

    ui.edit_settings(|s| s.set_fov_deg(200));
    assert_eq!(ui.settings.fov_deg, 110);
    assert!(ui.edit_settings(|s| s.set_fov_deg(999)).is_empty());
    assert_eq!(ui.settings_tx.undo_stack.len(), 1);

    ui.undo_settings();
    assert_eq!(ui.settings.fov_deg, 70);
    assert_eq!(ui.settings_tx.redo_stack.len(), 1);
    ui.edit_settings(|s| s.qol_mouse_tweaks = false);
    assert!(ui.settings_tx.redo_stack.is_empty());
}
