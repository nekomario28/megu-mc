// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for `mc-spine-client-ui`: every screen must lay out
//! with zero overlapping interactive hitboxes and rasterize to non-blank
//! pixels at both `1280×720` and `640×360`, and every interactive flow
//! (FPS cap, QoL toggles, fair-play, mod-sync consent, crash send gate)
//! must behave through the real input router.

use mc_spine_client_ui::draw::{
    encode_png_rgba, font_atlas_rgba, glyph_for, rasterize_draw_list, tessellate_draw_list,
    text_height, text_width, UiCommand, UiDrawList, UiRect, FONT_ATLAS_HEIGHT, FONT_ATLAS_WIDTH,
};
use mc_spine_client_ui::magic_hud::{
    CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState,
};
use mc_spine_client_ui::state::{
    invite_code_display, invite_code_parse, CrashReportMode, GamepadButton, HitRegion, HudSubtitle,
    LanguageSetting, MiniHudPreset, MinimapMode, ModSyncEntry, PlayerRadarScope, Screen,
    SettingsTab, SubtitleDirection, UiAction, UiInputEvent, UiKey, UiLang, UiModal, UiState,
    WidgetId, CRASH_PREVIEW_MAX_BYTES,
};
use mc_spine_core::crash_report::CrashContext;
use mc_spine_core::sys_metrics::{MiniHudAnchor, MiniHudConfig, MiniHudData};

const SIZES: [(u32, u32); 2] = [(1280, 720), (640, 360)];
const HIT_SIZES: [(u32, u32); 4] = [(640, 360), (1280, 720), (1920, 1080), (3840, 2160)];

/// Every screen the UI can show, with the state needed to render it.
fn all_screens(ui: &mut UiState) -> Vec<Screen> {
    ui.hud = mc_spine_client_ui::state::HudState::demo();
    ui.hud.show_player_list = true;
    ui.pending_mod_sync = vec![
        ModSyncEntry::added("create", "aabbccddee001122334455667788990011"),
        ModSyncEntry {
            name: "jei".to_string(),
            sha256_hex: "ff00ff00ff00ff00ff00ff00ff00ff00".to_string(),
            change: mc_spine_client_ui::state::ModSyncChange::Updated,
        },
        ModSyncEntry {
            name: "old-mod".to_string(),
            sha256_hex: "11".repeat(32),
            change: mc_spine_client_ui::state::ModSyncChange::Removed,
        },
    ];
    ui.missing_mod_deps = vec!["create".into(), "jei".into()];
    ui.crash = Some(CrashContext::new(
        "panic",
        "create",
        "1.21.1",
        "contraption NPE at /home/alice/mods/x.jar — Bearer sekret",
    ));
    vec![
        Screen::Title,
        Screen::Settings(SettingsTab::Video),
        Screen::Settings(SettingsTab::Audio),
        Screen::Settings(SettingsTab::Shaders),
        Screen::Settings(SettingsTab::Qol),
        Screen::Settings(SettingsTab::FairPlay),
        Screen::Settings(SettingsTab::Language),
        Screen::AboutMegu,
        Screen::Friends,
        Screen::ModSyncConsent,
        Screen::InGameHud,
        Screen::Pause,
        Screen::CrashRecovery,
    ]
}

fn assert_no_overlaps(regions: &[HitRegion]) {
    for (i, a) in regions.iter().enumerate() {
        for (j, b) in regions.iter().enumerate() {
            if i >= j {
                continue;
            }
            assert!(
                !a.rect.intersects(b.rect),
                "hit regions overlap: {a:?} vs {b:?}"
            );
        }
    }
}

fn assert_hit_regions_within_viewport(regions: &[HitRegion], w: u32, h: u32) {
    assert_no_overlaps(regions);
    for region in regions {
        assert!(
            region.rect.x >= 0
                && region.rect.y >= 0
                && region.rect.right() <= w as i32
                && region.rect.bottom() <= h as i32,
            "hit region outside {w}x{h}: {region:?}"
        );
    }
}

/// Every draw command — rects, bars, and text — must land inside the
/// viewport (text boxes included, so clipped overlay lines can't spill).
fn assert_draws_within_viewport(list: &UiDrawList, w: u32, h: u32) {
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => {
                assert!(
                    rect.x >= 0
                        && rect.y >= 0
                        && rect.right() <= w as i32
                        && rect.bottom() <= h as i32,
                    "draw outside {w}x{h}: {cmd:?}"
                );
            }
            UiCommand::Text {
                x,
                y,
                scale,
                text,
                ..
            } => {
                let right = *x + text_width(text, *scale) as i32;
                let bottom = *y + text_height(text, *scale) as i32;
                assert!(
                    *x >= 0 && *y >= 0 && right <= w as i32 && bottom <= h as i32,
                    "text outside {w}x{h}: {cmd:?}"
                );
            }
        }
    }
}

/// A fully populated telemetry snapshot: every MiniHudData field set so
/// both the Basic and Advanced segments have real content to draw.
fn populated_mini_hud_data() -> MiniHudData {
    MiniHudData {
        fps: Some(60.0),
        fps_one_percent_low: Some(9.0),
        mspt: Some(4.2),
        tps: Some(19.8),
        coords: Some((10.0, 64.0, -3.0)),
        facing: Some("N".into()),
        biome: Some("Plains".into()),
        ping_ms: Some(24),
        ram_used_mib: Some(512),
        ram_total_mib: Some(1024),
        cpu_percent: Some(9.0),
        gpu_name: Some("Test GPU".into()),
        gpu_busy_percent: Some(55),
        vram_used_mib: Some(512),
        vram_total_mib: Some(1024),
        loaded_chunks: Some(123),
        entity_count: Some(45),
        block_light: Some(15),
        sky_light: Some(15),
        shader_name: Some("megu".into()),
        audio_ok: Some(true),
    }
}

fn demo_subtitles() -> Vec<HudSubtitle> {
    vec![
        HudSubtitle {
            label: "Zombie groans".into(),
            direction: SubtitleDirection::Left,
        },
        HudSubtitle {
            label: "Creeper hiss".into(),
            direction: SubtitleDirection::Center,
        },
        HudSubtitle {
            label: "Skeleton shoots".into(),
            direction: SubtitleDirection::Right,
        },
        HudSubtitle {
            label: "Witch giggles".into(),
            direction: SubtitleDirection::Behind,
        },
    ]
}

fn assert_non_blank(rgba: &[u8]) {
    assert!(rgba.len().is_multiple_of(4));
    let (pixels, _) = rgba.as_chunks::<4>();
    let non_transparent = pixels.iter().filter(|p| p[3] != 0).count();
    assert!(non_transparent > 100, "frame is blank");
    // And not a flat single-color fill either.
    let first = pixels[0];
    let different = pixels.iter().filter(|&&p| p != first).count();
    assert!(different > 100, "frame is a flat fill");
}

#[test]
fn every_screen_lays_out_clean_and_rasterizes() {
    for &(w, h) in &SIZES {
        let mut ui = UiState::new();
        for screen in all_screens(&mut ui) {
            ui.screen = screen;
            ui.nav_stack.clear();
            let list = ui.render(w, h);
            assert!(!list.is_empty(), "{screen:?} produced no draw commands");
            assert_no_overlaps(&ui.hit_regions);
            let rgba = rasterize_draw_list(w, h, &list);
            assert_eq!(rgba.len(), (w * h * 4) as usize);
            assert_non_blank(&rgba);
        }
    }
}

#[test]
fn every_screen_has_separate_in_bounds_hit_regions_at_four_resolutions() {
    for &(w, h) in &HIT_SIZES {
        let mut ui = UiState::new();
        ui.website_url = "https://example.invalid".into();
        ui.support_url = "https://example.invalid/support".into();
        ui.contribute_url = "https://example.invalid/contribute".into();
        ui.report_url = "https://example.invalid/report".into();
        for screen in all_screens(&mut ui) {
            ui.screen = screen;
            ui.modal = None;
            ui.render(w, h);
            assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        }
    }
}

#[test]
fn modal_leaves_only_modal_regions() {
    let mut ui = UiState::new();
    ui.show_crash(CrashContext::new("panic", "mod", "1.21.1", "boom"));
    ui.render(1280, 720);
    let pre_modal = ui.hit_regions.len();
    assert!(pre_modal >= 2, "crash screen should have buttons");

    ui.modal = Some(UiModal::ConfirmSendCrashReport);
    ui.render(1280, 720);
    assert!(!ui.hit_regions.is_empty());
    assert!(ui.hit_regions.iter().all(|r| r.modal));
    assert_no_overlaps(&ui.hit_regions);
}

#[test]
fn title_flow_and_support_links() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.render(w, h);

    // Support/Contribute hidden while URLs are empty.
    assert!(!ui.hit_regions.iter().any(|r| r.id == WidgetId::SupportLink));
    assert!(!ui
        .hit_regions
        .iter()
        .any(|r| r.id == WidgetId::ContributeLink));

    ui.support_url = "https://example.invalid/support".to_string();
    ui.contribute_url = "https://example.invalid/contribute".to_string();
    ui.render(w, h);
    for (id, url) in [
        (WidgetId::SupportLink, ui.support_url.clone()),
        (WidgetId::ContributeLink, ui.contribute_url.clone()),
    ] {
        let r = ui
            .hit_regions
            .iter()
            .find(|r| r.id == id)
            .expect("link region");
        let acts = ui.handle_input(UiInputEvent::Click {
            x: r.rect.x + 2,
            y: r.rect.y + 2,
        });
        assert_eq!(acts, vec![UiAction::OpenUrl(url)]);
    }

    // Title buttons via real click hit-testing.
    click(&mut ui, WidgetId::TitleSettings, w, h);
    assert!(matches!(ui.screen, Screen::Settings(SettingsTab::Video)));
    ui.back();
    ui.render(w, h);
    let acts = click(&mut ui, WidgetId::TitleQuit, w, h);
    assert_eq!(acts, vec![UiAction::QuitGame]);
}

#[test]
fn title_links_respect_support_preference_and_report_is_always_accessible() {
    let mut ui = UiState::new();
    ui.website_url = "https://example.invalid".into();
    ui.support_url = "https://example.invalid/support".into();
    ui.contribute_url = "https://example.invalid/contribute".into();
    ui.report_url = "https://example.invalid/issues".into();
    assert!(ui.settings.qol.show_support_links);

    for &(w, h) in &HIT_SIZES {
        ui.screen = Screen::Title;
        let commands = ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        for (id, url) in [
            (WidgetId::WebsiteLink, ui.website_url.clone()),
            (WidgetId::ReportLink, ui.report_url.clone()),
            (WidgetId::SupportLink, ui.support_url.clone()),
            (WidgetId::ContributeLink, ui.contribute_url.clone()),
        ] {
            assert!(ui.hit_regions.iter().any(|r| r.id == id));
            assert_eq!(click(&mut ui, id, w, h), vec![UiAction::OpenUrl(url)]);
        }
        // Even at scale 4, links occupy the top-right above the title.
        let s = ui.effective_scale(h) as i32;
        let links: Vec<_> = ui
            .hit_regions
            .iter()
            .filter(|r| {
                matches!(
                    r.id,
                    WidgetId::WebsiteLink
                        | WidgetId::ReportLink
                        | WidgetId::SupportLink
                        | WidgetId::ContributeLink
                )
            })
            .collect();
        assert_eq!(links.len(), 4);
        assert!(links
            .iter()
            .all(|r| r.rect.y == 6 * s && r.rect.bottom() < 34 * s));
        assert_eq!(
            links.iter().map(|r| r.rect.right()).max(),
            Some(w as i32 - 4 * s)
        );
        let version = commands
            .commands
            .iter()
            .find_map(|cmd| match cmd {
                UiCommand::Text { y, text, .. } if text.starts_with("mc-spine client-ui") => {
                    Some(*y)
                }
                _ => None,
            })
            .expect("version label");
        assert!(
            links.iter().all(|r| r.rect.bottom() <= version),
            "version overlaps link bar at {w}x{h}"
        );
    }

    ui.settings.qol.show_support_links = false;
    for &(w, h) in &HIT_SIZES {
        ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        assert!(!ui
            .hit_regions
            .iter()
            .any(|r| matches!(r.id, WidgetId::SupportLink | WidgetId::ContributeLink)));
        assert_eq!(
            click(&mut ui, WidgetId::ReportLink, w, h),
            vec![UiAction::OpenUrl(ui.report_url.clone())]
        );
        assert_eq!(
            click(&mut ui, WidgetId::WebsiteLink, w, h),
            vec![UiAction::OpenUrl(ui.website_url.clone())]
        );
    }
    ui.settings.language = LanguageSetting::Japanese;
    let commands = ui.render(640, 360);
    assert!(commands
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text == "報告")));

    ui.report_url.clear();
    ui.render(640, 360);
    assert!(!ui.hit_regions.iter().any(|r| r.id == WidgetId::ReportLink));
    ui.website_url.clear();
    ui.render(640, 360);
    assert!(!ui.hit_regions.iter().any(|r| r.id == WidgetId::WebsiteLink));
}

#[test]
fn about_megu_opens_from_settings_and_links_ignore_title_toggle() {
    let mut ui = UiState::new();
    ui.website_url = "https://example.invalid".into();
    ui.support_url = "https://example.invalid/support".into();
    ui.contribute_url = "https://example.invalid/contribute".into();
    ui.report_url = "https://example.invalid/report".into();
    ui.settings.qol.show_support_links = false;

    for &(w, h) in &HIT_SIZES {
        ui.go_title();
        assert_eq!(
            click(&mut ui, WidgetId::TitleSettings, w, h),
            vec![UiAction::OpenScreen(Screen::Settings(SettingsTab::Video))]
        );
        assert_eq!(
            click(&mut ui, WidgetId::SettingsAboutMegu, w, h),
            vec![UiAction::OpenScreen(Screen::AboutMegu)]
        );
        assert_eq!(ui.screen, Screen::AboutMegu);
        let draw = ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        assert!(draw.commands.iter().any(|cmd| matches!(
            cmd,
            UiCommand::Text { text, .. } if text.starts_with("Megu v")
        )));
        assert!(draw.commands.iter().any(|cmd| matches!(
            cmd,
            UiCommand::Text { text, .. } if text.contains("LGPL-3.0-or-later")
        )));
        for (id, url) in [
            (WidgetId::WebsiteLink, ui.website_url.clone()),
            (WidgetId::SupportLink, ui.support_url.clone()),
            (WidgetId::ContributeLink, ui.contribute_url.clone()),
            (WidgetId::ReportLink, ui.report_url.clone()),
        ] {
            assert_eq!(click(&mut ui, id, w, h), vec![UiAction::OpenUrl(url)]);
        }
        assert_eq!(
            click(&mut ui, WidgetId::AboutBack, w, h),
            vec![UiAction::Back]
        );
        assert_eq!(ui.screen, Screen::Settings(SettingsTab::Video));
        click(&mut ui, WidgetId::SettingsAboutMegu, w, h);
        assert_eq!(key(&mut ui, UiKey::Escape), vec![UiAction::Back]);
        assert_eq!(ui.screen, Screen::Settings(SettingsTab::Video));
    }
}

#[test]
fn pause_report_bug_is_optional_and_never_advertises_support() {
    let mut ui = UiState::new();
    ui.screen = Screen::Pause;
    ui.support_url = "https://example.invalid/support".into();
    ui.contribute_url = "https://example.invalid/contribute".into();
    for &(w, h) in &HIT_SIZES {
        ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        assert!(!ui.hit_regions.iter().any(|r| matches!(
            r.id,
            WidgetId::PauseReportBug | WidgetId::SupportLink | WidgetId::ContributeLink
        )));
    }
    ui.report_url = "https://example.invalid/issues".into();
    for &(w, h) in &HIT_SIZES {
        ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        assert!(ui
            .hit_regions
            .iter()
            .any(|r| r.id == WidgetId::PauseReportBug));
        assert!(!ui
            .hit_regions
            .iter()
            .any(|r| matches!(r.id, WidgetId::SupportLink | WidgetId::ContributeLink)));
        assert_eq!(
            click(&mut ui, WidgetId::PauseReportBug, w, h),
            vec![UiAction::OpenUrl(ui.report_url.clone())]
        );
    }
}

#[test]
fn six_settings_tabs_fit_with_modern_extension_controls() {
    let mut ui = UiState::new();
    assert_eq!(SettingsTab::ALL.len(), 6);
    for &(w, h) in &HIT_SIZES {
        for tab in SettingsTab::ALL {
            ui.screen = Screen::Settings(tab);
            let list = ui.render(w, h);
            assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
            assert!(ui
                .hit_regions
                .iter()
                .any(|r| r.id == WidgetId::SettingsDone));
            let extension = matches!(
                tab,
                SettingsTab::Shaders | SettingsTab::Qol | SettingsTab::FairPlay
            );
            assert_eq!(
                list.commands.iter().any(
                    |cmd| matches!(cmd, UiCommand::Text { text, .. } if text == "MEGU ENHANCED")
                ),
                extension
            );
            for id in [
                WidgetId::TabVideo,
                WidgetId::TabAudio,
                WidgetId::TabShaders,
                WidgetId::TabQol,
                WidgetId::TabFairPlay,
                WidgetId::TabLanguage,
            ] {
                assert!(ui.hit_regions.iter().any(|r| r.id == id));
            }
        }
    }

    ui.screen = Screen::Settings(SettingsTab::Video);
    assert!(click(&mut ui, WidgetId::TabShaders, 640, 360).is_empty());
    assert_eq!(ui.screen, Screen::Settings(SettingsTab::Shaders));
    assert_eq!(
        click(&mut ui, WidgetId::ShadersOpenOptions, 640, 360),
        vec![UiAction::OpenShaderOptions]
    );
    assert!(!ui.settings.shader_quick.shaders_enabled);
    assert_eq!(
        click(&mut ui, WidgetId::ShadersEnabled, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert!(ui.settings.shader_quick.shaders_enabled);
    ui.active_shaderpack_name = "Example shaderpack".into();
    ui.active_shaderpack_profile = "High Quality".into();
    let list = ui.render(640, 360);
    for label in ["Example shaderpack", "High Quality"] {
        assert!(list
            .commands
            .iter()
            .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text == label)));
    }
    ui.active_shaderpack_name.clear();
    let list = ui.render(640, 360);
    assert!(list
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text == "Vanilla (Off)")));

    click(&mut ui, WidgetId::TabQol, 640, 360);
    assert_eq!(
        click(&mut ui, WidgetId::QolShowSupportLinks, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert!(!ui.settings.qol.show_support_links);
    ui.settings.language = LanguageSetting::Japanese;
    let list = ui.render(640, 360);
    assert!(list.commands.iter().any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text == "タイトルの支援リンクを表示: OFF")));
}

#[test]
fn magic_hud_only_adds_overlay_when_active() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    let baseline = ui.render(640, 360);
    assert!(ui.hud.magic.is_empty());
    assert_eq!(ui.render(640, 360), baseline);
    ui.hud.magic = MagicHudState {
        mana: 45.0,
        max_mana: 100.0,
        active_cast: Some(CastBarState {
            spell_label: "Fireball".into(),
            progress: 0.5,
            ..Default::default()
        }),
        cooldowns: vec![CooldownSlotState {
            spell_label: "Frost".into(),
            remaining_ticks: 20,
            total_ticks: 60,
        }],
        custom_statuses: vec![StatusBadgeState {
            label: "Ward".into(),
            remaining_ticks: 80,
            ..Default::default()
        }],
    };
    let overlay = ui.render(640, 360);
    assert!(overlay.len() > baseline.len());
    assert!(overlay
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Mana"))));
    assert!(overlay
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Fireball"))));
    assert!(overlay
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Frost"))));
    assert!(overlay
        .commands
        .iter()
        .any(|cmd| matches!(cmd, UiCommand::Text { text, .. } if text.contains("Ward"))));
    assert!(ui.hit_regions.is_empty());

    let mut reused = UiDrawList::new();
    ui.render_into(640, 360, &mut reused);
    assert_eq!(reused, overlay);
    ui.render_into(640, 360, &mut reused);
    assert_eq!(reused, overlay);
    ui.hud.magic = MagicHudState::default();
    assert_eq!(ui.render(640, 360), baseline);
}

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

fn key(ui: &mut UiState, k: UiKey) -> Vec<UiAction> {
    ui.handle_input(UiInputEvent::KeyPressed(k))
}

#[test]
fn fps_cap_slider_cycles_and_custom_input_works() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    click(&mut ui, WidgetId::TitleSettings, w, h);
    assert!(matches!(ui.screen, Screen::Settings(SettingsTab::Video)));

    // Focus the FPS slider via keyboard nav, then cycle with Right.
    ui.render(w, h);
    let idx = ui
        .hit_regions
        .iter()
        .position(|r| r.id == WidgetId::VideoFpsSlider)
        .unwrap();
    ui.focus_index = Some(idx);

    let mut seen = Vec::new();
    for _ in 0..24 {
        key(&mut ui, UiKey::Right);
        seen.push(ui.settings.video.fps_cap);
    }
    for expected in [280, 320, 600, 0] {
        assert!(
            seen.contains(&expected),
            "cycling never hit {expected}: {seen:?}"
        );
    }

    // Click the slider's rightmost stop = Unlimited.
    ui.render(w, h);
    let r = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::VideoFpsSlider)
        .unwrap()
        .rect;
    ui.handle_input(UiInputEvent::Click {
        x: r.x + r.w as i32 - 1,
        y: r.y + r.h as i32 / 2,
    });
    assert_eq!(ui.settings.video.fps_cap, 0);

    // Direct numeric input: click field, type 500, Enter.
    click(&mut ui, WidgetId::VideoFpsField, w, h);
    assert!(ui.fps_field_active());
    for c in "500".chars() {
        ui.handle_input(UiInputEvent::CharTyped(c));
    }
    let acts = key(&mut ui, UiKey::Enter);
    assert_eq!(acts, vec![UiAction::SettingsChanged]);
    assert_eq!(ui.settings.video.fps_cap, 500);
    assert!(!ui.fps_field_active());

    // Out-of-range values are rejected, not clamped.
    click(&mut ui, WidgetId::VideoFpsField, w, h);
    for c in "9".chars() {
        ui.handle_input(UiInputEvent::CharTyped(c));
    }
    key(&mut ui, UiKey::Enter);
    assert!(ui.fps_field_error());
    assert_eq!(ui.settings.video.fps_cap, 500, "rejected value applied");
    for c in "9999".chars() {
        ui.handle_input(UiInputEvent::CharTyped(c));
    }
    key(&mut ui, UiKey::Enter);
    assert!(ui.fps_field_error());
    assert_eq!(ui.settings.video.fps_cap, 500);
}

#[test]
fn qol_tab_defaults_and_toggles() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    assert!(!ui.settings.qol.prevent_tool_break);
    assert_eq!(ui.settings.qol.crash_report_mode, CrashReportMode::Ask);

    click(&mut ui, WidgetId::TitleSettings, w, h);
    click(&mut ui, WidgetId::TabQol, w, h);
    assert!(matches!(ui.screen, Screen::Settings(SettingsTab::Qol)));

    let acts = click(&mut ui, WidgetId::QolPreventBreak, w, h);
    assert_eq!(acts, vec![UiAction::SettingsChanged]);
    assert!(ui.settings.qol.prevent_tool_break);

    click(&mut ui, WidgetId::QolCrashMode, w, h);
    assert_eq!(
        ui.settings.qol.crash_report_mode,
        CrashReportMode::AutoCrashOnly
    );

    // Chat history slider clamps inside 100..=5000.
    ui.render(w, h);
    let idx = ui
        .hit_regions
        .iter()
        .position(|r| r.id == WidgetId::QolChatHistory)
        .unwrap();
    ui.focus_index = Some(idx);
    for _ in 0..60 {
        key(&mut ui, UiKey::Right);
    }
    assert_eq!(ui.settings.qol.chat_history_lines, 5000);
    for _ in 0..60 {
        key(&mut ui, UiKey::Left);
    }
    assert_eq!(ui.settings.qol.chat_history_lines, 100);
}

#[test]
fn fairplay_tab_defaults_and_config() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    assert_eq!(
        ui.settings.fair_play.minimap_mode,
        MinimapMode::Unrestricted
    );
    assert_eq!(
        ui.settings.fair_play.player_radar_scope,
        PlayerRadarScope::All
    );

    click(&mut ui, WidgetId::TitleSettings, w, h);
    click(&mut ui, WidgetId::TabFairPlay, w, h);

    click(&mut ui, WidgetId::FpMinimapMode, w, h);
    assert_eq!(ui.settings.fair_play.minimap_mode, MinimapMode::FairPlay);

    click(&mut ui, WidgetId::FpRadarScope, w, h);
    assert_eq!(
        ui.settings.fair_play.player_radar_scope,
        PlayerRadarScope::PartyOnly
    );

    click(&mut ui, WidgetId::FpCaveMap, w, h);
    assert!(!ui.settings.fair_play.allow_cave_map);
}

#[test]
fn language_tab_cycles() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    click(&mut ui, WidgetId::TitleSettings, w, h);
    click(&mut ui, WidgetId::TabLanguage, w, h);
    click(&mut ui, WidgetId::LanguageChoice, w, h);
    assert_eq!(ui.settings.language, LanguageSetting::English);
    click(&mut ui, WidgetId::LanguageChoice, w, h);
    assert_eq!(ui.settings.language, LanguageSetting::Japanese);
    assert_eq!(ui.lang(), UiLang::Ja);
    click(&mut ui, WidgetId::LanguageChoice, w, h);
    assert_eq!(ui.settings.language, LanguageSetting::Auto);
    assert_eq!(ui.lang(), UiLang::En);
}

#[test]
fn mod_sync_requires_explicit_accept() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.pending_mod_sync = vec![
        ModSyncEntry::added("create", "aa".repeat(32)),
        ModSyncEntry::added("jei", "bb".repeat(32)),
    ];
    ui.screen = Screen::ModSyncConsent;
    ui.render(w, h);

    // Clicking dead space installs nothing; Escape backs out without
    // installing either.
    ui.handle_input(UiInputEvent::Click { x: 2, y: 2 });
    key(&mut ui, UiKey::Escape);
    assert_eq!(ui.pending_mod_sync.len(), 2);
    assert_eq!(ui.mod_sync_decision, None);

    // Decline produces the decline action and installs nothing.
    ui.screen = Screen::ModSyncConsent;
    ui.nav_stack.clear();
    let acts = click(&mut ui, WidgetId::ModSyncDecline, w, h);
    assert_eq!(acts, vec![UiAction::DeclineModSync]);
    assert_eq!(ui.pending_mod_sync.len(), 2);

    // Accept emits ApplyModSync with the full entry list.
    let acts = click(&mut ui, WidgetId::ModSyncAccept, w, h);
    assert_eq!(ui.mod_sync_decision, Some(true));
    match &acts[..] {
        [UiAction::ApplyModSync(entries)] => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name, "create");
        }
        other => panic!("expected ApplyModSync, got {other:?}"),
    }
    assert!(ui.pending_mod_sync.is_empty());
}

#[test]
fn mod_sync_missing_dependencies_require_an_explicit_click() {
    let mut ui = UiState::new();
    ui.screen = Screen::ModSyncConsent;
    for &(w, h) in &HIT_SIZES {
        ui.render(w, h);
        assert!(!ui
            .hit_regions
            .iter()
            .any(|r| r.id == WidgetId::ModSyncDownloadMissing));
    }

    ui.missing_mod_deps = vec!["create".into(), "fabric-api".into()];
    for &(w, h) in &HIT_SIZES {
        ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        assert!(ui
            .hit_regions
            .iter()
            .any(|r| r.id == WidgetId::ModSyncDownloadMissing));
        assert_eq!(ui.mod_sync_decision, None);
        assert_eq!(
            click(&mut ui, WidgetId::ModSyncDownloadMissing, w, h),
            vec![UiAction::DownloadMissingFromModrinth(vec![
                "create".into(),
                "fabric-api".into(),
            ])]
        );
        assert_eq!(ui.missing_mod_deps.len(), 2);
        assert_eq!(ui.mod_sync_decision, None);
    }
}

#[test]
fn crash_recovery_gates_send_on_confirmation() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.show_crash(CrashContext::new(
        "panic",
        "create",
        "1.21.1",
        "NPE at /home/alice/x.jar Bearer tok123 192.168.4.9",
    ));
    assert!(matches!(ui.screen, Screen::CrashRecovery));

    // Preview is fingerprinted, redacted, and capped at 4 KiB.
    let preview = ui.crash_preview();
    let fp = ui.crash_fingerprint();
    assert!(fp.starts_with("MEGU-E-"));
    assert_eq!(fp.len(), "MEGU-E-".len() + 8);
    assert!(preview.contains(&fp));
    assert!(!preview.contains("alice"));
    assert!(!preview.contains("tok123"));
    assert!(!preview.contains("192.168.4.9"));
    assert!(preview.len() <= CRASH_PREVIEW_MAX_BYTES);

    // Clicking Send Report opens the modal; NO submit action yet.
    ui.render(w, h);
    let send_rect = ui
        .hit_regions
        .iter()
        .find(|r| r.id == WidgetId::CrashSendReport)
        .unwrap()
        .rect;
    let acts = click(&mut ui, WidgetId::CrashSendReport, w, h);
    assert!(acts.is_empty(), "submitted without confirmation");
    assert_eq!(ui.modal, Some(UiModal::ConfirmSendCrashReport));

    // Underlying screen is inert: clicking where the send button was does
    // nothing now (only modal regions are live).
    ui.render(w, h);
    assert!(ui.hit_regions.iter().all(|r| r.modal));

    // Cancel path: reopen the modal, then cancel — still no submit.
    let acts = click(&mut ui, WidgetId::ModalCancel, w, h);
    assert!(acts.is_empty());
    assert_eq!(ui.modal, None);
    click(&mut ui, WidgetId::CrashSendReport, w, h);
    assert_eq!(ui.modal, Some(UiModal::ConfirmSendCrashReport));

    // OK emits SubmitCrashReport exactly once and disables resend (the
    // button goes disabled, so it no longer owns a hit region at all).
    let acts = click(&mut ui, WidgetId::ModalOk, w, h);
    assert_eq!(acts, vec![UiAction::SubmitCrashReport]);
    assert!(ui.crash_sent);
    ui.render(w, h);
    assert!(!ui
        .hit_regions
        .iter()
        .any(|r| r.id == WidgetId::CrashSendReport));
    let r = send_rect;
    let acts = ui.handle_input(UiInputEvent::Click {
        x: r.x + r.w as i32 / 2,
        y: r.y + r.h as i32 / 2,
    });
    assert!(acts.is_empty(), "double-send produced an action");

    // Return to Title goes back cleanly.
    let acts = click(&mut ui, WidgetId::CrashReturnTitle, w, h);
    assert_eq!(acts, vec![UiAction::OpenScreen(Screen::Title)]);
    assert_eq!(ui.screen, Screen::Title);
    assert!(ui.nav_stack.is_empty());
}

#[test]
fn friends_invite_and_join_flow() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.invite_seed = 0xdead_beef;
    ui.screen = Screen::Friends;

    let acts = click(&mut ui, WidgetId::FriendsGenerate, w, h);
    assert_eq!(acts, vec![UiAction::GenerateInvite]);
    let code = ui.active_invite.expect("no invite generated");
    let text = invite_code_display(code);
    assert_eq!(text.len(), "MEGU-XXXX-XXXX".len());
    assert!(text.starts_with("MEGU-"));
    assert_eq!(invite_code_parse(&text), Some(code));

    // Tolerant parses: lowercase, no hyphens, homoglyphs.
    assert_eq!(
        invite_code_parse(&text.to_lowercase().replace('-', "")),
        Some(code)
    );

    // Copy + revoke.
    let acts = click(&mut ui, WidgetId::FriendsCopy, w, h);
    assert_eq!(acts, vec![UiAction::CopyInvite(text.clone())]);
    let acts = click(&mut ui, WidgetId::FriendsRevoke, w, h);
    assert_eq!(acts, vec![UiAction::RevokeInvite]);
    assert_eq!(ui.active_invite, None);

    // Guest join: a too-short code errors, a good code joins. (Note the
    // parser is deliberately tolerant — "not-a-code" would normalize to
    // eight valid Crockford digits, so this test uses a bad length.)
    ui.join_code_text = "nope".to_string();
    let acts = click(&mut ui, WidgetId::FriendsJoin, w, h);
    assert!(acts.is_empty());
    assert!(ui.join_error);

    click(&mut ui, WidgetId::FriendsJoinField, w, h);
    ui.join_code_text.clear();
    for c in text.chars() {
        ui.handle_input(UiInputEvent::CharTyped(c));
    }
    let acts = key(&mut ui, UiKey::Enter);
    assert_eq!(acts, vec![UiAction::JoinWithCode(code)]);
}

#[test]
fn keyboard_focus_and_gamepad_navigation() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);
    ui.render(w, h);
    assert!(ui.focused().is_none());

    key(&mut ui, UiKey::Tab);
    assert!(ui.focused().is_some());
    let first = ui.focused().unwrap();
    key(&mut ui, UiKey::Down);
    let second = ui.focused().unwrap();
    assert_ne!(first, second);
    key(&mut ui, UiKey::Up);
    assert_eq!(ui.focused(), Some(first));

    // Enter activates the focused widget.
    ui.render(w, h);
    let idx = ui
        .hit_regions
        .iter()
        .position(|r| r.id == WidgetId::TitleSettings)
        .unwrap();
    ui.focus_index = Some(idx);
    let acts = key(&mut ui, UiKey::Enter);
    assert_eq!(
        acts,
        vec![UiAction::OpenScreen(Screen::Settings(SettingsTab::Video))]
    );

    // Gamepad: dpad navigation + South to activate + Start to escape.
    ui.handle_input(UiInputEvent::GamepadPressed(GamepadButton::DpadRight));
    ui.handle_input(UiInputEvent::GamepadPressed(GamepadButton::Start));
    assert_eq!(ui.screen, Screen::Title);
}

#[test]
fn escape_navigation_and_pause() {
    let mut ui = UiState::new();
    let (w, h) = (1280u32, 720u32);

    ui.navigate(Screen::InGameHud);
    let acts = key(&mut ui, UiKey::Escape);
    assert_eq!(acts, vec![UiAction::OpenScreen(Screen::Pause)]);
    assert_eq!(ui.screen, Screen::Pause);

    // Save & Quit returns to title.
    let acts = click(&mut ui, WidgetId::PauseSaveQuit, w, h);
    assert_eq!(acts, vec![UiAction::OpenScreen(Screen::Title)]);
    assert_eq!(ui.screen, Screen::Title);
}

#[test]
fn hud_tab_list_toggles_and_rasterizes() {
    let mut ui = UiState::new();
    ui.hud = mc_spine_client_ui::state::HudState::demo();
    ui.screen = Screen::InGameHud;
    ui.render(1280, 720);
    assert!(ui.hit_regions.is_empty(), "HUD must not eat clicks");

    key(&mut ui, UiKey::Tab);
    assert!(ui.hud.show_player_list);
    let rgba = rasterize_draw_list(1280, 720, &ui.render(1280, 720));
    assert_non_blank(&rgba);
    key(&mut ui, UiKey::Tab);
    assert!(!ui.hud.show_player_list);
}

// ---------------------------------------------------------------------------
// MiniHUD + directional subtitles (both strictly opt-in)
// ---------------------------------------------------------------------------

#[test]
fn minihud_and_subtitles_default_off_and_emit_zero_commands() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    assert!(!ui.settings.qol.mini_hud.enabled);
    assert!(!ui.settings.qol.show_subtitles);
    assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Off);
    assert!(ui.hud.mini_hud_data.fps.is_none());
    assert!(ui.hud.subtitles.is_empty());
    assert!(!mc_spine_client_ui::state::QolSettings::default()
        .mini_hud
        .enabled);

    for &(w, h) in &HIT_SIZES {
        let baseline = ui.render(w, h);
        // Fully populated data stays invisible while both features off.
        ui.hud.mini_hud_data = populated_mini_hud_data();
        ui.hud.subtitles = demo_subtitles();
        let drawn = ui.render(w, h);
        assert_eq!(
            drawn, baseline,
            "opt-out overlays emitted commands at {w}x{h}"
        );
        ui.hud.mini_hud_data = MiniHudData::default();
        ui.hud.subtitles.clear();
    }
}

#[test]
fn qol_tab_wires_minihud_preset_and_subtitles_without_overlap() {
    let mut ui = UiState::new();
    for &(w, h) in &HIT_SIZES {
        ui.screen = Screen::Settings(SettingsTab::Qol);
        let list = ui.render(w, h);
        assert_hit_regions_within_viewport(&ui.hit_regions, w, h);
        assert_draws_within_viewport(&list, w, h);
        for id in [WidgetId::QolMiniHud, WidgetId::QolSubtitles] {
            assert!(
                ui.hit_regions.iter().any(|r| r.id == id),
                "{id:?} missing at {w}x{h}"
            );
        }
        for label in ["MiniHUD: OFF", "Subtitles: OFF"] {
            assert!(
                list.commands
                    .iter()
                    .any(|c| matches!(c, UiCommand::Text { text, .. } if text == label)),
                "missing {label} at {w}x{h}"
            );
        }
    }

    // Cycle the MiniHUD preset OFF → Basic → Advanced → OFF.
    ui.screen = Screen::Settings(SettingsTab::Qol);
    assert_eq!(
        click(&mut ui, WidgetId::QolMiniHud, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Basic);
    assert!(ui.settings.qol.mini_hud.enabled);
    assert!(ui.settings.qol.mini_hud.show_fps);
    assert!(!ui.settings.qol.mini_hud.show_ram);
    let list = ui.render(640, 360);
    assert!(list
        .commands
        .iter()
        .any(|c| matches!(c, UiCommand::Text { text, .. } if text == "MiniHUD: Basic")));

    assert_eq!(
        click(&mut ui, WidgetId::QolMiniHud, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Advanced);
    let m = &ui.settings.qol.mini_hud;
    assert!(
        m.show_one_percent_low
            && m.show_mspt_tps
            && m.show_ram
            && m.show_cpu
            && m.show_gpu
            && m.show_vram
            && m.show_chunks_entities
            && m.show_light_level
            && m.show_shader_audio_status
    );

    assert_eq!(
        click(&mut ui, WidgetId::QolMiniHud, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Off);
    assert!(!ui.settings.qol.mini_hud.enabled);

    // Subtitles toggle flips through the real input router.
    assert!(!ui.settings.qol.show_subtitles);
    assert_eq!(
        click(&mut ui, WidgetId::QolSubtitles, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert!(ui.settings.qol.show_subtitles);
    assert_eq!(
        click(&mut ui, WidgetId::QolSubtitles, 640, 360),
        vec![UiAction::SettingsChanged]
    );
    assert!(!ui.settings.qol.show_subtitles);
}

#[test]
fn minihud_basic_and_advanced_render_in_bounds_at_every_anchor() {
    for anchor in [
        MiniHudAnchor::TopLeft,
        MiniHudAnchor::TopRight,
        MiniHudAnchor::BottomLeft,
        MiniHudAnchor::BottomRight,
    ] {
        let mut ui = UiState::new();
        ui.screen = Screen::InGameHud;
        ui.hud = mc_spine_client_ui::state::HudState::demo();
        ui.hud.mini_hud_data = populated_mini_hud_data();
        ui.settings.qol.mini_hud = MiniHudConfig {
            enabled: true,
            anchor,
            ..MiniHudConfig::default()
        };
        for &(w, h) in &HIT_SIZES {
            ui.settings.qol.mini_hud = MiniHudConfig {
                enabled: false,
                anchor,
                ..MiniHudConfig::default()
            };
            let baseline = ui.render(w, h);
            ui.settings.qol.mini_hud.enabled = true;
            let drawn = ui.render(w, h);
            assert!(
                drawn.len() > baseline.len(),
                "Basic MiniHUD drew nothing at {w}x{h} ({anchor:?})"
            );
            assert_draws_within_viewport(&drawn, w, h);
            for needle in ["60 FPS", "XYZ 10.0 64.0 -3.0", "Ping 24ms"] {
                assert!(
                    drawn.commands.iter().any(|c| matches!(c,
                        UiCommand::Text { text, .. } if text.contains(needle))),
                    "missing {needle} at {w}x{h} ({anchor:?})"
                );
            }
            assert_non_blank(&rasterize_draw_list(w, h, &drawn));

            // Advanced preset adds the hardware/world segments.
            ui.settings.qol.cycle_mini_hud();
            assert_eq!(ui.settings.qol.mini_hud_preset(), MiniHudPreset::Advanced);
            let adv = ui.render(w, h);
            assert!(adv.len() > drawn.len());
            assert_draws_within_viewport(&adv, w, h);
            for needle in [
                "1% low 9 FPS",
                "4.2 mspt  19.8 TPS",
                "RAM 512/1024 MiB",
                "CPU 9%",
                "GPU Test GPU 55%",
                "VRAM 512/1024 MiB",
                "C 123  E 45",
                "Light B15 S15",
                "Shader megu  Audio ok",
            ] {
                assert!(
                    adv.commands.iter().any(|c| matches!(c,
                        UiCommand::Text { text, .. } if text.contains(needle))),
                    "missing {needle} at {w}x{h} ({anchor:?})"
                );
            }
            assert_non_blank(&rasterize_draw_list(w, h, &adv));
        }
    }
}

#[test]
fn directional_subtitles_render_markers_and_cap_at_four_rows() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud = mc_spine_client_ui::state::HudState::demo();
    // Six entries: the two oldest must be trimmed to the 4-row cap.
    ui.hud.subtitles = vec![
        HudSubtitle {
            label: "Trimmed A".into(),
            direction: SubtitleDirection::Center,
        },
        HudSubtitle {
            label: "Trimmed B".into(),
            direction: SubtitleDirection::Center,
        },
    ];
    ui.hud.subtitles.extend(demo_subtitles());

    for &(w, h) in &HIT_SIZES {
        ui.settings.qol.show_subtitles = false;
        let baseline = ui.render(w, h);
        ui.settings.qol.show_subtitles = true;
        let drawn = ui.render(w, h);
        assert!(
            drawn.len() > baseline.len(),
            "subtitles drew nothing at {w}x{h}"
        );
        assert_draws_within_viewport(&drawn, w, h);
        assert_non_blank(&rasterize_draw_list(w, h, &drawn));
        for needle in [
            "< Zombie groans",
            "Creeper hiss",
            "Skeleton shoots >",
            "v Witch giggles",
        ] {
            assert!(
                drawn
                    .commands
                    .iter()
                    .any(|c| matches!(c, UiCommand::Text { text, .. } if text == needle)),
                "missing {needle} at {w}x{h}"
            );
        }
        assert!(
            !drawn
                .commands
                .iter()
                .any(|c| matches!(c, UiCommand::Text { text, .. } if text.contains("Trimmed"))),
            "subtitle rows past the cap leaked at {w}x{h}"
        );
    }
}

#[test]
fn font_covers_ascii_and_fallback() {
    assert_eq!(glyph_for('A').len(), 8);
    // 'A' has ink; ' ' has none; non-ASCII gets the box.
    assert!(glyph_for('A').iter().any(|&b| b != 0));
    assert!(glyph_for(' ').iter().all(|&b| b == 0));
    assert!(glyph_for('あ').iter().any(|&b| b != 0));
    assert_ne!(glyph_for('あ'), glyph_for('A'));
    assert_eq!(text_width("MC-SPINE", 1), 8 * 8);
    assert_eq!(text_width("ab\ncd", 2), 4 * 8);
}

#[test]
fn png_rgba_roundtrip_shape() {
    let (w, h) = (8u32, 4u32);
    let rgba = vec![0xabu8; (w * h * 4) as usize];
    let png = encode_png_rgba(w, h, &rgba);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), w);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), h);
    assert_eq!(png[25], 6); // color type RGBA
    assert!(png.windows(4).any(|c| c == b"IEND"));
    // Mismatched buffer must not emit a corrupt file.
    assert!(encode_png_rgba(w, h, &rgba[..rgba.len() - 1]).is_empty());
}

#[test]
fn tessellation_and_atlas_are_real() {
    let mut list = UiDrawList::new();
    list.fill_rect(
        UiRect::new(0, 0, 64, 32),
        mc_spine_client_ui::draw::UiColor::rgb(255, 0, 0),
    );
    list.text(
        2,
        2,
        1,
        mc_spine_client_ui::draw::UiColor::rgb(255, 255, 255),
        "Hi",
    );
    list.progress_bar(
        UiRect::new(0, 40, 100, 10),
        0.5,
        mc_spine_client_ui::draw::UiColor::rgb(0, 255, 0),
        mc_spine_client_ui::draw::UiColor::rgb(0, 0, 0),
        mc_spine_client_ui::draw::UiColor::rgb(80, 80, 80),
    );
    let mesh = tessellate_draw_list(640, 360, &list);
    assert!(!mesh.vertices.is_empty());
    assert!(mesh.indices.len().is_multiple_of(3));
    // Text produces glyph quads whose UVs differ from the white tile.
    let has_glyph_uv = mesh.vertices.iter().any(|v| v.uv[0] > 0.0 || v.uv[1] > 0.0);
    assert!(has_glyph_uv);

    let atlas = font_atlas_rgba();
    assert_eq!(
        atlas.len(),
        (FONT_ATLAS_WIDTH * FONT_ATLAS_HEIGHT * 4) as usize
    );
    let inked = atlas
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] == 255)
        .count();
    assert!(inked > 500, "atlas has no glyph ink");
}

#[test]
fn draw_commands_match_spec_shape() {
    // The command vocabulary the spec pins down.
    let mut list = UiDrawList::new();
    list.push(UiCommand::FillRect {
        rect: UiRect::new(0, 0, 10, 10),
        color: mc_spine_client_ui::draw::UiColor::rgb(1, 2, 3),
    });
    list.push(UiCommand::BorderRect {
        rect: UiRect::new(0, 0, 10, 10),
        color: mc_spine_client_ui::draw::UiColor::rgb(255, 255, 255),
        thickness: 1,
    });
    list.push(UiCommand::Text {
        x: 0,
        y: 0,
        scale: 1,
        color: mc_spine_client_ui::draw::UiColor::rgb(255, 255, 255),
        text: "ok".to_string(),
    });
    assert_eq!(list.len(), 3);
    let rgba = rasterize_draw_list(32, 32, &list);
    assert_non_blank(&rgba);
}

#[test]
fn render_into_reuses_draw_list_buffers_and_recycles_text_heaps() {
    let mut ui = UiState::new();
    ui.hud = mc_spine_client_ui::state::HudState::demo();
    ui.navigate(Screen::InGameHud);

    let mut list = UiDrawList::new();
    ui.render_into(1280, 720, &mut list);
    let first_commands = list.commands.clone();
    // The demo HUD emits text commands whose payloads can be recycled.
    let text_count = first_commands
        .iter()
        .filter(|c| matches!(c, UiCommand::Text { .. }))
        .count();
    assert!(text_count > 0);

    // Re-render into the same list: commands stay identical and the
    // recycled pool consumed as many heaps as this frame emitted.
    ui.render_into(1280, 720, &mut list);
    assert_eq!(list.commands, first_commands);
    let pool_after_second = list.recycled_text_buffer_count();
    assert!(
        pool_after_second <= text_count,
        "pool holds at most one frame's worth of payloads"
    );

    // Clearing recycles every Text payload; text_str drains them back.
    let cap_before = list.commands.capacity();
    list.clear();
    assert!(list.commands.is_empty());
    assert_eq!(list.commands.capacity(), cap_before);
    assert_eq!(list.recycled_text_buffer_count(), text_count);
    list.text_str(
        0,
        0,
        1,
        mc_spine_client_ui::draw::UiColor::rgb(1, 2, 3),
        "hud",
    );
    assert_eq!(list.recycled_text_buffer_count(), text_count - 1);
    match &list.commands[0] {
        UiCommand::Text { text, .. } => assert_eq!(text, "hud"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn render_into_matches_render_output_on_every_screen() {
    for &(w, h) in &SIZES {
        let mut ui = UiState::new();
        for screen in all_screens(&mut ui) {
            ui.navigate(screen);
            let allocated = ui.render(w, h);
            let mut reused = UiDrawList::new();
            ui.render_into(w, h, &mut reused);
            assert_eq!(reused.commands, allocated.commands, "{screen:?} at {w}x{h}");
        }
    }
}
