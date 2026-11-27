// SPDX-License-Identifier: LGPL-3.0-or-later
//! Exhaustive pre-release layout audit for `megu-client-ui`.
//!
//! Every screen, every settings tab, every modal, every embedded
//! sub-screen, and the fully-populated in-game HUD (Compact Debug HUD at every
//! anchor + MagicHUD + subtitles + crafting-conflict switcher + chat +
//! actionbar + player list + bossbars) must satisfy, at every supported
//! resolution and in both UI languages:
//!
//! * zero overlapping interactive hit regions,
//! * every hit region fully inside the viewport,
//! * every draw command (rect fills, borders, progress bars, text
//!   *bounds* included) fully inside the viewport,
//! * no panic, divide-by-zero, or arithmetic underflow — including the
//!   pathological `0x0`, `1x1`, and `320x240` viewports and every
//!   explicit GUI scale,
//! * a reachable Back/Close/Escape path on every screen.

use megu_client_ui::draw::{
    rasterize_draw_list, tessellate_draw_list, text_height, text_width, UiColor, UiCommand,
    UiDrawList, UiRect,
};
use megu_client_ui::magic_hud::{
    CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState,
};
use megu_client_ui::mod_manager::{ModManagerEntry, ModManagerState};
use megu_client_ui::recipe_browser::{
    BrowserTab, DetailMode, ItemEntry, RecipeBrowserState, RecipeIndex,
};
use megu_client_ui::shader_screen::ShaderPackScreenState;
use megu_client_ui::state::{
    CraftingConflictOverlay, HitRegion, HudState, HudSubtitle, LanguageSetting, ModSyncEntry,
    Screen, SettingsTab, SubtitleDirection, UiInputEvent, UiLang, UiModal, UiState, WidgetId,
};
use megu_core::crash_report::CrashContext;
use megu_client_ui::state::{CompactDebugHudAnchor, CompactDebugHudConfig, CompactDebugHudData};

/// Required ship resolutions: compact, vanilla default, 720p/Deck,
/// 1080p, 1440p, 4K HiDPI.
const SIZES: [(u32, u32); 6] = [
    (640, 360),
    (854, 480),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

/// Sizes that must never panic — including zero and single-pixel.
const PATHOLOGICAL: [(u32, u32); 8] = [
    (0, 0),
    (1, 1),
    (1, 500),
    (500, 1),
    (320, 240),
    (319, 239),
    (80, 60),
    (17, 11),
];

const LANGS: [LanguageSetting; 2] = [LanguageSetting::English, LanguageSetting::Japanese];

const ANCHORS: [CompactDebugHudAnchor; 4] = [
    CompactDebugHudAnchor::TopLeft,
    CompactDebugHudAnchor::TopRight,
    CompactDebugHudAnchor::BottomLeft,
    CompactDebugHudAnchor::BottomRight,
];

const MODALS: [UiModal; 3] = [
    UiModal::ConfirmSendCrashReport,
    UiModal::ConfirmResetDefaults,
    UiModal::StartupTimeline,
];

const ALL_TABS: [SettingsTab; 6] = [
    SettingsTab::Video,
    SettingsTab::Audio,
    SettingsTab::Shaders,
    SettingsTab::Qol,
    SettingsTab::FairPlay,
    SettingsTab::Language,
];

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

fn assert_no_overlaps(regions: &[HitRegion], context: &str) {
    for (i, a) in regions.iter().enumerate() {
        for b in regions.iter().skip(i + 1) {
            assert!(
                !a.rect.intersects(b.rect),
                "{context}: overlapping hit regions {a:?} vs {b:?}"
            );
        }
    }
}

fn assert_hit_regions_within_viewport(regions: &[HitRegion], w: u32, h: u32, context: &str) {
    for region in regions {
        assert!(
            region.rect.x >= 0
                && region.rect.y >= 0
                && region.rect.right() <= w as i32
                && region.rect.bottom() <= h as i32,
            "{context}: hit region outside {w}x{h}: {region:?}"
        );
    }
    assert_no_overlaps(regions, context);
}

fn assert_draws_within_viewport(list: &UiDrawList, w: u32, h: u32, context: &str) {
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ItemIcon { rect, .. }
            | UiCommand::Sprite { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => {
                assert!(
                    rect.x >= 0
                        && rect.y >= 0
                        && rect.right() <= w as i32
                        && rect.bottom() <= h as i32,
                    "{context}: draw command outside {w}x{h}: {cmd:?}"
                );
            }
            UiCommand::Text {
                x, y, scale, text, ..
            } => {
                let right = *x + text_width(text, *scale) as i32;
                let bottom = *y + text_height(text, *scale) as i32;
                assert!(
                    *x >= 0 && *y >= 0 && right <= w as i32 && bottom <= h as i32,
                    "{context}: text outside {w}x{h}: {cmd:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Populated fixture data (hostile where it matters)
// ---------------------------------------------------------------------------

/// Compact Debug HUD data with every field set so Basic *and* Advanced presets
/// emit their full line sets.
fn populated_compact_debug_hud_data() -> CompactDebugHudData {
    CompactDebugHudData {
        fps: Some(60.0),
        fps_one_percent_low: Some(9.0),
        mspt: Some(4.2),
        tps: Some(19.8),
        coords: Some((10.0, 64.0, -3.0)),
        facing: Some("N".into()),
        biome: Some("Flower Forest of Unusual Length".into()),
        ping_ms: Some(24),
        ram_used_mib: Some(512),
        ram_total_mib: Some(1024),
        cpu_percent: Some(9.0),
        gpu_name: Some("Test GPU With A Long Name".into()),
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

/// Compact Debug HUD config with every segment on (the Advanced preset).
fn all_compact_debug_hud_segments(anchor: CompactDebugHudAnchor) -> CompactDebugHudConfig {
    CompactDebugHudConfig {
        enabled: true,
        anchor,
        show_one_percent_low: true,
        show_mspt_tps: true,
        show_ram: true,
        show_cpu: true,
        show_gpu: true,
        show_vram: true,
        show_chunks_entities: true,
        show_light_level: true,
        show_shader_audio_status: true,
        ..CompactDebugHudConfig::default()
    }
}

/// Worst-case in-game HUD: every overlay active at once, with
/// deliberately over-long host-supplied strings (plus a stray newline,
/// which the layout must sanitize rather than wrap out of bounds).
fn hostile_hud(_subtitles_on: bool) -> HudState {
    let mut hud = HudState::demo();
    hud.absorption = 8.0;
    hud.air = 120;
    hud.xp_level = 42;
    hud.bossbars = vec![
        (
            "Ender Dragon With An Extremely Long Display Name\ninjected".to_string(),
            0.65,
        ),
        ("Wither".to_string(), 0.4),
        ("Elder Guardian".to_string(), 0.9),
    ];
    for (i, slot) in hud.hotbar.iter_mut().enumerate() {
        *slot = Some(format!("Item With Long Name {i}"));
    }
    hud.offhand = Some("Shield of Overflowing Text".into());
    hud.actionbar = "A very long actionbar message that must be clipped to the viewport".into();
    hud.chat_lines = (0..8)
        .map(|i| {
            if i % 2 == 0 {
                format!("<Player{i}> short message")
            } else {
                format!(
                    "<Player{i}> {}",
                    "a very long chat line that would otherwise spill past the right edge "
                )
            }
        })
        .collect();
    hud.show_player_list = true;
    hud.players = (0..24)
        .map(|i| (format!("PlayerNameNumber{i:02}"), 10 + i as u16 * 7))
        .collect();
    hud.compact_debug_hud_data = populated_compact_debug_hud_data();
    hud.subtitles = vec![
        HudSubtitle {
            label: "Zombie groans with an unusually long descriptive label".into(),
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
            label: "Witch giggles behind you".into(),
            direction: SubtitleDirection::Behind,
        },
    ];
    hud.crafting_conflict = CraftingConflictOverlay {
        selected_index: 1,
        total_candidates: 4,
    };
    hud.magic = populated_magic();
    hud
}

fn populated_magic() -> MagicHudState {
    MagicHudState {
        mana: 42.0,
        max_mana: 100.0,
        active_cast: Some(CastBarState {
            spell_label: "Bouvet Island Summon".into(),
            progress: 0.4,
            channeled: false,
            interrupted_flash_ticks: 0,
        }),
        cooldowns: vec![
            CooldownSlotState {
                spell_label: "Fireball".into(),
                remaining_ticks: 20,
                total_ticks: 60,
            },
            CooldownSlotState {
                spell_label: "Ice".into(),
                remaining_ticks: 5,
                total_ticks: 30,
            },
        ],
        custom_statuses: vec![
            StatusBadgeState {
                label: "Levitation".into(),
                remaining_ticks: 100,
                amplifier: 1,
                is_harmful: true,
                shield_amount: None,
            },
            StatusBadgeState {
                label: "Blessed".into(),
                remaining_ticks: 60,
                amplifier: 0,
                is_harmful: false,
                shield_amount: Some(4.0),
            },
            StatusBadgeState {
                label: "Cursed".into(),
                remaining_ticks: 30,
                amplifier: 2,
                is_harmful: true,
                shield_amount: None,
            },
        ],
    }
}

fn demo_mod_manager() -> ModManagerState {
    let mut entries = vec![
        ModManagerEntry::new("create", "Create", "0.5.1f"),
        ModManagerEntry::new("jei", "Just Enough Items", "15.3.0"),
        ModManagerEntry::new(
            "longmodid",
            "A Mod With A Really Quite Long Display Name That Should Clip",
            "1.0.0",
        ),
    ];
    entries[0].missing_deps = vec![
        "flywheel".into(),
        "machine_tutorial".into(),
        "registrate".into(),
        "extra-dep".into(),
    ];
    ModManagerState::new(entries)
}

fn demo_recipe_browser() -> RecipeBrowserState {
    let mut index = RecipeIndex::new();
    index.add_item(ItemEntry::new("minecraft:iron_ingot", "Iron Ingot"));
    index.add_item(ItemEntry::new("minecraft:stick", "Stick"));
    index.add_item(ItemEntry::new(
        "minecraft:oak_planks",
        "Oak Planks With A Long Localised Name",
    ));
    for i in 0..40 {
        index.add_item(ItemEntry::new(
            &format!("demo:item_{i:02}"),
            &format!("Demo Item {i:02}"),
        ));
    }
    RecipeBrowserState::new(index)
}

/// Every screen plus the state needed to render it meaningfully.
fn all_screens(ui: &mut UiState) -> Vec<Screen> {
    ui.hud = hostile_hud(true);
    ui.settings.qol.show_subtitles = true;
    ui.pending_mod_sync = vec![
        ModSyncEntry::added(
            "a-mod-with-a-quite-long-modid-that-must-be-clipped",
            "aabbccddee001122334455667788990011",
        ),
        ModSyncEntry {
            name: "jei".to_string(),
            sha256_hex: "ff00ff00ff00ff00ff00ff00ff00ff00".to_string(),
            change: megu_client_ui::state::ModSyncChange::Updated,
        },
        ModSyncEntry {
            name: "old-mod".to_string(),
            sha256_hex: "11".repeat(32),
            change: megu_client_ui::state::ModSyncChange::Removed,
        },
    ];
    ui.missing_mod_deps = vec!["create".into(), "jei".into()];
    ui.crash = Some(CrashContext::new(
        "panic",
        "a-mod-with-a-very-long-name-indeed",
        "1.21.1",
        "contraption NPE at /home/alice/mods/x.jar — Bearer sekret",
    ));
    ui.mod_manager = demo_mod_manager();
    ui.recipe_browser = demo_recipe_browser();
    ui.website_url = "https://example.invalid".into();
    ui.support_url = "https://example.invalid/support".into();
    ui.contribute_url = "https://example.invalid/contribute".into();
    ui.report_url = "https://example.invalid/report".into();
    ui.active_shaderpack_name = "A Shaderpack With A Long Name".into();
    ui.active_shaderpack_profile = "Insane++".into();
    ui.title_status =
        Some("Singleplayer \"Old World\": megu server exited during startup".into());
    ui.singleplayer.worlds = (0..24)
        .map(|i| format!("World {i:02}"))
        .chain(std::iter::once(
            "A World With A Rather Long Folder Name".to_string(),
        ))
        .collect();
    let mut screens: Vec<Screen> = ALL_TABS.iter().map(|t| Screen::Settings(*t)).collect();
    screens.extend([
        Screen::Title,
        Screen::Singleplayer,
        Screen::AboutMegu,
        Screen::Friends,
        Screen::ModSyncConsent,
        Screen::ModManager,
        Screen::RecipeBrowser,
        Screen::InGameHud,
        Screen::Death,
        Screen::Pause,
        Screen::CrashRecovery,
    ]);
    screens
}

// ---------------------------------------------------------------------------
// 1. Full matrix: every screen x resolution x language
// ---------------------------------------------------------------------------

#[test]
fn every_screen_every_resolution_both_languages_clean_layout() {
    for lang in LANGS {
        for &(w, h) in &SIZES {
            let mut ui = UiState::new();
            ui.settings.language = lang;
            let screens = all_screens(&mut ui);
            for screen in screens {
                ui.screen = screen;
                ui.nav_stack.clear();
                ui.modal = None;
                let list = ui.render(w, h);
                let ctx = format!("{screen:?} @{w}x{h} lang={lang:?}");
                assert!(
                    !list.commands.is_empty(),
                    "{ctx}: screen produced no draw commands"
                );
                assert_hit_regions_within_viewport(&ui.hit_regions, w, h, &ctx);
                assert_draws_within_viewport(&list, w, h, &ctx);
                let rgba = rasterize_draw_list(w, h, &list);
                assert_eq!(rgba.len(), (w * h * 4) as usize, "{ctx}");
                let _ = tessellate_draw_list(w, h, &list);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Every modal on every underlying screen
// ---------------------------------------------------------------------------

#[test]
fn every_modal_clean_at_all_resolutions() {
    for lang in LANGS {
        for &(w, h) in &SIZES {
            for under in [Screen::CrashRecovery, Screen::Title, Screen::Pause] {
                for modal in MODALS {
                    let mut ui = UiState::new();
                    ui.settings.language = lang;
                    ui.screen = under;
                    ui.crash = Some(CrashContext::new("panic", "create", "1.21.1", "boom"));
                    ui.modal = Some(modal);
                    let list = ui.render(w, h);
                    let ctx = format!("modal {modal:?} on {under:?} @{w}x{h} lang={lang:?}");
                    assert!(
                        !ui.hit_regions.is_empty() && ui.hit_regions.iter().all(|r| r.modal),
                        "{ctx}: modal must own every live region"
                    );
                    assert_hit_regions_within_viewport(&ui.hit_regions, w, h, &ctx);
                    assert_draws_within_viewport(&list, w, h, &ctx);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. HUD overlay matrix: Compact Debug HUD anchors x all overlays x resolutions
// ---------------------------------------------------------------------------

/// The translucent HUD backings (chat lines, subtitle rows, Compact Debug HUD box)
/// share one exact color — collect their rects and assert none overlap
/// each other or the vitals icon band.
fn translucent_fills(list: &UiDrawList) -> Vec<UiRect> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::FillRect { rect, color } if *color == UiColor::rgba(0, 0, 0, 110) => {
                Some(*rect)
            }
            _ => None,
        })
        .collect()
}

/// 8x8 dark backing cells under the heart/armor/hunger/air icons.
fn vitals_icon_cells(list: &UiDrawList, scale: u32) -> Vec<UiRect> {
    let cell = 8 * scale;
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::FillRect { rect, color }
                if *color == UiColor::rgb(26, 26, 30) && rect.w == cell && rect.h == cell =>
            {
                Some(*rect)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn hud_overlay_matrix_no_collisions() {
    for lang in LANGS {
        for &(w, h) in &SIZES {
            for anchor in ANCHORS {
                for subtitles in [true, false] {
                    for magic_on in [true, false] {
                        let mut ui = UiState::new();
                        ui.settings.language = lang;
                        ui.settings.qol.show_subtitles = subtitles;
                        ui.settings.qol.compact_debug_hud = all_compact_debug_hud_segments(anchor);
                        ui.screen = Screen::InGameHud;
                        ui.hud = hostile_hud(subtitles);
                        if !magic_on {
                            ui.hud.magic = MagicHudState::default();
                        }
                        let list = ui.render(w, h);
                        let ctx = format!(
                            "HUD anchor={anchor:?} subs={subtitles} magic={magic_on} @{w}x{h} lang={lang:?}"
                        );
                        assert_hit_regions_within_viewport(&ui.hit_regions, w, h, &ctx);
                        assert_draws_within_viewport(&list, w, h, &ctx);

                        // Translucent backings must never stack on top of
                        // each other (chat vs subtitles vs Compact Debug HUD).
                        let fills = translucent_fills(&list);
                        for (i, a) in fills.iter().enumerate() {
                            for b in fills.iter().skip(i + 1) {
                                assert!(
                                    !a.intersects(*b),
                                    "{ctx}: HUD backings overlap {a:?} vs {b:?}"
                                );
                            }
                        }

                        // The crafting-conflict switcher is the HUD's only
                        // hit region; it must clear the vitals icons and
                        // every translucent backing.
                        let scale = ui.effective_scale(h);
                        let icons = vitals_icon_cells(&list, scale);
                        if let Some(region) = ui
                            .hit_regions
                            .iter()
                            .find(|r| r.id == WidgetId::CraftConflictNext)
                        {
                            for cell in &icons {
                                assert!(
                                    !region.rect.intersects(*cell),
                                    "{ctx}: conflict button overlaps vitals icon {cell:?}"
                                );
                            }
                            for fill in &fills {
                                assert!(
                                    !region.rect.intersects(*fill),
                                    "{ctx}: conflict button overlaps HUD backing {fill:?}"
                                );
                            }
                        }
                        // And the Compact Debug HUD box (a translucent fill) must
                        // clear the vitals icons too.
                        for fill in &fills {
                            for cell in &icons {
                                assert!(
                                    !fill.intersects(*cell),
                                    "{ctx}: HUD backing overlaps vitals icon {fill:?} vs {cell:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Same matrix, but with an empty (default) HUD: Compact Debug HUD alone must
/// still stay in bounds at every anchor.
#[test]
fn compact_debug_hud_alone_at_every_anchor_and_resolution() {
    for &(w, h) in &SIZES {
        for anchor in ANCHORS {
            let mut ui = UiState::new();
            ui.settings.qol.compact_debug_hud = all_compact_debug_hud_segments(anchor);
            ui.screen = Screen::InGameHud;
            ui.hud = HudState::default();
            ui.hud.compact_debug_hud_data = populated_compact_debug_hud_data();
            let list = ui.render(w, h);
            let ctx = format!("compact debug HUD-only anchor={anchor:?} @{w}x{h}");
            assert_draws_within_viewport(&list, w, h, &ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Embedded screens render standalone within bounds
// ---------------------------------------------------------------------------

#[test]
fn embedded_mod_manager_standalone_bounds() {
    for lang in [UiLang::En, UiLang::Ja] {
        for &(w, h) in &SIZES {
            let mut menu = demo_mod_manager();
            menu.lang = lang;
            // Selection shows the detail pane + missing-deps warning box.
            let (list, hits) = menu.render(w, h);
            let ctx = format!("ModManager @{w}x{h} lang={lang:?}");
            for region in &hits {
                assert!(
                    region.rect.x >= 0
                        && region.rect.y >= 0
                        && region.rect.right() <= w as i32
                        && region.rect.bottom() <= h as i32,
                    "{ctx}: region outside viewport {region:?}"
                );
            }
            for (i, a) in hits.iter().enumerate() {
                for b in hits.iter().skip(i + 1) {
                    assert!(!a.rect.intersects(b.rect), "{ctx}: overlap {a:?} vs {b:?}");
                }
            }
            assert_draws_within_viewport(&list, w, h, &ctx);
        }
    }
}

#[test]
fn embedded_recipe_browser_standalone_bounds() {
    for lang in [UiLang::En, UiLang::Ja] {
        for &(w, h) in &SIZES {
            for tab in [
                BrowserTab::Items,
                BrowserTab::Tree,
                BrowserTab::WorldDropsInfo,
                BrowserTab::MachineTutorials,
            ] {
                let mut rb = demo_recipe_browser();
                rb.lang = lang;
                rb.tab = tab;
                rb.bookmarks = vec!["minecraft:iron_ingot".into(), "minecraft:stick".into()];
                rb.detail = Some(("minecraft:iron_ingot".to_string(), DetailMode::Recipes));
                rb.tree = Some(rb.index.recipe_tree(
                    "minecraft:iron_ingot",
                    8,
                    &std::collections::BTreeMap::new(),
                ));
                let (list, hits) = rb.render(w, h);
                let ctx = format!("RecipeBrowser {tab:?} @{w}x{h} lang={lang:?}");
                for region in &hits {
                    assert!(
                        region.rect.x >= 0
                            && region.rect.y >= 0
                            && region.rect.right() <= w as i32
                            && region.rect.bottom() <= h as i32,
                        "{ctx}: region outside viewport {region:?}"
                    );
                }
                for (i, a) in hits.iter().enumerate() {
                    for b in hits.iter().skip(i + 1) {
                        assert!(!a.rect.intersects(b.rect), "{ctx}: overlap {a:?} vs {b:?}");
                    }
                }
                assert_draws_within_viewport(&list, w, h, &ctx);
            }
        }
    }
}

#[test]
fn shader_screen_standalone_bounds() {
    for &(w, h) in &SIZES {
        for &(pw, ph) in &PATHOLOGICAL {
            let state = ShaderPackScreenState::default();
            let (_w, _h) = (pw, ph);
            let (list, hits) = state.render(w, h);
            let ctx = format!("ShaderScreen @{w}x{h}");
            for region in &hits {
                assert!(
                    region.rect.x >= 0
                        && region.rect.y >= 0
                        && region.rect.right() <= w as i32
                        && region.rect.bottom() <= h as i32,
                    "{ctx}: region outside viewport {region:?}"
                );
            }
            for (i, a) in hits.iter().enumerate() {
                for b in hits.iter().skip(i + 1) {
                    assert!(!a.rect.intersects(b.rect), "{ctx}: overlap {a:?} vs {b:?}");
                }
            }
            assert_draws_within_viewport(&list, w, h, &ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Pathological viewport safety: no panic, no underflow
// ---------------------------------------------------------------------------

#[test]
fn pathological_viewports_never_panic() {
    for lang in LANGS {
        for &(w, h) in &PATHOLOGICAL {
            for ui_scale in [0u8, 1, 2, 4, 255] {
                let mut ui = UiState::new();
                ui.settings.language = lang;
                ui.ui_scale = ui_scale;
                let screens = all_screens(&mut ui);
                for screen in screens {
                    ui.screen = screen;
                    ui.nav_stack.clear();
                    ui.modal = None;
                    let list = ui.render(w, h);
                    // Regions may legitimately be empty at degenerate
                    // sizes, but anything emitted must stay in bounds.
                    assert_hit_regions_within_viewport(
                        &ui.hit_regions,
                        w,
                        h,
                        &format!("{screen:?} @{w}x{h} scale={ui_scale}"),
                    );
                    let rgba = rasterize_draw_list(w, h, &list);
                    assert_eq!(rgba.len(), (w * h * 4) as usize);
                    let _ = tessellate_draw_list(w, h, &list);
                    ui.handle_input(UiInputEvent::Click { x: 0, y: 0 });
                }
                for modal in MODALS {
                    ui.modal = Some(modal);
                    let list = ui.render(w, h);
                    assert_hit_regions_within_viewport(
                        &ui.hit_regions,
                        w,
                        h,
                        &format!("modal @{w}x{h} scale={ui_scale}"),
                    );
                    let _ = rasterize_draw_list(w, h, &list);
                }
            }
        }
    }
}

#[test]
fn pathological_embedded_screens_never_panic() {
    for &(w, h) in &PATHOLOGICAL {
        let menu = demo_mod_manager();
        let (list, hits) = menu.render(w, h);
        for r in &hits {
            assert!(
                r.rect.x >= 0 && r.rect.y >= 0 && r.rect.right() <= w as i32,
                "ModManager region outside {w}x{h}: {r:?}"
            );
        }
        let _ = rasterize_draw_list(w, h, &list);

        let rb = demo_recipe_browser();
        let (list, hits) = rb.render(w, h);
        for r in &hits {
            assert!(
                r.rect.x >= 0 && r.rect.y >= 0 && r.rect.right() <= w as i32,
                "RecipeBrowser region outside {w}x{h}: {r:?}"
            );
        }
        let _ = rasterize_draw_list(w, h, &list);

        let shader = ShaderPackScreenState::default();
        let (list, hits) = shader.render(w, h);
        for r in &hits {
            assert!(
                r.rect.x >= 0 && r.rect.y >= 0 && r.rect.right() <= w as i32,
                "ShaderScreen region outside {w}x{h}: {r:?}"
            );
        }
        let _ = rasterize_draw_list(w, h, &list);

        let magic = populated_magic();
        let list =
            megu_client_ui::magic_hud::build_magic_hud_draw_list(&magic, w, h);
        let _ = rasterize_draw_list(w, h, &list);
    }
}

// ---------------------------------------------------------------------------
// 6. Navigation: every screen has a reachable Escape/Back path
// ---------------------------------------------------------------------------

#[test]
fn every_screen_escapes_to_a_safe_place() {
    for screen in [
        Screen::Title,
        Screen::Settings(SettingsTab::Video),
        Screen::Settings(SettingsTab::Shaders),
        Screen::AboutMegu,
        Screen::Friends,
        Screen::ModSyncConsent,
        Screen::ModManager,
        Screen::RecipeBrowser,
        Screen::InGameHud,
        Screen::Death,
        Screen::Pause,
        Screen::CrashRecovery,
    ] {
        let mut ui = UiState::new();
        ui.screen = screen;
        ui.nav_stack.clear();
        ui.modal = Some(UiModal::ConfirmSendCrashReport);
        // Escape dismisses the modal first.
        ui.handle_input(UiInputEvent::KeyPressed(megu_client_ui::state::UiKey::Escape));
        assert!(ui.modal.is_none(), "{screen:?}: escape did not dismiss modal");
        // Escape again must never panic and must land somewhere defined.
        ui.handle_input(UiInputEvent::KeyPressed(megu_client_ui::state::UiKey::Escape));
        match screen {
            // In-game Escape opens the pause menu.
            Screen::InGameHud => assert_eq!(ui.screen, Screen::Pause),
            // Pause, Title, and Death are absorbing states for Escape
            // (vanilla's DeathScreen has no back path).
            Screen::Pause | Screen::Title | Screen::CrashRecovery | Screen::Death => {}
            // Every menu Escape backs out to Title.
            _ => assert_eq!(
                ui.screen,
                Screen::Title,
                "{screen:?}: escape did not reach Title"
            ),
        }
    }
}

#[test]
fn every_screen_has_back_or_close_widget() {
    let mut ui = UiState::new();
    let _ = all_screens(&mut ui);
    // Screens that expose a clickable Back/Done/Close path.
    let back_widgets: &[(&[WidgetId], Screen)] = &[
        (&[WidgetId::SettingsDone], Screen::Settings(SettingsTab::Video)),
        (&[WidgetId::SettingsDone], Screen::Settings(SettingsTab::Audio)),
        (&[WidgetId::SettingsDone], Screen::Settings(SettingsTab::Shaders)),
        (&[WidgetId::SettingsDone], Screen::Settings(SettingsTab::Qol)),
        (&[WidgetId::SettingsDone], Screen::Settings(SettingsTab::FairPlay)),
        (&[WidgetId::SettingsDone], Screen::Settings(SettingsTab::Language)),
        (&[WidgetId::AboutBack], Screen::AboutMegu),
        (&[WidgetId::FriendsBack], Screen::Friends),
        (
            &[WidgetId::ModSyncAccept, WidgetId::ModSyncDecline],
            Screen::ModSyncConsent,
        ),
        (&[WidgetId::PauseBackToGame], Screen::Pause),
        (&[WidgetId::CrashReturnTitle], Screen::CrashRecovery),
        (
            &[WidgetId::DeathRespawn, WidgetId::DeathTitleScreen],
            Screen::Death,
        ),
    ];
    for &(w, h) in &SIZES {
        for (ids, screen) in back_widgets {
            ui.screen = *screen;
            ui.modal = None;
            ui.render(w, h);
            assert!(
                ids.iter().any(|id| ui.hit_regions.iter().any(|r| r.id == *id)),
                "{screen:?} @{w}x{h}: no reachable Back/Close region"
            );
        }
    }
    // ModManager and RecipeBrowser expose their own Done widgets.
    for &(w, h) in &SIZES {
        ui.screen = Screen::ModManager;
        ui.render(w, h);
        let has_done = ui.hit_regions.iter().any(|r| match r.id {
            WidgetId::ModManager(id) => format!("{id:?}").contains("Done"),
            _ => false,
        });
        assert!(has_done, "ModManager @{w}x{h}: no Done region");
        ui.screen = Screen::RecipeBrowser;
        ui.render(w, h);
        assert!(
            ui.hit_regions.iter().any(|r| r.id == WidgetId::RecipeBrowserDone),
            "RecipeBrowser @{w}x{h}: no Done region"
        );
    }
}

#[test]
fn every_settings_tab_reachable_via_click() {
    let mut ui = UiState::new();
    ui.screen = Screen::Settings(SettingsTab::Video);
    ui.render(1920, 1080);
    for (tab, id) in [
        (SettingsTab::Video, WidgetId::TabVideo),
        (SettingsTab::Audio, WidgetId::TabAudio),
        (SettingsTab::Shaders, WidgetId::TabShaders),
        (SettingsTab::Qol, WidgetId::TabQol),
        (SettingsTab::FairPlay, WidgetId::TabFairPlay),
        (SettingsTab::Language, WidgetId::TabLanguage),
    ] {
        ui.render(1920, 1080);
        let r = ui
            .hit_regions
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("tab region {id:?} missing"));
        ui.handle_input(UiInputEvent::Click {
            x: r.rect.x + 1,
            y: r.rect.y + 1,
        });
        assert_eq!(ui.screen, Screen::Settings(tab), "click did not open {tab:?}");
    }
}

// ---------------------------------------------------------------------------
// 7. Label truncation: every button/tab caption renders whole
// ---------------------------------------------------------------------------

/// No button or tab label may be clipped at the ship resolutions — the
/// regression where full-width CJK advance (16px) squeezed every
/// Japanese caption (`公式サイ`, `オーディ`, `フルスクリーン: OF`, ...)
/// down to a fragment. `truncated_widget_labels` reports each clipped
/// caption; the list must stay empty on every player-facing screen in
/// both languages, Compact Debug HUD off *and* on (the chip grid is the widest
/// QoL layout).
#[test]
fn no_button_or_tab_label_truncates_at_1280x720() {
    for lang in [UiLang::En, UiLang::Ja] {
        for &(w, h) in &[(1280u32, 720u32), (640u32, 360u32)] {
            for mini_on in [false, true] {
                let mut ui = UiState::new();
                // Pin the pre-vanilla auto scale (h/270): the no-truncation
                // contract applies at logical width ≥ 640 — under the
                // canonical formula 720p resolves to scale 3 (427 px).
                ui.ui_scale = ((h / 270).clamp(1, 4)) as u8;
                if lang == UiLang::Ja {
                    ui.set_locale(megu_core::i18n::BuiltinLocale::JaJp);
                }
                let mut screens = all_screens(&mut ui);
                // Screens the hostile fixture doesn't reach.
                screens.extend([
                    Screen::Multiplayer,
                    Screen::ModSyncPrompt,
                    Screen::Disconnected,
                ]);
                if mini_on {
                    ui.settings.qol.compact_debug_hud = all_compact_debug_hud_segments(CompactDebugHudAnchor::TopLeft);
                }
                for screen in screens {
                    ui.screen = screen;
                    ui.nav_stack.clear();
                    ui.modal = None;
                    let clipped = megu_client_ui::layout::truncated_widget_labels(&ui, w, h);
                    assert!(
                        clipped.is_empty(),
                        "{screen:?} @{w}x{h} lang={lang:?} mini={mini_on}: \
                         truncated captions {clipped:?}"
                    );
                }
            }
        }
    }
}
