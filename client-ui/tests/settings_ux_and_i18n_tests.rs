// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Feature tests for the UX polish lane: visual-style presets and the
//! four clean OFF/ON toggles, the mistake-proof `SettingsTransaction`
//! (dirty dots, per-item revert, multi-step undo/redo, discard, and the
//! confirmation-gated undo-recoverable reset), the 16-locale
//! `megu_core::i18n` language tab, and the adaptive startup
//! timeline (`Boot: N ms` badge + 250 ms live-overlay threshold).

use megu_client_ui::draw::UiCommand;
use megu_client_ui::state::{
    AudioSettings, LanguageSetting, Screen, SettingKey, StartupStageTiming, StartupTimeline,
    UiAction, UiInputEvent, UiLang, UiModal, UiState, VisualStylePreset, WidgetId,
    LIVE_LOADING_OVERLAY_MIN_MS, SETTINGS_SESSION_UNDO_CAP,
};
use megu_core::i18n::{BuiltinLocale, ALL_BUILTIN_LOCALES};

const SIZES: [(u32, u32); 6] = [
    (640, 360),
    (854, 480),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

/// Render then click the center of `id`'s hit region.
fn click(ui: &mut UiState, id: WidgetId, w: u32, h: u32) -> Vec<UiAction> {
    ui.render(w, h);
    let r = ui
        .hit_regions
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no hit region for {id:?} on {:?}", ui.screen))
        .rect;
    ui.handle_input(UiInputEvent::Click {
        x: r.x + r.w as i32 / 2,
        y: r.y + r.h as i32 / 2,
    })
}

/// Open `Screen::Settings(tab)` from Title at `1280x720`.
fn open_settings(ui: &mut UiState) {
    click(ui, WidgetId::TitleSettings, 1280, 720);
    assert!(matches!(ui.screen, Screen::Settings(_)));
}

/// `true` when `id` has a hit region on the last rendered frame.
fn has_region(ui: &UiState, id: WidgetId) -> bool {
    ui.hit_regions.iter().any(|r| r.id == id)
}

fn timeline(total_ms: u32) -> StartupTimeline {
    StartupTimeline {
        total_ms,
        stages: vec![
            StartupStageTiming {
                stage: "kernel-init".into(),
                duration_ms: 40,
                detail: "core".into(),
            },
            StartupStageTiming {
                stage: "mod-scan".into(),
                duration_ms: total_ms.saturating_sub(40),
                detail: "mods".into(),
            },
        ],
        native_mods: 12,
        wgpu_routed_mods: 3,
        quarantined_mods: 1,
    }
}

// ---------------------------------------------------------------------------
// 1. Visual style presets + external shader pack priority
// ---------------------------------------------------------------------------

#[test]
fn visual_style_preset_ordering_labels_and_default() {
    assert_eq!(
        VisualStylePreset::ALL,
        [VisualStylePreset::Vanilla, VisualStylePreset::MeguNatural]
    );
    assert_eq!(VisualStylePreset::default(), VisualStylePreset::Vanilla);

    // Cycles forward and wraps cleanly; no jargon anywhere in labels.
    assert_eq!(
        VisualStylePreset::Vanilla.next(),
        VisualStylePreset::MeguNatural
    );
    assert_eq!(
        VisualStylePreset::MeguNatural.next(),
        VisualStylePreset::Vanilla
    );
    assert_eq!(
        VisualStylePreset::Vanilla.step(-1),
        VisualStylePreset::MeguNatural
    );
    assert_eq!(VisualStylePreset::Vanilla.label(UiLang::En), "Vanilla");
    assert_eq!(VisualStylePreset::Vanilla.label(UiLang::Ja), "バニラ");
    assert_eq!(
        VisualStylePreset::MeguNatural.label(UiLang::En),
        "Megu Natural"
    );
    for p in VisualStylePreset::ALL {
        for lang in [UiLang::En, UiLang::Ja] {
            let label = p.label(lang);
            assert!(!label.contains("Feather"));
            assert!(!label.contains("陰翳"));
        }
    }
    assert_eq!(UiState::new().settings.visual.preset, VisualStylePreset::Vanilla);
}

#[test]
fn external_shader_pack_always_wins_over_builtin_style() {
    let mut ui = UiState::new();
    assert!(!ui.settings.visual.builtin_shaders_yielded());
    assert!(ui.settings.visual.external_shader_status(UiLang::En).is_none());

    ui.settings.visual.external_shader_pack_name = Some("BSL".to_string());
    ui.settings.visual.preset = VisualStylePreset::MeguNatural;
    assert!(ui.settings.visual.builtin_shaders_yielded());
    assert_eq!(
        ui.settings.visual.external_shader_status(UiLang::En),
        Some("External Shader Active: BSL".to_string())
    );
    assert_eq!(
        ui.settings.visual.external_shader_status(UiLang::Ja),
        Some("外部シェーダー優先中: BSL".to_string())
    );

    // The Shaders tab shows the status line and yields the preset cycler.
    open_settings(&mut ui);
    click(&mut ui, WidgetId::TabShaders, 1280, 720);
    let list = ui.render(1280, 720);
    let texts: Vec<&str> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("External Shader Active: BSL")),
        "status line missing: {texts:?}"
    );
    assert!(
        !has_region(&ui, WidgetId::VisualPreset),
        "preset cycler must yield while an external pack is active"
    );

    // Clearing the pack restores the built-in preset cycler.
    ui.settings.visual.external_shader_pack_name = None;
    ui.render(1280, 720);
    assert!(has_region(&ui, WidgetId::VisualPreset));
}

// ---------------------------------------------------------------------------
// 2. Clean two-way OFF/ON toggles
// ---------------------------------------------------------------------------

#[test]
fn experience_toggles_default_off_and_flip_two_way() {
    let mut ui = UiState::new();
    let v = &ui.settings.visual;
    assert!(!v.blocks_and_skins_3d);
    assert!(!v.world_effects);
    assert!(!v.spatial_audio_3d);
    assert!(!v.camera_micro_motion);
    assert_eq!(v.preset, VisualStylePreset::Vanilla);
    assert!(v.external_shader_pack_name.is_none());

    open_settings(&mut ui);
    click(&mut ui, WidgetId::TabShaders, 1280, 720);

    let acts = click(&mut ui, WidgetId::VisualBlocks3d, 1280, 720);
    assert!(acts.contains(&UiAction::SettingsChanged));
    assert!(ui.settings.visual.blocks_and_skins_3d);
    click(&mut ui, WidgetId::VisualBlocks3d, 1280, 720);
    assert!(!ui.settings.visual.blocks_and_skins_3d);

    click(&mut ui, WidgetId::VisualWorldEffects, 1280, 720);
    assert!(ui.settings.visual.world_effects);
    click(&mut ui, WidgetId::VisualCameraMotion, 1280, 720);
    assert!(ui.settings.visual.camera_micro_motion);

    // 3D Audio lives on the Audio tab.
    click(&mut ui, WidgetId::TabAudio, 1280, 720);
    click(&mut ui, WidgetId::AudioSpatial3d, 1280, 720);
    assert!(ui.settings.visual.spatial_audio_3d);

    // Labels render as plain OFF/ON — never a four-level ladder.
    let list = ui.render(1280, 720);
    let texts: Vec<&str> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("3D Audio: ON")));
    assert!(texts.iter().all(|t| !t.contains("Perf")));
    assert!(texts.iter().all(|t| !t.contains("Realistic")));

    // Preset cycles through the ordered stops from the same tab.
    click(&mut ui, WidgetId::TabShaders, 1280, 720);
    click(&mut ui, WidgetId::VisualPreset, 1280, 720);
    assert_eq!(ui.settings.visual.preset, VisualStylePreset::MeguNatural);
    click(&mut ui, WidgetId::VisualPreset, 1280, 720);
    assert_eq!(ui.settings.visual.preset, VisualStylePreset::Vanilla);
}

// ---------------------------------------------------------------------------
// 3. SettingsTransaction: dirty tracking, revert, undo/redo, discard, reset
// ---------------------------------------------------------------------------

#[test]
fn session_snapshot_dirty_tracking_and_single_item_revert() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    assert!(!ui.is_session_dirty());
    assert!(!ui.is_setting_dirty(SettingKey::VideoVsync));

    // Two independent edits inside the same session.
    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    click(&mut ui, WidgetId::VideoLod, 1280, 720);
    assert!(ui.is_session_dirty());
    assert!(ui.is_setting_dirty(SettingKey::VideoVsync));
    assert!(ui.is_setting_dirty(SettingKey::VideoLod));
    // Vsync also flips the preset to Custom; an untouched key stays clean.
    assert!(!ui.is_setting_dirty(SettingKey::AudioMaster));

    let lod_after = ui.settings.video.lod;
    let undo_len = ui.settings_tx.undo_stack.len();

    // Revert only Vsync: the Lod change and undo history survive.
    ui.revert_single_setting(SettingKey::VideoVsync);
    assert!(!ui.is_setting_dirty(SettingKey::VideoVsync));
    assert!(ui.is_setting_dirty(SettingKey::VideoLod));
    assert_eq!(ui.settings.video.lod, lod_after);
    assert_eq!(ui.settings_tx.undo_stack.len(), undo_len + 1);

    // The revert itself is undoable.
    ui.undo_settings();
    assert!(ui.is_setting_dirty(SettingKey::VideoVsync));
}

#[test]
fn multi_step_undo_redo_and_redo_clear_on_new_edit() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    let opened = ui.settings_tx.opened_snapshot.clone();

    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    click(&mut ui, WidgetId::VideoGpuBackend, 1280, 720);
    click(&mut ui, WidgetId::VideoSmoothLighting, 1280, 720);
    assert_eq!(ui.settings_tx.undo_stack.len(), 3);

    ui.undo_settings();
    ui.undo_settings();
    assert!(ui.is_setting_dirty(SettingKey::VideoVsync));
    assert!(!ui.is_setting_dirty(SettingKey::VideoSmoothLighting));
    assert_eq!(ui.settings_tx.redo_stack.len(), 2);

    ui.redo_settings();
    ui.redo_settings();
    assert!(ui.is_setting_dirty(SettingKey::VideoSmoothLighting));
    assert!(ui.settings_tx.redo_stack.is_empty());

    // Multi-step undo walks all the way back to the opened snapshot.
    ui.undo_settings();
    ui.undo_settings();
    ui.undo_settings();
    assert_eq!(ui.settings, opened);
    assert!(!ui.is_session_dirty());
    ui.undo_settings(); // empty stack: no-op, no panic.
    assert_eq!(ui.settings, opened);

    // A fresh edit after undo clears the redo stack.
    ui.undo_settings();
    assert!(!ui.settings_tx.redo_stack.is_empty());
    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    assert!(ui.settings_tx.redo_stack.is_empty());
}

#[test]
fn ctrl_z_ctrl_y_drive_undo_redo_on_settings_screen() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    assert!(ui.is_setting_dirty(SettingKey::VideoVsync));

    ui.handle_input(UiInputEvent::CharTyped('\u{1a}')); // Ctrl+Z
    assert!(!ui.is_setting_dirty(SettingKey::VideoVsync));
    ui.handle_input(UiInputEvent::CharTyped('\u{19}')); // Ctrl+Y
    assert!(ui.is_setting_dirty(SettingKey::VideoVsync));
}

#[test]
fn undo_stack_is_bounded() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    // Alternating two edits pushes a bounded history, never unbounded.
    for _ in 0..(SETTINGS_SESSION_UNDO_CAP + 8) {
        click(&mut ui, WidgetId::VideoVsync, 1280, 720);
        click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    }
    assert!(ui.settings_tx.undo_stack.len() <= SETTINGS_SESSION_UNDO_CAP);
}

#[test]
fn discard_session_restores_snapshot_and_is_undoable() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    let opened = ui.settings_tx.opened_snapshot.clone();

    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    click(&mut ui, WidgetId::VideoLod, 1280, 720);
    assert!(ui.is_session_dirty());

    let acts = ui.discard_settings_session();
    assert!(acts.contains(&UiAction::SettingsChanged));
    assert_eq!(ui.settings, opened);
    assert!(!ui.is_session_dirty());

    // Discard pushes the pre-discard state: Undo brings the edits back.
    ui.undo_settings();
    assert!(ui.is_session_dirty());
    assert!(ui.is_setting_dirty(SettingKey::VideoVsync));
    assert!(ui.is_setting_dirty(SettingKey::VideoLod));
}

#[test]
fn reset_to_defaults_requires_confirmation_and_is_undoable() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    let baseline = ui.settings.clone();

    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    let edited = ui.settings.clone();
    assert_ne!(edited.video.vsync, baseline.video.vsync);

    // The Reset button never resets directly — it opens the modal.
    ui.request_reset_defaults();
    assert_eq!(ui.modal, Some(UiModal::ConfirmResetDefaults));
    assert_eq!(ui.settings, edited, "reset must wait for confirmation");

    // Cancel keeps everything exactly as it was.
    let acts = click(&mut ui, WidgetId::ModalCancel, 1280, 720);
    assert!(acts.is_empty());
    assert!(ui.modal.is_none());
    assert_eq!(ui.settings, edited);

    // Confirming applies defaults (back to the opened snapshot here)...
    ui.request_reset_defaults();
    click(&mut ui, WidgetId::ModalOk, 1280, 720);
    assert!(ui.modal.is_none());
    assert_eq!(ui.settings, UiState::new().settings);
    assert!(!ui.is_setting_dirty(SettingKey::VideoVsync));

    // ...and the confirmed reset stays 1-click Undo-recoverable.
    ui.undo_settings();
    assert_eq!(ui.settings, edited);
}

#[test]
fn session_toolbar_buttons_wire_through_the_input_router() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    click(&mut ui, WidgetId::VideoVsync, 1280, 720);
    assert!(ui.is_session_dirty());

    // The toolbar buttons are disabled inert regions when unavailable:
    // with one edit in flight Undo + Discard + Reset are clickable.
    click(&mut ui, WidgetId::SettingsDiscard, 1280, 720);
    assert!(!ui.is_session_dirty());
    click(&mut ui, WidgetId::SettingsUndo, 1280, 720);
    assert!(ui.is_session_dirty());
    click(&mut ui, WidgetId::SettingsUndo, 1280, 720);
    assert!(!ui.is_session_dirty());
    click(&mut ui, WidgetId::SettingsRedo, 1280, 720);
    assert!(ui.is_session_dirty());
}

// ---------------------------------------------------------------------------
// 4. 16 built-in locales + native autonyms + OS detection
// ---------------------------------------------------------------------------

#[test]
fn sixteen_builtin_locales_are_defined_with_native_autonyms() {
    assert_eq!(ALL_BUILTIN_LOCALES.len(), 16);
    let codes: Vec<&str> = ALL_BUILTIN_LOCALES.iter().map(|l| l.code()).collect();
    assert_eq!(
        codes,
        [
            "en_us", "ja_jp", "zh_cn", "zh_tw", "ko_kr", "es_es", "pt_br", "fr_fr", "de_de",
            "ru_ru", "it_it", "pl_pl", "tr_tr", "id_id", "vi_vn", "uk_ua"
        ]
    );
    for locale in ALL_BUILTIN_LOCALES {
        assert!(!locale.native_name().is_empty());
        assert!(!locale.english_name().is_empty());
    }
    // Autonyms are the native spellings, not English names.
    assert!(BuiltinLocale::JaJp.native_name().starts_with("日本語"));
    assert!(BuiltinLocale::UkUa.native_name().starts_with("Українська"));
}

#[test]
fn language_cycler_steps_all_16_locales_with_autonyms() {
    let mut ui = UiState::new();
    open_settings(&mut ui);
    click(&mut ui, WidgetId::TabLanguage, 1280, 720);
    ui.render(1280, 720); // hit_regions update on render, not on click
    assert!(has_region(&ui, WidgetId::LanguageDetectOs));

    // Auto is the start: legacy field Auto + en_us locale.
    assert_eq!(ui.settings.language, LanguageSetting::Auto);
    assert_eq!(ui.settings.locale, BuiltinLocale::EnUs);
    assert_eq!(ui.language_display_name(), "Auto");

    // Auto → English → Japanese keeps the old three-click contract.
    click(&mut ui, WidgetId::LanguageChoice, 1280, 720);
    assert_eq!(ui.settings.language, LanguageSetting::English);
    assert_eq!(ui.settings.locale, BuiltinLocale::EnUs);
    click(&mut ui, WidgetId::LanguageChoice, 1280, 720);
    assert_eq!(ui.settings.language, LanguageSetting::Japanese);
    assert_eq!(ui.settings.locale, BuiltinLocale::JaJp);
    // The Language dirty key covers locale + legacy field together.
    assert!(ui.is_setting_dirty(SettingKey::Language));

    // The remaining 13 built-ins follow in catalog order, shown by
    // their native autonym; each click records undoable history.
    for &locale in &ALL_BUILTIN_LOCALES[2..] {
        click(&mut ui, WidgetId::LanguageChoice, 1280, 720);
        assert_eq!(ui.settings.locale, locale);
        assert_eq!(ui.language_display_name(), locale.native_name());
        assert_eq!(ui.lang(), UiLang::En); // non-EN/JA locales render En strings
    }
    // 16 locale stops + the Auto wrap = back where the cycle started.
    click(&mut ui, WidgetId::LanguageChoice, 1280, 720);
    assert_eq!(ui.settings.language, LanguageSetting::Auto);
    assert_eq!(ui.settings.locale, BuiltinLocale::EnUs);
    // Landing back on the opened value clears the dirty dot again.
    assert!(!ui.is_setting_dirty(SettingKey::Language));
}

#[test]
fn detect_os_locale_applies_a_builtin_locale() {
    let mut ui = UiState::new();
    ui.detect_and_apply_os_locale();
    assert!(ALL_BUILTIN_LOCALES.contains(&ui.settings.locale));
    assert_eq!(ui.settings.language, LanguageSetting::Auto);
}

#[test]
fn ui_text_resolves_builtin_catalog_entries() {
    let mut ui = UiState::new();
    use megu_core::i18n::UiTextKey;
    ui.set_locale(BuiltinLocale::JaJp);
    assert_eq!(ui.lang(), UiLang::Ja);
    assert_eq!(ui.ui_text(UiTextKey::LanguageAuto), "言語を自動判定");
    ui.set_locale(BuiltinLocale::DeDe);
    assert_eq!(ui.ui_text(UiTextKey::LanguageAuto), "Sprache automatisch erkennen");
}

// ---------------------------------------------------------------------------
// 5. StartupTimeline: overlay threshold + zero-overlap boot badge
// ---------------------------------------------------------------------------

#[test]
fn live_loading_overlay_threshold_is_exactly_250ms() {
    assert_eq!(LIVE_LOADING_OVERLAY_MIN_MS, 250);
    for total in [0, 1, 184, 249] {
        assert!(!timeline(total).should_show_live_overlay(), "{total}");
    }
    for total in [250, 251, 5000] {
        assert!(timeline(total).should_show_live_overlay(), "{total}");
    }

    let mut ui = UiState::new();
    assert!(!ui.should_show_live_loading_overlay()); // no data yet
    ui.startup_timeline = Some(timeline(249));
    assert!(!ui.should_show_live_loading_overlay());
    ui.startup_timeline = Some(timeline(250));
    assert!(ui.should_show_live_loading_overlay());
}

#[test]
fn boot_badge_is_zero_overlap_on_title_and_about_all_resolutions() {
    for (w, h) in SIZES {
        let mut ui = UiState::new();
        ui.startup_timeline = Some(timeline(184));
        for screen in [Screen::Title, Screen::AboutMegu] {
            ui.navigate(screen);
            ui.render(w, h);

            // Badge may be suppressed at tiny widths, but if present it
            // must stay inside the viewport and off every other region.
            if let Some(badge) = ui.hit_regions.iter().find(|r| r.id == WidgetId::BootBadge) {
                let r = badge.rect;
                assert!(r.x >= 0 && r.y >= 0, "{screen:?} @{w}x{h}");
                assert!(r.x + r.w as i32 <= w as i32, "{screen:?} @{w}x{h}");
                assert!(r.y + r.h as i32 <= h as i32, "{screen:?} @{w}x{h}");
                for other in &ui.hit_regions {
                    if other.id != WidgetId::BootBadge {
                        assert!(
                            !r.intersects(other.rect),
                            "{screen:?} @{w}x{h}: badge overlaps {:?}",
                            other.id
                        );
                    }
                }
            }
            ui.back();
        }
    }
}

#[test]
fn boot_badge_click_opens_timeline_modal_and_ok_closes() {
    let (w, h) = (1280u32, 720u32);
    let mut ui = UiState::new();
    ui.startup_timeline = Some(timeline(184));

    // No badge without a populated timeline — and the modal path.
    ui.render(w, h);
    assert!(has_region(&ui, WidgetId::BootBadge));
    click(&mut ui, WidgetId::BootBadge, w, h);
    assert_eq!(ui.modal, Some(UiModal::StartupTimeline));

    // Modal stays in-bounds and contains the total + stage text.
    let list = ui.render(w, h);
    let texts: Vec<&str> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("184 ms")), "{texts:?}");
    for r in &ui.hit_regions {
        assert!(r.rect.x >= 0 && r.rect.y >= 0);
        assert!(r.rect.x + r.rect.w as i32 <= w as i32);
        assert!(r.rect.y + r.rect.h as i32 <= h as i32);
        assert!(r.modal, "modal must own the only hit regions");
    }
    click(&mut ui, WidgetId::ModalOk, w, h);
    assert!(ui.modal.is_none());

    // Without a timeline there is no badge to mis-click.
    let mut bare = UiState::new();
    bare.render(w, h);
    assert!(!has_region(&bare, WidgetId::BootBadge));
}

// ---------------------------------------------------------------------------
// 6. Ear-safe first-launch audio defaults
// ---------------------------------------------------------------------------

#[test]
fn audio_settings_first_launch_defaults_are_ear_safe() {
    let a = AudioSettings::default();
    assert!(
        a.master <= 60,
        "first-launch master {} must stay ear-safe",
        a.master
    );
    assert!(
        a.music <= 60,
        "first-launch music {} must stay ear-safe",
        a.music
    );
    assert!(
        a.max_volume_cap <= 90,
        "first-launch cap {} must stay ear-safe",
        a.max_volume_cap
    );
    assert_eq!(a.max_concurrent_same_sound, 4);
}
