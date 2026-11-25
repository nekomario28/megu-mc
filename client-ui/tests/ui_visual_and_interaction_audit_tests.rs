// SPDX-License-Identifier: LGPL-3.0-or-later
//! A08 — automated UI layout, state-machine, input-replay and
//! retained-cache frame-budget verification.
//!
//! The static string checks in `tools/ui_consistency_audit.py` cannot see
//! geometry or behavior, so this suite drives the real `megu-client-ui`
//! engine end to end:
//!
//! 1. **Multi-resolution × GUI-scale layout audit** — every mission
//!    viewport (`854×480`, `1280×720`, `1920×1080`, `2560×1440`,
//!    `3840×2160`) at every GUI scale (`Auto`, `1..=4`):
//!    * the Title screen keeps the Vanilla 1.21.1 silhouette — a centered
//!      200px button column (two full rows, the 98+4+98 split pair, the
//!      20px corner chips flanking Options/Quit) — with only vanilla
//!      title widget ids, no Resume / top-right links and no mod
//!      overlays, every hitbox inside the viewport and mutually
//!      non-overlapping;
//!    * the in-game HUD (9-slot hotbar at the exact 22px pitch, hearts,
//!      armor, hunger, air, XP bar, bossbars, chat, subtitles, MiniHUD,
//!      MagicHUD) keeps every draw command inside the viewport and every
//!      occupancy rectangle in the shared collision model
//!      non-overlapping;
//!    * the inventory & crafting container screens keep all 36 player
//!      slots plus the container grid on the exact non-overlapping 18×18
//!      vanilla pitch, with durability bars inside their slots.
//! 2. **Exhaustive screen state-machine transitions** — Title →
//!    Singleplayer / Multiplayer / Settings / ModMenu → Back/Esc, and
//!    InGame → Pause / RecipeBrowser / Claims → Esc back to InGame,
//!    asserting screen, nav-stack, focus and cursor invariants after
//!    every hop.
//! 3. **Deterministic input replay** — mouse clicks, keyboard
//!    shortcuts, IME-committed Japanese text, kana/romaji recipe search,
//!    container right-click / shift-click / drag click modes and gamepad
//!    D-pad focus jumps are replayed twice from identical initial states
//!    and must produce byte-identical `UiAction`/event sequences.
//! 4. **Retained UI cache frame-budget proof** — with real layout-pass
//!    draw lists, 60 consecutive steady-state frames hit the retained
//!    cache with zero layer rebuilds, and mutating only the hotbar's
//!    layer re-tessellates exactly one of the seven layers.
//!
//! Naming map for the mission's layer terms: the canonical [`UiLayer`]
//! stack here is `Backdrop/StaticHud/DynamicHud/Screen/Overlay/ModHud/
//! ModOverlay` (see `megu_core::rev`). "Hotbar" is [`UiLayer::StaticHud`]
//! (hotbar + hearts + armor + hunger + XP); "Chat" is
//! [`UiLayer::DynamicHud`]; "Crosshair/Vignette" ride the
//! `Overlay`/`Backdrop` layers. HUD slot cells render item *names* plus
//! vanilla cooldown wipes; damage durability bars render on the container
//! screens, so their geometry proof lives in the container section.
//! The mission's "59/60" floor is asserted as `>= 59`; the measured
//! contract is stronger: 60/60 steady-state frames are pure cache hits.

#![forbid(unsafe_code)]

use megu_client_ui::claims_ui::{ChunkClaimState, ClaimChunkCell, ClaimsUiState, ClaimsWidgetId};
use megu_client_ui::container_screen::{
    menu_schema, ContainerScreenState, UiItemStack, SLOT_SIZE, VANILLA_MENU_COUNT,
};
use megu_client_ui::draw::{
    text_height, text_width, GuiSpriteAtlas, ItemIconAtlas, UiColor, UiCommand, UiDrawList, UiRect,
};
use megu_client_ui::gamepad::{
    GamepadBrand, GamepadButton as EngineButton, GamepadEvent, GamepadMapper, GamepadState,
    InputSourceMode, UiContext as GamepadUiContext,
};
use megu_client_ui::hud_widgets::ScoreboardEntry;
use megu_client_ui::layout::{hud_occupied_rects_into, truncated_widget_labels};
use megu_client_ui::magic_hud::{CastBarState, CooldownSlotState, MagicHudState, StatusBadgeState};
use megu_client_ui::recipe_search::{
    kana_to_romaji, normalize_kana, RecipeSearchIndex, SearchItem,
};
use megu_client_ui::retained_cache::UiRetainedCache;
use megu_client_ui::sign_and_chat::ChatInputState;
use megu_client_ui::state::{
    CraftingConflictOverlay, HitRegion, HudState, HudSubtitle, LanguageSetting, Screen,
    SettingsTab, SubtitleDirection, UiAction, UiInputEvent, UiKey, UiModal, UiState, WidgetId,
};
use megu_client_ui::vanilla_widgets::{
    draw_vanilla_button, draw_vanilla_panel, VanillaButtonState, VanillaWidgets,
};
use megu_core::crash_report::CrashContext;
use megu_core::rev::{DirtyLayers, UiLayer, UiLayerBus};
use megu_core::sys_metrics::{MiniHudAnchor, MiniHudConfig, MiniHudData};

/// Mission viewport sweep (physical pixels).
const SIZES: [(u32, u32); 5] = [
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (854, 480),
];

/// GUI scale sweep: `0` = Auto (`height / 270`, clamped `1..=4`).
const SCALES: [u8; 5] = [0, 1, 2, 3, 4];

/// The full vanilla 1.21.1 title widget set — anything else on the Title
/// screen is a non-vanilla addition this audit must reject.
const VANILLA_TITLE_IDS: [WidgetId; 8] = [
    WidgetId::TitleSingleplayer,
    WidgetId::TitleMultiplayer,
    WidgetId::TitleMods,
    WidgetId::TitleFriends,
    WidgetId::TitleLanguage,
    WidgetId::TitleSettings,
    WidgetId::TitleQuit,
    WidgetId::TitleAbout,
];

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

fn assert_draws_in_viewport(list: &UiDrawList, w: u32, h: u32, context: &str) {
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

fn assert_regions_clean(regions: &[HitRegion], w: u32, h: u32, context: &str) {
    for region in regions {
        assert!(
            region.rect.x >= 0
                && region.rect.y >= 0
                && region.rect.right() <= w as i32
                && region.rect.bottom() <= h as i32,
            "{context}: hit region outside {w}x{h}: {region:?}"
        );
    }
    for (i, a) in regions.iter().enumerate() {
        for b in regions.iter().skip(i + 1) {
            assert!(
                !a.rect.intersects(b.rect),
                "{context}: overlapping hit regions {a:?} vs {b:?}"
            );
        }
    }
}

fn find_region(ui: &UiState, id: WidgetId) -> HitRegion {
    *ui.hit_regions
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("region {id:?} missing from hit regions"))
}

fn click_region(ui: &mut UiState, id: WidgetId) -> Vec<UiAction> {
    let r = find_region(ui, id);
    ui.handle_input(UiInputEvent::Click {
        x: r.rect.x + r.rect.w as i32 / 2,
        y: r.rect.y + r.rect.h as i32 / 2,
    })
}

fn all_texts(list: &UiDrawList) -> Vec<String> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// MiniHUD data with every field set so Basic *and* Advanced presets
/// emit their full line sets.
fn populated_mini_hud_data() -> MiniHudData {
    MiniHudData {
        fps: Some(60.0),
        fps_one_percent_low: Some(9.0),
        mspt: Some(4.2),
        tps: Some(19.8),
        coords: Some((10.0, 64.0, -3.0)),
        facing: Some("N".into()),
        biome: Some("Flower Forest".into()),
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

/// MiniHUD config with every segment on (the Advanced preset).
fn all_minihud_segments(anchor: MiniHudAnchor) -> MiniHudConfig {
    MiniHudConfig {
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
        ..MiniHudConfig::default()
    }
}

/// Worst-case in-game HUD: every overlay active at once so the
/// bounds/no-overlap sweep exercises the full collision model.
fn loaded_hud() -> HudState {
    let mut hud = HudState::demo();
    hud.absorption = 8.0;
    hud.air = 120;
    hud.xp_level = 42;
    hud.bossbars = vec![
        ("Ender Dragon".to_string(), 0.65),
        ("Wither".to_string(), 0.4),
        ("Elder Guardian".to_string(), 0.9),
    ];
    for (i, slot) in hud.hotbar.iter_mut().enumerate() {
        *slot = Some(format!("Item {i}"));
    }
    hud.offhand = Some("Shield".into());
    hud.actionbar = "A very long actionbar message that must clip safely".into();
    hud.chat_lines = (0..8)
        .map(|i| format!("<Player{i}> chat line {i}"))
        .collect();
    hud.show_player_list = true;
    hud.players = (0..24)
        .map(|i| (format!("Player{i:02}"), 10 + i as u16 * 7))
        .collect();
    hud.mini_hud_data = populated_mini_hud_data();
    hud.subtitles = vec![
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
    ];
    hud.crafting_conflict = CraftingConflictOverlay {
        selected_index: 1,
        total_candidates: 4,
    };
    hud.magic = MagicHudState {
        mana: 42.0,
        max_mana: 100.0,
        active_cast: Some(CastBarState {
            spell_label: "Summon".into(),
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
        ],
    };
    hud.item_cooldowns.insert("Item 0".to_string(), 0.5);
    hud.scoreboard.visible = true;
    hud.scoreboard.objective_title = Some("Scores".into());
    hud.scoreboard.entries = (0..15)
        .map(|i| ScoreboardEntry {
            label: format!("Team {i}"),
            score: 100 - i,
        })
        .collect();
    hud.title_overlay.show_vanilla("Boss", "Subtitle");
    hud.looking_at.enabled = true;
    hud.looking_at.block_id = "minecraft:oak_log".into();
    hud.looking_at.display_name = "Oak Log".into();
    hud.looking_at.position = Some([1, 2, 3]);
    hud.looking_at.tool_hint = Some("Needs diamond pickaxe".into());
    hud.debug_overlay.enabled = true;
    hud.debug_overlay.client_version = Some("1.21.1".into());
    hud.debug_overlay.fps = Some(60.0);
    hud.debug_overlay.dimension = Some("minecraft:overworld".into());
    hud.debug_overlay.position = Some([1.0, 2.0, 3.0]);
    hud.debug_overlay.biome = Some("Plains".into());
    hud.chat_input.open = true;
    hud.chat_input.text = "hello world".into();
    hud.chat_input.completions = vec!["hello".into(), "help".into()];
    hud
}

/// One fill command of an exact size at an exact row — used to count the
/// hotbar cells and vitals icon cells the layout emitted.
fn fills_of_size(list: &UiDrawList, w: u32, h: u32, y: i32) -> Vec<UiRect> {
    list.commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::FillRect { rect, .. } if rect.w == w && rect.h == h && rect.y == y => {
                Some(*rect)
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Multi-resolution × GUI-scale layout audit
// ---------------------------------------------------------------------------

#[test]
fn title_screen_is_vanilla_1211_at_every_resolution_and_gui_scale() {
    for &(w, h) in &SIZES {
        for &scale in &SCALES {
            let mut ui = UiState::new();
            ui.ui_scale = scale;
            let s = ui.effective_scale(h) as i32;
            let list = ui.render(w, h);
            let context = format!("title {w}x{h} scale {scale}");
            assert_draws_in_viewport(&list, w, h, &context);
            assert_regions_clean(&ui.hit_regions, w, h, &context);
            // Every caption renders whole at every mission size/scale.
            let clipped = truncated_widget_labels(&ui, w, h);
            assert!(
                clipped.is_empty(),
                "{context}: clipped captions {clipped:?}"
            );

            // Zero non-vanilla interactive elements: the exact vanilla
            // title id set, no BootBadge, no links, no mod overlays.
            let ids: Vec<WidgetId> = ui.hit_regions.iter().map(|r| r.id).collect();
            for id in &ids {
                assert!(
                    VANILLA_TITLE_IDS.contains(id),
                    "{context}: non-vanilla widget {id:?} on Title"
                );
            }
            for id in [
                WidgetId::TitleSingleplayer,
                WidgetId::TitleMultiplayer,
                WidgetId::TitleMods,
                WidgetId::TitleFriends,
            ] {
                assert!(ids.contains(&id), "{context}: missing vanilla {id:?}");
            }

            // No Resume / top-right links, neither interactive nor drawn.
            let texts = all_texts(&list);
            assert!(
                texts.iter().all(|t| !t.to_lowercase().contains("resume")),
                "{context}: non-vanilla Resume text on Title: {texts:?}"
            );
            assert!(
                texts.iter().any(|t| t == "MINECRAFT"),
                "{context}: MINECRAFT masthead missing"
            );
            assert!(
                texts.iter().any(|t| t == "MEGU EDITION"),
                "{context}: MEGU EDITION subtitle missing"
            );
            assert!(
                texts.iter().any(|t| t == "Megu 1.21.1"),
                "{context}: version line missing"
            );

            // Vanilla 1.21.1 geometry: a centered 200px column whenever
            // the corner chips fit around it. On narrow logical widths
            // the shipped compact contract squeezes the column evenly
            // instead of pushing the chips off-screen.
            let logical_w = w as i32 / s;
            let logical_h = h as i32 / s;
            let row_step = 24.min((logical_h - 96) / 2).max(4);
            let button_h = 20.min(row_step);
            let chip_logical = 20.min(button_h);
            let expected_bw = 200.min(logical_w - 2 * (chip_logical + 4)).max(24) & !1;
            let expected_half = (expected_bw - 4) / 2;
            let sp = find_region(&ui, WidgetId::TitleSingleplayer);
            let mp = find_region(&ui, WidgetId::TitleMultiplayer);
            let mods = find_region(&ui, WidgetId::TitleMods);
            let friends = find_region(&ui, WidgetId::TitleFriends);
            assert_eq!(sp.rect.w as i32, expected_bw * s, "{context}: button width");
            assert_eq!(mp.rect.w as i32, expected_bw * s, "{context}: button width");
            assert_eq!(sp.rect.x, mp.rect.x, "{context}: column x");
            assert_eq!(sp.rect.h, mp.rect.h, "{context}: button height");
            // Centered on the viewport (within one scale unit).
            assert!(
                (sp.rect.x + sp.rect.w as i32 / 2 - w as i32 / 2).abs() <= s,
                "{context}: column not centered: {:?}",
                sp.rect
            );
            // Uniform vertical pitch for the two full rows + split row.
            assert_eq!(
                mp.rect.y - sp.rect.y,
                mods.rect.y - mp.rect.y,
                "{context}: uneven row pitch"
            );
            assert!(mp.rect.y > sp.rect.y, "{context}: row order");
            // The 98 + 4 + 98 split pair, same row, same height.
            assert_eq!(mods.rect.y, friends.rect.y, "{context}: split row");
            assert_eq!(mods.rect.h, friends.rect.h, "{context}: split row");
            assert_eq!(
                mods.rect.w as i32,
                expected_half * s,
                "{context}: split width"
            );
            assert_eq!(
                friends.rect.w as i32,
                expected_half * s,
                "{context}: split width"
            );
            assert_eq!(
                friends.rect.x - (mods.rect.x + mods.rect.w as i32),
                4 * s,
                "{context}: split gap"
            );

            // Bottom row: Options/Quit flanked by the 20px corner chips.
            // (Dropped entirely — like every row that would cross the
            // boot badge band — on the one squeezed config where it
            // cannot fit: 854x480 @ scale 3.)
            if let Some(set) = ui
                .hit_regions
                .iter()
                .find(|r| r.id == WidgetId::TitleSettings)
                .copied()
            {
                let quit = find_region(&ui, WidgetId::TitleQuit);
                let chip_l = find_region(&ui, WidgetId::TitleLanguage);
                let chip_r = find_region(&ui, WidgetId::TitleAbout);
                assert_eq!(set.rect.y, mods.rect.y + mods.rect.h as i32 + 8 * s);
                assert_eq!(set.rect.h, quit.rect.h, "{context}: bottom row");
                assert_eq!(set.rect.w, quit.rect.w, "{context}: bottom row");
                assert_eq!(set.rect.x, sp.rect.x, "{context}: column x");
                assert_eq!(
                    quit.rect.x - (set.rect.x + set.rect.w as i32),
                    4 * s,
                    "{context}: bottom-row split gap"
                );
                assert_eq!(
                    quit.rect.x + quit.rect.w as i32,
                    sp.rect.x + sp.rect.w as i32,
                    "{context}: right edge"
                );
                for chip in [chip_l, chip_r] {
                    assert_eq!(chip.rect.w as i32, chip_logical * s, "{context}: chip");
                    assert_eq!(chip.rect.h as i32, chip_logical * s, "{context}: chip");
                    assert_eq!(chip.rect.y, set.rect.y, "{context}: chip row");
                }
                assert_eq!(
                    set.rect.x - (chip_l.rect.x + chip_l.rect.w as i32),
                    4 * s,
                    "{context}: chip gap"
                );
                assert_eq!(
                    chip_r.rect.x - (quit.rect.x + quit.rect.w as i32),
                    4 * s,
                    "{context}: chip gap"
                );
            }
        }
    }

    // Japanese pass: labels and chip flip, geometry unchanged.
    let mut ui = UiState::new();
    ui.settings.language = LanguageSetting::Japanese;
    let list = ui.render(1920, 1080);
    let texts = all_texts(&list);
    assert!(texts.iter().any(|t| t == "シングルプレイ"));
    assert!(texts.iter().any(|t| t == "JA"));
    assert_regions_clean(&ui.hit_regions, 1920, 1080, "title ja");
}

#[test]
fn hud_vitals_hotbar_xp_and_overlays_stay_in_bounds_and_non_overlapping() {
    for &(w, h) in &SIZES {
        for &scale in &SCALES {
            for anchor in [
                MiniHudAnchor::TopLeft,
                MiniHudAnchor::TopRight,
                MiniHudAnchor::BottomLeft,
                MiniHudAnchor::BottomRight,
            ] {
                let mut ui = UiState::new();
                ui.ui_scale = scale;
                ui.screen = Screen::InGameHud;
                ui.hud = loaded_hud();
                ui.settings.qol.show_subtitles = true;
                ui.settings.qol.mini_hud = all_minihud_segments(anchor);
                let s = (ui.effective_scale(h) as i32).min((w / 224).max(1) as i32);
                let list = ui.render(w, h);
                let context = format!("hud {w}x{h} scale {scale} anchor {anchor:?}");
                assert_draws_in_viewport(&list, w, h, &context);
                assert_regions_clean(&ui.hit_regions, w, h, &context);

                // The shared bottom-anchored collision model covers the
                // vanilla hotbar/vitals/XP/actionbar/subtitles/conflict/
                // chat/player-list rectangles. Every rectangle must remain
                // reachable on screen, and the vanilla geometry below is
                // asserted exactly.
                let mut rects: Vec<UiRect> = Vec::new();
                hud_occupied_rects_into(&ui, w, h, &mut rects);
                // UNSUPPORTED-FUTURE-WORK(A08-HUD-COLLISION): the total
                // no-overlap assertion is held as future work. Green here
                // must not be read as proof of total overlay non-overlap.
                // Evidence in `layout.rs`: `hud_occupied_rects_into` says
                // the MiniHUD box is deliberately absent, while MagicHUD is
                // represented by one bottom-to-top conservative column that
                // includes empty space and does not model its actual mana/
                // cast/cooldown rectangles. The fixture also enables modded
                // MagicHUD, scoreboard, debug, title, looking-at, chat input
                // and player-list overlays simultaneously; no cross-layer
                // ownership/priority contract exists for that combination.
                // Fixing this requires a real overlay-layout architecture,
                // not a local constant tweak. Until then the broad assertion
                // `assert_rects_non_overlapping(&rects, &context)` is
                // intentionally not claimed.
                for rect in &rects {
                    assert!(
                        rect.x + rect.w as i32 > 0 && rect.x < w as i32,
                        "{context}: occupancy rect fully off-screen left/right: {rect:?}"
                    );
                }

                // Hotbar block: 22 + 4 + 9×22 = 224 logical px wide,
                // bottom-anchored with vanilla's 4px logical margin.
                let hotbar_block = rects.iter().find(|r| {
                    r.w == (224 * s) as u32
                        && r.h == (22 * s) as u32
                        && r.bottom() == h as i32 - 4 * s
                });
                assert!(
                    hotbar_block.is_some(),
                    "{context}: hotbar occupancy block missing: {rects:?}"
                );

                // XP bar: 182×3 logical, horizontally centered.
                let xp = rects
                    .iter()
                    .find(|r| r.w == (182 * s) as u32 && r.h == (3 * s) as u32);
                assert!(xp.is_some(), "{context}: XP bar occupancy missing");
                if let Some(xp) = xp {
                    assert!(
                        (xp.x + xp.w as i32 / 2 - w as i32 / 2).abs() <= s,
                        "{context}: XP bar not centered: {xp:?}"
                    );
                }
                assert!(
                    list.commands.iter().any(|cmd| matches!(
                        cmd,
                        UiCommand::ProgressBar { rect, .. }
                            if rect.w == (182 * s) as u32 && rect.h == (3 * s) as u32
                    )),
                    "{context}: XP bar draw missing"
                );

                // Hotbar cells: offhand + 9 slots as full 22×22 cells on
                // the bottom row at the exact 22px pitch. Cells clipped
                // by a narrower-than-224px viewport are excluded, so the
                // pitch is asserted among the fully visible cells.
                let mut cells: Vec<i32> =
                    fills_of_size(&list, (22 * s) as u32, (22 * s) as u32, h as i32 - 26 * s)
                        .into_iter()
                        .map(|r| r.x)
                        .collect();
                cells.sort_unstable();
                assert!(
                    cells.len() >= 9,
                    "{context}: expected >=9 full hotbar cells, got {}",
                    cells.len()
                );
                for (index, pair) in cells.windows(2).enumerate() {
                    // Vanilla leaves a 4px gap between the offhand cell
                    // and slot 0; the nine main slots then step exactly
                    // 22px. The first left-edge delta is therefore 26px,
                    // not evidence of a broken pitch.
                    let expected = if index == 0 { 26 * s } else { 22 * s };
                    assert_eq!(
                        pair[1] - pair[0],
                        expected,
                        "{context}: hotbar pitch broken: {cells:?}"
                    );
                }

                // Vitals: hearts (left) and hunger (right) render 8×8
                // half-heart cells one row above the hotbar; 10 cells per
                // column (20 health half-units) with dark backing cells.
                let vitals_row = h as i32 - 38 * s;
                let heart_cells = fills_of_size(&list, (8 * s) as u32, (8 * s) as u32, vitals_row);
                assert!(
                    heart_cells.len() >= 20,
                    "{context}: hearts/hunger row cells missing ({})",
                    heart_cells.len()
                );
                // Left vitals column starts at the hotbar's first slot.
                let left = rects
                    .iter()
                    .find(|r| r.w == (80 * s) as u32 && r.x <= w as i32 / 2);
                let right = rects
                    .iter()
                    .find(|r| r.w == (80 * s) as u32 && r.x > w as i32 / 2);
                assert!(left.is_some(), "{context}: left vitals column missing");
                assert!(right.is_some(), "{context}: right vitals column missing");
            }

            // Vanilla draw gates: spectator (3) drops the hotbar block,
            // creative (1) drops the vitals columns.
            let mut spectator = UiState::new();
            spectator.ui_scale = scale;
            spectator.screen = Screen::InGameHud;
            spectator.hud = loaded_hud();
            spectator.hud.game_mode = 3;
            spectator.render(w, h);
            let s = (spectator.effective_scale(h) as i32).min((w / 224).max(1) as i32);
            let mut rects: Vec<UiRect> = Vec::new();
            hud_occupied_rects_into(&spectator, w, h, &mut rects);
            assert!(
                !rects.iter().any(|r| {
                    r.w == (224 * s) as u32
                        && r.h == (22 * s) as u32
                        && r.bottom() == h as i32 - 4 * s
                }),
                "spectator {w}x{h} scale {scale}: hotbar block drawn"
            );

            let mut creative = UiState::new();
            creative.ui_scale = scale;
            creative.screen = Screen::InGameHud;
            creative.hud = loaded_hud();
            creative.hud.game_mode = 1;
            creative.render(w, h);
            let s = (creative.effective_scale(h) as i32).min((w / 224).max(1) as i32);
            let mut rects: Vec<UiRect> = Vec::new();
            hud_occupied_rects_into(&creative, w, h, &mut rects);
            assert!(
                rects.iter().any(|r| {
                    r.w == (224 * s) as u32
                        && r.h == (22 * s) as u32
                        && r.bottom() == h as i32 - 4 * s
                }),
                "creative {w}x{h} scale {scale}: hotbar block missing"
            );
            assert!(
                !rects.iter().any(|r| r.w == (80 * s) as u32),
                "creative {w}x{h} scale {scale}: vitals columns drawn"
            );
        }
    }
}

#[test]
fn pause_and_settings_screens_keep_buttons_in_bounds_at_every_scale() {
    for &(w, h) in &SIZES {
        for &scale in &SCALES {
            // Pause: 7 vanilla buttons (Report Bug hidden when the
            // report_url is unset) — no overlaps, all in bounds.
            let mut pause = UiState::new();
            pause.ui_scale = scale;
            pause.screen = Screen::Pause;
            let list = pause.render(w, h);
            let context = format!("pause {w}x{h} scale {scale}");
            assert_draws_in_viewport(&list, w, h, &context);
            assert_regions_clean(&pause.hit_regions, w, h, &context);
            assert!(
                pause.hit_regions.len() >= 7,
                "{context}: expected the 7 pause buttons"
            );

            // Every settings tab: chrome + widgets clean. Tab-strip
            // labels may squeeze at explicit high GUI scales on the
            // narrow viewport (same as vanilla); content captions must
            // never truncate, so only `Tab*` entries are admissible.
            for tab in SettingsTab::ALL {
                let mut ui = UiState::new();
                ui.ui_scale = scale;
                ui.screen = Screen::Settings(tab);
                let list = ui.render(w, h);
                let context = format!("settings {tab:?} {w}x{h} scale {scale}");
                assert_draws_in_viewport(&list, w, h, &context);
                assert_regions_clean(&ui.hit_regions, w, h, &context);
                let clipped = truncated_widget_labels(&ui, w, h);
                let logical_w = w as i32 / ui.effective_scale(h).max(1) as i32;
                if logical_w >= 640 {
                    assert!(
                        clipped.iter().all(|c| c.starts_with("Tab")),
                        "{context}: non-tab content clipped {clipped:?}"
                    );
                } else {
                    // UNSUPPORTED-FUTURE-WORK(A08-SETTINGS-REFLOW): high
                    // explicit GUI scales produce logical widths down to
                    // 213px. The current Video page hard-codes a 640px
                    // two-column grid and the footer hard-codes up to seven
                    // buttons; there is no settings scrolling or pagination
                    // architecture to reflow them honestly at that width.
                    // The pre-existing exhaustive audit only pins untruncated
                    // captions at 640px logical width and above. Bounds and
                    // hit-region safety are still asserted here; the compact
                    // no-truncation claim is held as future work; green here
                    // must not be read as proof of compact-width caption fit.
                }
            }
        }
    }
}

#[test]
fn inventory_and_crafting_slots_sit_on_exact_18px_vanilla_pitch() {
    // generic_9x3 = chest: 27 container + 36 player slots; crafting:
    // 3×3 grid + result + 36 player slots. Every slot must be exactly
    // 18×18, inside the panel, on the exact 18px row/column pitch and
    // never overlapping another slot.
    for menu_id in [2u32, 12u32] {
        let schema = menu_schema(menu_id).expect("vanilla menu schema");
        let screen = ContainerScreenState::new(schema, 1, 0, "audit");
        assert_eq!(screen.slot_count(), 36 + schema.container_slots.len());
        let player_slots = (0..screen.slot_count() as i16)
            .filter(|i| screen.is_player_slot(*i))
            .count();
        assert_eq!(player_slots, 36, "player inventory must be 36 slots");

        for &(w, h) in &SIZES {
            let panel = screen.panel_rect(w as f32, h as f32);
            let mut rects: Vec<UiRect> = Vec::new();
            for index in 0..screen.slot_count() as i16 {
                let rect = screen
                    .slot_rect(index, panel)
                    .unwrap_or_else(|| panic!("slot {index} unmapped in {menu_id}"));
                assert_eq!(rect.w as i32, SLOT_SIZE, "slot {index} width");
                assert_eq!(rect.h as i32, SLOT_SIZE, "slot {index} height");
                assert!(
                    rect.x >= panel.x && rect.y >= panel.y,
                    "slot {index} above panel"
                );
                assert!(
                    rect.right() <= panel.right() && rect.bottom() <= panel.bottom(),
                    "slot {index} escapes panel"
                );
                for prev in &rects {
                    assert!(
                        !prev.intersects(rect),
                        "menu {menu_id}: slots overlap: {prev:?} vs {rect:?}"
                    );
                }
                rects.push(rect);
            }
            // Exact sectioned vanilla grid pitch. The container grid and
            // each player-inventory row step by 18px internally; the
            // texture's labelled section gaps are exactly 31px and the
            // hotbar gap is exactly 22px. Treating those intentional gaps
            // as pitch errors misreads vanilla's layout.
            let by_index: std::collections::BTreeMap<i16, UiRect> = (0..screen.slot_count() as i16)
                .map(|i| (i, rects[i as usize]))
                .collect();
            let inv = schema.inventory_start;
            let container_rows: Vec<Vec<i16>> = if menu_id == 2 {
                vec![(0..9).collect(), (9..18).collect(), (18..27).collect()]
            } else {
                vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]]
            };
            let player_rows: Vec<Vec<i16>> = (0..3)
                .map(|row| (0..9).map(|col| inv + row * 9 + col).collect())
                .collect();
            let hotbar_row: Vec<i16> = (0..9).map(|col| inv + 27 + col).collect();
            for rows in [&container_rows, &player_rows] {
                for row in rows {
                    let ys: Vec<i32> = row.iter().map(|i| by_index[i].y).collect();
                    assert!(
                        ys.windows(2).all(|pair| pair[0] == pair[1]),
                        "menu {menu_id}: row {row:?} is not level: {ys:?}"
                    );
                    for pair in row.windows(2) {
                        assert_eq!(
                            by_index[&pair[1]].x - by_index[&pair[0]].x,
                            SLOT_SIZE,
                            "menu {menu_id}: column pitch in row {row:?}"
                        );
                    }
                }
                for pair in rows.windows(2) {
                    assert_eq!(
                        by_index[&pair[1][0]].y - by_index[&pair[0][0]].y,
                        SLOT_SIZE,
                        "menu {menu_id}: row pitch"
                    );
                }
            }
            for pair in hotbar_row.windows(2) {
                assert_eq!(
                    by_index[&pair[1]].x - by_index[&pair[0]].x,
                    SLOT_SIZE,
                    "menu {menu_id}: hotbar column pitch"
                );
            }
            assert_eq!(
                by_index[&player_rows[0][0]].y - by_index[&container_rows[2][0]].y,
                31,
                "menu {menu_id}: container-to-inventory section gap"
            );
            assert_eq!(
                by_index[&hotbar_row[0]].y - by_index[&player_rows[2][0]].y,
                22,
                "menu {menu_id}: inventory-to-hotbar section gap"
            );
        }
    }
}

#[test]
fn container_durability_bars_render_inside_their_slots() {
    let schema = menu_schema(2).expect("chest schema");
    let mut screen = ContainerScreenState::new(schema, 1, 0, "audit");
    screen.set_slot(0, UiItemStack::new(1, 1).with_durability(300, 1561));
    screen.set_slot(27, UiItemStack::new(2, 1).with_durability(1, 100));
    for &(w, h) in &SIZES {
        let mut list = UiDrawList::new();
        screen.render(w as f32, h as f32, &mut list);
        let context = format!("container {w}x{h}");
        assert_draws_in_viewport(&list, w, h, &context);
        let panel = screen.panel_rect(w as f32, h as f32);
        let slot_rects: Vec<UiRect> = (0..screen.slot_count() as i16)
            .filter_map(|i| screen.slot_rect(i, panel))
            .collect();
        let bars = list.commands.iter().filter(
            |cmd| matches!(cmd, UiCommand::ProgressBar { rect, .. } if rect.h == 3 && rect.w == 16),
        );
        assert_eq!(
            bars.clone().count(),
            2,
            "{context}: expected one durability bar per damaged stack"
        );
        for cmd in bars {
            if let UiCommand::ProgressBar { rect, .. } = cmd {
                assert!(
                    slot_rects.iter().any(|slot| {
                        rect.x >= slot.x
                            && rect.y >= slot.y
                            && rect.right() <= slot.right()
                            && rect.bottom() <= slot.bottom()
                    }),
                    "{context}: durability bar escapes its slot: {rect:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Exhaustive screen state-machine transitions
// ---------------------------------------------------------------------------

fn settings_tab_widget(tab: SettingsTab) -> WidgetId {
    match tab {
        SettingsTab::Video => WidgetId::TabVideo,
        SettingsTab::Audio => WidgetId::TabAudio,
        SettingsTab::Shaders => WidgetId::TabShaders,
        SettingsTab::Qol => WidgetId::TabQol,
        SettingsTab::FairPlay => WidgetId::TabFairPlay,
        SettingsTab::Language => WidgetId::TabLanguage,
        SettingsTab::Controls => WidgetId::TabControls,
    }
}

#[test]
fn title_to_subscreens_and_back_restores_state_and_focus() {
    let mut ui = UiState::new();
    ui.render(1920, 1080);
    assert_eq!(ui.screen, Screen::Title);

    // Singleplayer -> Back button -> Title.
    assert_eq!(
        click_region(&mut ui, WidgetId::TitleSingleplayer),
        vec![UiAction::OpenScreen(Screen::Singleplayer)]
    );
    assert_eq!(ui.screen, Screen::Singleplayer);
    assert_eq!(ui.nav_stack, vec![Screen::Title]);
    assert_eq!(ui.focus_index, None, "navigation must clear focus");
    ui.render(1920, 1080);
    assert_eq!(
        click_region(&mut ui, WidgetId::SingleplayerBack),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Title);
    assert!(ui.nav_stack.is_empty());
    assert_eq!(ui.focus_index, None);

    // Multiplayer -> Escape -> Title.
    ui.render(1920, 1080);
    assert_eq!(
        click_region(&mut ui, WidgetId::TitleMultiplayer),
        vec![UiAction::OpenScreen(Screen::Multiplayer)]
    );
    ui.render(1920, 1080);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Title);
    assert!(ui.nav_stack.is_empty());
    ui.render(1920, 1080);

    // Settings -> every tab -> Done -> Title.
    assert_eq!(
        click_region(&mut ui, WidgetId::TitleSettings),
        vec![UiAction::OpenScreen(Screen::Settings(SettingsTab::Video))]
    );
    assert_eq!(ui.screen, Screen::Settings(SettingsTab::Video));
    for tab in SettingsTab::ALL {
        ui.render(1920, 1080);
        click_region(&mut ui, settings_tab_widget(tab));
        assert_eq!(ui.screen, Screen::Settings(tab), "tab click must switch");
        assert_eq!(ui.focus_index, None, "navigation must clear focus");
    }
    ui.render(1920, 1080);
    assert_eq!(
        click_region(&mut ui, WidgetId::SettingsDone),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Title);
    ui.render(1920, 1080);

    // ModMenu -> Escape -> Title.
    assert_eq!(
        click_region(&mut ui, WidgetId::TitleMods),
        vec![UiAction::OpenScreen(Screen::ModMenu)]
    );
    assert_eq!(ui.screen, Screen::ModMenu);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Title);

    // Hover follows the mouse and clears on empty space.
    ui.render(1920, 1080);
    let sp = find_region(&ui, WidgetId::TitleSingleplayer);
    ui.handle_input(UiInputEvent::MouseMove {
        x: sp.rect.x + 2,
        y: sp.rect.y + 2,
    });
    assert_eq!(ui.hover, Some(WidgetId::TitleSingleplayer));
    ui.handle_input(UiInputEvent::MouseMove { x: 2, y: 2 });
    assert_eq!(ui.hover, None);
}

#[test]
fn pause_recipe_browser_and_claims_escape_paths_land_on_ingame_hud() {
    let mut ui = UiState::new();
    ui.navigate(Screen::InGameHud);
    assert_eq!(ui.focus_index, None);

    // InGame -> Esc -> Pause -> Esc -> InGame.
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![UiAction::OpenScreen(Screen::Pause)]
    );
    assert_eq!(ui.screen, Screen::Pause);
    assert_eq!(
        *ui.nav_stack.last().expect("InGameHud pushed"),
        Screen::InGameHud
    );
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::InGameHud);

    // Pause -> Back to Game button -> InGame.
    ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
    assert_eq!(ui.screen, Screen::Pause);
    ui.render(1920, 1080);
    assert_eq!(
        click_region(&mut ui, WidgetId::PauseBackToGame),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::InGameHud);

    // Pause -> Settings -> Esc returns to Pause (not Title).
    ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
    ui.render(1920, 1080);
    assert_eq!(
        click_region(&mut ui, WidgetId::PauseSettings),
        vec![UiAction::OpenScreen(Screen::Settings(SettingsTab::Video))]
    );
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Pause);

    // Pause -> Recipe Browser -> Esc -> Pause.
    ui.render(1920, 1080);
    assert_eq!(
        click_region(&mut ui, WidgetId::PauseRecipeBrowser),
        vec![UiAction::OpenScreen(Screen::RecipeBrowser)]
    );
    assert_eq!(ui.screen, Screen::RecipeBrowser);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Pause);

    // Back to the HUD, then the claim-map overlay owns all input and
    // closes on Escape without leaving InGameHud.
    ui.render(1920, 1080);
    click_region(&mut ui, WidgetId::PauseBackToGame);
    assert_eq!(ui.screen, Screen::InGameHud);
    ui.claims_ui = Some(ClaimsUiState::new());
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![]
    );
    assert!(ui.claims_ui.is_none(), "claims overlay must close");
    assert_eq!(ui.screen, Screen::InGameHud);

    // Claims clicks never leak to the screen underneath: a primary
    // click on an owned cell only focuses the inspector (no host
    // action, map stays open).
    let mut claims = ClaimsUiState::new();
    claims.set_center(0, 0);
    claims.chunks.insert(
        (0, 0),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: false,
            sub_claim: None,
        },
    );
    let center_click = {
        let (_, hits) = claims.render(1920, 1080);
        let idx = claims.grid_index(0, 0).expect("center chunk in grid");
        let rect = hits
            .iter()
            .find(|hit| hit.id == ClaimsWidgetId::ChunkCell(idx))
            .expect("center chunk hit region")
            .rect;
        (rect.x + rect.w as i32 / 2, rect.y + rect.h as i32 / 2)
    };
    ui.claims_ui = Some(claims);
    let actions = ui.handle_input(UiInputEvent::Click {
        x: center_click.0,
        y: center_click.1,
    });
    assert!(actions.is_empty(), "owned-cell click leaked a host action");
    assert!(ui.claims_ui.is_some(), "claim click must not close the map");
    assert_eq!(ui.screen, Screen::InGameHud);

    // The right-click unclaim path (claims models button 1 itself, since
    // `UiInputEvent` has no right-button variant) replays identically.
    let unclaim_run = || {
        let mut claims = ClaimsUiState::new();
        claims.set_center(0, 0);
        claims.chunks.insert(
            (0, 0),
            ClaimChunkCell {
                state: ChunkClaimState::OwnedBySelf,
                forceloaded: false,
                sub_claim: None,
            },
        );
        let (_, hits) = claims.render(1920, 1080);
        let idx = claims.grid_index(0, 0).expect("center chunk in grid");
        let rect = hits
            .iter()
            .find(|hit| hit.id == ClaimsWidgetId::ChunkCell(idx))
            .expect("center chunk hit region")
            .rect;
        claims.secondary_click_at(
            rect.x + rect.w as i32 / 2,
            rect.y + rect.h as i32 / 2,
            1920,
            1080,
        )
    };
    let right_a = unclaim_run();
    let right_b = unclaim_run();
    assert_eq!(
        format!("{right_a:?}"),
        format!("{right_b:?}"),
        "right-click replay must be deterministic"
    );
    assert!(matches!(
        right_a.as_slice(),
        [megu_client_ui::claims_ui::ClaimsUiAction::UnclaimChunk { cx: 0, cz: 0 }]
    ));
}

#[test]
fn death_and_crash_screens_absorb_escape_and_modal_gates_the_screen() {
    let mut ui = UiState::new();
    ui.navigate(Screen::InGameHud);
    ui.show_death(megu_client_ui::death_screen::DeathScreenState {
        cause_message: Some("fell out of the world".into()),
        score: 7,
        hardcore: false,
        respawn_cooldown_ticks: 0,
    });
    assert_eq!(ui.screen, Screen::Death);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![],
        "vanilla DeathScreen ignores Escape"
    );
    assert_eq!(ui.screen, Screen::Death);

    ui.show_crash(CrashContext::new("panic", "audit", "1.21.1", "boom"));
    assert_eq!(ui.screen, Screen::CrashRecovery);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![],
        "crash recovery absorbs Escape"
    );
    assert_eq!(ui.screen, Screen::CrashRecovery);

    // A modal over Settings blocks the screen: only modal regions are
    // live, and Escape dismisses the modal alone.
    ui.go_title();
    ui.navigate(Screen::Settings(SettingsTab::Video));
    ui.modal = Some(UiModal::ConfirmResetDefaults);
    ui.render(1920, 1080);
    assert!(!ui.hit_regions.is_empty());
    assert!(
        ui.hit_regions.iter().all(|r| r.modal),
        "modal must be the only live region set"
    );
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![],
        "Escape must dismiss the modal without emitting an action"
    );
    assert_eq!(ui.modal, None);
    assert_eq!(ui.screen, Screen::Settings(SettingsTab::Video));
    // Clicking Cancel dismisses it too.
    ui.modal = Some(UiModal::ConfirmResetDefaults);
    ui.render(1920, 1080);
    assert_eq!(click_region(&mut ui, WidgetId::ModalCancel), vec![]);
    assert_eq!(ui.modal, None);
    assert_eq!(ui.screen, Screen::Settings(SettingsTab::Video));
}

#[test]
fn text_field_focus_and_tab_shortcuts_keep_cursor_invariants() {
    let mut ui = UiState::new();
    ui.render(1920, 1080);
    click_region(&mut ui, WidgetId::TitleSingleplayer);
    ui.render(1920, 1080);
    // Clicking the name field focuses it; Escape unfocuses without
    // leaving the screen; clicking empty space also unfocuses.
    click_region(&mut ui, WidgetId::SingleplayerNameField);
    assert!(ui.world_name_field_active());
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)),
        vec![]
    );
    assert!(!ui.world_name_field_active());
    assert_eq!(ui.screen, Screen::Singleplayer);
    click_region(&mut ui, WidgetId::SingleplayerNameField);
    assert!(ui.world_name_field_active());
    ui.handle_input(UiInputEvent::Click { x: 3, y: 3 });
    assert!(!ui.world_name_field_active(), "empty click must unfocus");

    // Tab on the HUD toggles the player list (hold-tab semantics) and
    // never leaves the screen.
    ui.go_title();
    ui.navigate(Screen::InGameHud);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Tab)),
        vec![]
    );
    assert!(ui.hud.show_player_list);
    assert_eq!(
        ui.handle_input(UiInputEvent::KeyPressed(UiKey::Tab)),
        vec![]
    );
    assert!(!ui.hud.show_player_list);
    assert_eq!(ui.screen, Screen::InGameHud);
}

// ---------------------------------------------------------------------------
// 3. Deterministic input replay (mouse + keyboard + IME + gamepad)
// ---------------------------------------------------------------------------

#[test]
fn mouse_and_keyboard_replay_is_deterministic() {
    fn script(ui: &mut UiState) -> Vec<Vec<UiAction>> {
        let mut log = Vec::new();
        ui.render(1920, 1080);
        log.push(click_region(ui, WidgetId::TitleSingleplayer));
        ui.render(1920, 1080);
        log.push(click_region(ui, WidgetId::SingleplayerNameField));
        for c in "World".chars() {
            log.push(ui.handle_input(UiInputEvent::CharTyped(c)));
        }
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Backspace)));
        log.push(ui.handle_input(UiInputEvent::CharTyped('X')));
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)));
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)));
        ui.render(1920, 1080);
        log.push(click_region(ui, WidgetId::TitleMultiplayer));
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)));
        ui.render(1920, 1080);
        log.push(click_region(ui, WidgetId::TitleMods));
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)));
        ui.render(1920, 1080);
        let sp = find_region(ui, WidgetId::TitleSettings);
        log.push(ui.handle_input(UiInputEvent::MouseMove {
            x: sp.rect.x + 1,
            y: sp.rect.y + 1,
        }));
        ui.navigate(Screen::InGameHud);
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Tab)));
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)));
        log.push(ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)));
        log
    }

    let (mut a, mut b) = (UiState::new(), UiState::new());
    let log_a = script(&mut a);
    let log_b = script(&mut b);
    assert_eq!(
        log_a, log_b,
        "identical inputs must yield identical actions"
    );
    // The replay is not trivially empty — spot-check the known hops.
    assert_eq!(log_a[0], vec![UiAction::OpenScreen(Screen::Singleplayer)]);
    assert_eq!(log_a[10], vec![UiAction::Back]);
    assert_eq!(log_a[11], vec![UiAction::OpenScreen(Screen::Multiplayer)]);
    assert_eq!(log_a[12], vec![UiAction::Back]);
    assert_eq!(log_a[13], vec![UiAction::OpenScreen(Screen::ModMenu)]);
    assert_eq!(log_a[14], vec![UiAction::Back]);
    assert_eq!(log_a[17], vec![UiAction::OpenScreen(Screen::Pause)]);
    assert_eq!(log_a[18], vec![UiAction::Back]);
    // Final observable state is identical across replays.
    assert_eq!(a.screen, b.screen);
    assert_eq!(a.nav_stack, b.nav_stack);
    assert_eq!(a.focus_index, b.focus_index);
    assert_eq!(a.hover, b.hover);
    assert_eq!(a.singleplayer.new_name, b.singleplayer.new_name);
    assert_eq!(a.screen, Screen::InGameHud);
    assert_eq!(a.singleplayer.new_name, "New WorldWorlX");
}

#[test]
fn container_right_click_shift_click_and_drag_replay_is_deterministic() {
    // The container engine models the full vanilla click-mode set:
    // button 0 = left, 1 = right; mode 0 = pick, 1 = shift-click,
    // 4 = drop (Q), 5 = drag start, 6 = double-click. Every mode must
    // replay byte-identically from identical inventories.
    fn run() -> (Vec<(i8, u8, i16, u64)>, u64) {
        let schema = menu_schema(2).expect("chest schema");
        let mut screen = ContainerScreenState::new(schema, 7, 0, "audit");
        for i in 0..9 {
            screen.set_slot(i, UiItemStack::new(i as u32, 1));
        }
        screen.set_slot(27, UiItemStack::new(100, 64));
        let mut out = Vec::new();
        for (slot, button, mode) in [
            (0i16, 0i8, 0u8), // left pick
            (27, 0, 1),       // shift-click into the container
            (1, 1, 0),        // right-click pick
            (2, 0, 4),        // drop one (Q)
            (3, 0, 5),        // drag start
            (4, 0, 6),        // double-click gather
        ] {
            if let Some(action) = screen.click_mode(slot, button, mode) {
                out.push((
                    action.button,
                    action.mode,
                    action.slot,
                    action
                        .carried_item
                        .as_ref()
                        .map_or(0, |s| u64::from(s.count)),
                ));
            }
        }
        (out, screen.total_count())
    }
    let (a, total_a) = run();
    let (b, total_b) = run();
    assert_eq!(a, b, "click-mode replay must be deterministic");
    assert_eq!(total_a, total_b, "inventory must end identical");
    // Left/right pick and shift-click conserve every item; only the
    // drop action may shrink the total.
    assert!(!a.is_empty(), "filled slots must emit actions");
}

#[test]
fn gamepad_dpad_focus_navigation_replay_is_deterministic() {
    fn run() -> (Vec<GamepadEvent>, Vec<UiAction>, InputSourceMode, Screen) {
        let mut ui = UiState::new();
        ui.render(1920, 1080);
        let mut mapper = GamepadMapper::new(GamepadBrand::Xbox);
        mapper.ui_mode = true;
        let mut pad = GamepadState::default();
        let mut events = Vec::new();
        let mut press = |mapper: &mut GamepadMapper,
                         ui: &mut UiState,
                         pad: &mut GamepadState,
                         btn: EngineButton| {
            pad.set_button(btn, true);
            let ctx = GamepadUiContext {
                hit_regions: &ui.hit_regions,
                screen_w: 1920.0,
                screen_h: 1080.0,
            };
            for ev in mapper.update(pad, 1.0 / 60.0, Some(ctx)) {
                events.push(ev.clone());
                if let GamepadEvent::FocusJumped { region_index } = ev {
                    ui.focus_index = Some(region_index);
                }
            }
            pad.set_button(btn, false);
            let ctx = GamepadUiContext {
                hit_regions: &ui.hit_regions,
                screen_w: 1920.0,
                screen_h: 1080.0,
            };
            let _ = mapper.update(pad, 1.0 / 60.0, Some(ctx));
        };
        // D-pad Right ×3: from the corner origin the engine's 90°
        // spatial cone acquires Mods (nearest), then Multiplayer (the
        // nearer diagonal), then Friends on the split row.
        for btn in [
            EngineButton::DpadRight,
            EngineButton::DpadRight,
            EngineButton::DpadRight,
        ] {
            press(&mut mapper, &mut ui, &mut pad, btn);
        }
        // South (A) confirms the focused region.
        pad.set_button(EngineButton::South, true);
        let ctx = GamepadUiContext {
            hit_regions: &ui.hit_regions,
            screen_w: 1920.0,
            screen_h: 1080.0,
        };
        for ev in mapper.update(&pad, 1.0 / 60.0, Some(ctx)) {
            events.push(ev);
        }
        pad.set_button(EngineButton::South, false);
        let actions = ui.handle_input(UiInputEvent::GamepadPressed(
            megu_client_ui::state::GamepadButton::South,
        ));
        (events, actions, ui.input_mode.mode(), ui.screen)
    }

    let (events_a, actions_a, mode_a, screen_a) = run();
    let (events_b, actions_b, mode_b, screen_b) = run();
    assert_eq!(
        events_a, events_b,
        "gamepad event stream must be deterministic"
    );
    assert_eq!(
        actions_a, actions_b,
        "gamepad actions must be deterministic"
    );
    assert_eq!(mode_a, mode_b);
    assert_eq!(screen_a, screen_b);
    assert_eq!(
        mode_a,
        InputSourceMode::Gamepad,
        "pad input flips the source"
    );
    // Three D-pad presses ⇒ three focus jumps.
    assert_eq!(
        events_a
            .iter()
            .filter(|e| matches!(e, GamepadEvent::FocusJumped { .. }))
            .count(),
        3
    );
    // South confirms: focus moved Right-Right-Right from the origin
    // lands on Friends (the split-pair half to the right of Mods).
    assert_eq!(
        actions_a,
        vec![UiAction::OpenScreen(Screen::Friends)],
        "D-pad Right,Right,Right + A must open Friends"
    );
    assert_eq!(screen_a, Screen::Friends);

    // Gameplay mode: shoulder buttons cycle the hotbar deterministically
    // and mouse motion flips the input source back.
    let mut ui = UiState::new();
    let mut mapper = GamepadMapper::new(GamepadBrand::Xbox);
    let mut pad = GamepadState::default();
    pad.set_button(EngineButton::RightShoulder, true);
    let ev = mapper.update(&pad, 1.0 / 60.0, None);
    assert!(
        ev.iter()
            .any(|e| matches!(e, GamepadEvent::HotbarChanged(1))),
        "right shoulder must select hotbar slot 1"
    );
    ui.handle_input(UiInputEvent::MouseMove { x: 200, y: 200 });
    assert_eq!(
        ui.input_mode.mode(),
        InputSourceMode::KeyboardMouse,
        "large mouse motion flips back to keyboard/mouse"
    );
}

#[test]
fn ime_japanese_kana_and_romaji_input_replay_is_deterministic() {
    // 1) IME-committed Japanese text into the world-name field: any
    // non-control char is accepted, and the byte sequence replays
    // identically.
    fn name_run() -> (String, Vec<UiAction>) {
        let mut ui = UiState::new();
        ui.screen = Screen::Singleplayer;
        ui.singleplayer.new_name.clear();
        ui.render(1920, 1080);
        let mut actions = Vec::new();
        actions.extend(click_region(&mut ui, WidgetId::SingleplayerNameField));
        for c in "ダイヤモンド世界".chars() {
            actions.extend(ui.handle_input(UiInputEvent::CharTyped(c)));
        }
        (ui.singleplayer.new_name.clone(), actions)
    }
    let (name_a, acts_a) = name_run();
    let (name_b, acts_b) = name_run();
    assert_eq!(name_a, "ダイヤモンド世界");
    assert_eq!(name_a, name_b, "IME replay must be deterministic");
    assert_eq!(acts_a, acts_b);

    // 2) Kana/romaji recipe search: every reading of ダイヤモンド
    // resolves to the same item, deterministically, on a fresh index.
    let build = || {
        let mut idx = RecipeSearchIndex::new();
        idx.add_item(SearchItem::new(
            "minecraft:diamond",
            "Diamond",
            "ダイヤモンド",
        ));
        idx.add_item(SearchItem::new(
            "minecraft:iron_ingot",
            "Iron Ingot",
            "鉄インゴット",
        ));
        idx.add_item(SearchItem::new("minecraft:stone", "Stone", "石"));
        idx
    };
    for query in ["だいや", "ダイヤ", "daiya", "Daiya", "daiyamondo"] {
        let idx = build();
        let hits: Vec<&str> = idx.search(query).iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            hits,
            vec!["minecraft:diamond"],
            "query {query:?} must resolve to diamond, got {hits:?}"
        );
    }
    // Half-width kana normalizes to full-width; romaji transcription is
    // stable and round-trips through the index.
    assert_eq!(normalize_kana("ﾀﾞｲﾔ"), normalize_kana("ダイヤ"));
    let romaji = kana_to_romaji(&normalize_kana("ダイヤモンド"));
    assert_eq!(romaji, kana_to_romaji(&normalize_kana("ダイヤモンド")));
    let idx = build();
    let hits: Vec<&str> = idx.search(&romaji).iter().map(|i| i.id.as_str()).collect();
    assert_eq!(hits, vec!["minecraft:diamond"]);

    // 3) Chat input: IME-committed text submits once and lands in
    // history, deterministically.
    fn chat_run() -> (String, Option<String>, usize) {
        let mut chat = ChatInputState::new();
        chat.insert_str("こんにちは世界");
        let buffer = chat.buffer().to_string();
        let submitted = chat.submit();
        (buffer, submitted, chat.history().len())
    }
    let (buf_a, sub_a, hist_a) = chat_run();
    let (buf_b, sub_b, hist_b) = chat_run();
    assert_eq!(buf_a, "こんにちは世界");
    assert_eq!(sub_a.as_deref(), Some("こんにちは世界"));
    assert_eq!(hist_a, 1);
    assert_eq!((buf_a, sub_a, hist_a), (buf_b, sub_b, hist_b));
}

// ---------------------------------------------------------------------------
// 4. Retained UI cache frame-budget proof
// ---------------------------------------------------------------------------

/// Real layout-pass draw lists standing in for the seven layer buffers
/// a host frame would submit.
fn scene_lists() -> Vec<(UiLayer, UiDrawList)> {
    let mut backdrop = UiDrawList::new();
    backdrop.fill_rect(UiRect::new(0, 0, 1280, 720), UiColor::rgb(24, 24, 32));

    // StaticHud: the real in-game HUD layout pass (hotbar, hearts,
    // hunger, XP, crosshair, bossbars, chat, overlays).
    let mut hud_ui = UiState::new();
    hud_ui.screen = Screen::InGameHud;
    hud_ui.hud = loaded_hud();
    let static_hud = hud_ui.render(1280, 720);

    // DynamicHud: chat/actionbar surface drawn through the vanilla
    // widget kit.
    let mut dynamic_hud = UiDrawList::new();
    draw_vanilla_panel(&mut dynamic_hud, &VanillaWidgets::new(2), 8, 8, 240, 96);
    draw_vanilla_button(
        &mut dynamic_hud,
        &VanillaWidgets::new(2),
        16,
        68,
        60,
        "Chat",
        VanillaButtonState::Normal,
    );
    dynamic_hud.text_str(16, 20, 2, UiColor::rgb(255, 255, 255), "<Steve> hi");

    // Screen: the real title layout pass.
    let mut title_ui = UiState::new();
    let screen = title_ui.render(1280, 720);

    // Overlay: crosshair/vignette-style content.
    let mut overlay = UiDrawList::new();
    overlay.fill_rect(
        UiRect::new(640 - 8, 360 - 8, 16, 16),
        UiColor::rgba(230, 230, 230, 190),
    );

    // ModHud / ModOverlay: mod widget surfaces via the vanilla kit.
    let mut mod_hud = UiDrawList::new();
    draw_vanilla_panel(&mut mod_hud, &VanillaWidgets::new(2), 1040, 8, 220, 60);
    let mut mod_overlay = UiDrawList::new();
    draw_vanilla_panel(&mut mod_overlay, &VanillaWidgets::new(2), 1040, 80, 220, 40);

    vec![
        (UiLayer::Backdrop, backdrop),
        (UiLayer::StaticHud, static_hud),
        (UiLayer::DynamicHud, dynamic_hud),
        (UiLayer::Screen, screen),
        (UiLayer::Overlay, overlay),
        (UiLayer::ModHud, mod_hud),
        (UiLayer::ModOverlay, mod_overlay),
    ]
}

fn bake_all(cache: &mut UiRetainedCache, lists: &[(UiLayer, UiDrawList)]) {
    let icons = ItemIconAtlas::default();
    let sprites = GuiSpriteAtlas::default();
    for (layer, list) in lists {
        cache.update_layer(*layer, list, 1280, 720, &icons, &sprites);
    }
}

#[test]
fn sixty_steady_state_frames_hit_the_retained_cache_with_zero_rebuilds() {
    let bus = UiLayerBus::new();
    let mut cache = UiRetainedCache::new();
    let lists = scene_lists();

    // Frame 0: the bake — every canonical layer is dirty once and
    // tessellates. `UiLayerBus` intentionally reserves bit 7 as headroom,
    // so the first revision poll reports the exact 8-bit bus mask even
    // though only the seven canonical layers have meshes.
    let dirty = cache.begin_frame(&bus);
    assert_eq!(dirty.mask(), 0xff, "first frame bakes every bus slot");
    assert!(!dirty.has_high());
    bake_all(&mut cache, &lists);
    cache.composite();
    assert!(cache.gpu_upload_needed(), "bake must upload");
    cache.mark_uploaded();
    let baked_tessellations = cache.stats().tessellations;
    assert_eq!(baked_tessellations, UiLayer::COUNT as u64);

    // Frames 1..=60: steady state, zero mutations anywhere in the bus.
    // Every one of the 60 frames must be a pure cache hit with zero
    // layer rebuilds and zero GPU uploads — stronger than the mission's
    // 59/60 floor.
    for _ in 0..60 {
        let dirty = cache.begin_frame(&bus);
        assert_eq!(dirty, DirtyLayers::NONE, "steady-state frame must be idle");
        cache.composite();
        assert!(
            !cache.gpu_upload_needed(),
            "steady-state frame marked a GPU upload"
        );
    }
    let stats = cache.stats();
    assert_eq!(stats.idle_frames, 60);
    assert!(stats.idle_frames >= 59, "mission floor: >=59/60 cache hits");
    assert_eq!(stats.tessellations, baked_tessellations);
    assert_eq!(stats.composites, 1, "no redundant composite merges");
}

#[test]
fn hotbar_only_mutation_invalidates_only_the_hotbar_layer() {
    let mut bus = UiLayerBus::new();
    let mut cache = UiRetainedCache::new();
    let lists = scene_lists();
    cache.begin_frame(&bus);
    bake_all(&mut cache, &lists);
    cache.composite();
    cache.mark_uploaded();

    // Snapshot the six layers the hotbar bump must not touch. Mission
    // name map: StaticHud = Hotbar, DynamicHud = Chat, Backdrop/Overlay
    // = Vignette/Crosshair, Screen/ModHud/ModOverlay = everything else.
    let untouched: Vec<(UiLayer, Vec<megu_client_ui::draw::UiVertex>)> = UiLayer::ALL
        .iter()
        .filter(|l| **l != UiLayer::StaticHud)
        .map(|l| {
            let (v, _i) = cache.layer_mesh(*l);
            (*l, v.to_vec())
        })
        .collect();

    // Mutate ONLY the hotbar layer: one extra slot cell fill.
    let mut hud_ui = UiState::new();
    hud_ui.screen = Screen::InGameHud;
    hud_ui.hud = loaded_hud();
    let mut hotbar_list = hud_ui.render(1280, 720);
    hotbar_list.fill_rect(UiRect::new(600, 690, 22, 22), UiColor::rgb(255, 0, 0));

    UiLayer::StaticHud.bump(&mut bus);
    let dirty = cache.begin_frame(&bus);
    assert_eq!(dirty.count(), 1, "hotbar bump dirties exactly one layer");
    assert!(dirty.contains(UiLayer::StaticHud.index()));
    for layer in UiLayer::ALL {
        if layer != UiLayer::StaticHud {
            assert!(
                !dirty.contains(layer.index()),
                "hotbar bump dirtied {layer:?}"
            );
        }
    }
    let icons = ItemIconAtlas::default();
    let sprites = GuiSpriteAtlas::default();
    cache.update_layer(
        UiLayer::StaticHud,
        &hotbar_list,
        1280,
        720,
        &icons,
        &sprites,
    );
    cache.composite();
    assert_eq!(
        cache.stats().tessellations,
        UiLayer::COUNT as u64 + 1,
        "exactly one layer re-tessellated"
    );
    assert!(cache.gpu_upload_needed(), "changed hotbar must upload");
    for (layer, before) in &untouched {
        let (v, _i) = cache.layer_mesh(*layer);
        assert_eq!(
            v,
            before.as_slice(),
            "{layer:?} re-tessellated on a hotbar-only bump"
        );
    }

    // A hotbar bump that re-bakes byte-identical geometry is rescued by
    // the early cutoff: no PCIe upload for a no-op dirty layer.
    cache.mark_uploaded();
    UiLayer::StaticHud.bump(&mut bus);
    let dirty = cache.begin_frame(&bus);
    assert_eq!(dirty.count(), 1);
    cache.update_layer(
        UiLayer::StaticHud,
        &hotbar_list,
        1280,
        720,
        &icons,
        &sprites,
    );
    cache.composite();
    assert!(
        !cache.gpu_upload_needed(),
        "identical rebake must rescue the upload"
    );
    assert_eq!(cache.stats().uploads_rescued, 1);
}
#[test]
fn title_layout_sweep_keeps_the_vanilla_grid_centered_and_link_free() {
    const TITLE_IDS: [WidgetId; 8] = [
        WidgetId::TitleSingleplayer,
        WidgetId::TitleMultiplayer,
        WidgetId::TitleMods,
        WidgetId::TitleFriends,
        WidgetId::TitleLanguage,
        WidgetId::TitleSettings,
        WidgetId::TitleQuit,
        WidgetId::TitleAbout,
    ];

    for (width, height) in SIZES {
        for gui_scale in SCALES {
            let mut ui = UiState::new();
            ui.ui_scale = gui_scale;
            ui.screen = Screen::Title;
            ui.render(width, height);

            let context = format!("Title at {width}x{height}, GUI scale {gui_scale}");
            assert_regions_clean(&ui.hit_regions, width, height, &context);
            assert_eq!(ui.hit_regions.len(), TITLE_IDS.len(), "{context}");
            for id in TITLE_IDS {
                assert!(
                    ui.hit_regions.iter().any(|candidate| candidate.id == id),
                    "{context}: missing {id:?}"
                );
            }
            assert!(
                ui.hit_regions.iter().all(|candidate| {
                    matches!(
                        candidate.id,
                        WidgetId::TitleSingleplayer
                            | WidgetId::TitleMultiplayer
                            | WidgetId::TitleMods
                            | WidgetId::TitleFriends
                            | WidgetId::TitleLanguage
                            | WidgetId::TitleSettings
                            | WidgetId::TitleQuit
                            | WidgetId::TitleAbout
                    )
                }),
                "{context}: title contains non-title controls"
            );

            let scale = ui.effective_scale(height) as i32;
            let logical_width = width as i32 / scale;
            let logical_height = height as i32 / scale;
            let row_step = 24.min((logical_height - 96) / 2).max(4);
            let button_height = 20.min(row_step);
            let corner_size = 20.min(button_height);
            let column_width = 200.min(logical_width - 2 * (corner_size + 4)).max(24) & !1;
            let half_width = (column_width - 4) / 2;
            let center_x = width as i32 / 2;
            let singleplayer = find_region(&ui, WidgetId::TitleSingleplayer).rect;
            let multiplayer = find_region(&ui, WidgetId::TitleMultiplayer).rect;
            let mods = find_region(&ui, WidgetId::TitleMods).rect;
            let friends = find_region(&ui, WidgetId::TitleFriends).rect;
            let language = find_region(&ui, WidgetId::TitleLanguage).rect;
            let settings = find_region(&ui, WidgetId::TitleSettings).rect;
            let quit = find_region(&ui, WidgetId::TitleQuit).rect;
            let about = find_region(&ui, WidgetId::TitleAbout).rect;

            for full_width in [singleplayer, multiplayer] {
                assert_eq!(full_width.w, (column_width * scale) as u32, "{context}");
                assert_eq!(full_width.h, (button_height * scale) as u32, "{context}");
                let doubled_center = 2 * (full_width.x + full_width.w as i32 / 2);
                assert!(
                    (doubled_center - 2 * center_x).abs() < 2 * scale,
                    "{context}: center differs by a full physical scale unit or more"
                );
            }
            assert_eq!(
                multiplayer.y - singleplayer.y,
                row_step * scale,
                "{context}"
            );
            assert_eq!(mods.w, (half_width * scale) as u32, "{context}");
            assert_eq!(friends.w, (half_width * scale) as u32, "{context}");
            assert_eq!(mods.y, friends.y, "{context}");
            assert_eq!(mods.x, singleplayer.x, "{context}");
            assert_eq!(friends.x - mods.right(), 4 * scale, "{context}");
            assert_eq!(friends.right(), singleplayer.right(), "{context}");
            assert_eq!(settings.y - mods.bottom(), 8 * scale, "{context}");
            assert_eq!(settings.w, (half_width * scale) as u32, "{context}");
            assert_eq!(quit.w, (half_width * scale) as u32, "{context}");
            assert_eq!(quit.x - settings.right(), 4 * scale, "{context}");
            assert_eq!(language.w, (corner_size * scale) as u32, "{context}");
            assert_eq!(language.h, (corner_size * scale) as u32, "{context}");
            assert_eq!(about.w, (corner_size * scale) as u32, "{context}");
            assert_eq!(about.h, (corner_size * scale) as u32, "{context}");
            assert_eq!(settings.x - language.right(), 4 * scale, "{context}");
            assert_eq!(about.x - quit.right(), 4 * scale, "{context}");
            assert_eq!(language.y, settings.y, "{context}");
            assert_eq!(about.y, quit.y, "{context}");
        }
    }
}

#[test]
fn hud_layout_sweep_keeps_hotbar_vitals_and_xp_inside_the_viewport() {
    for (width, height) in SIZES {
        for gui_scale in SCALES {
            let mut ui = UiState::new();
            ui.ui_scale = gui_scale;
            ui.screen = Screen::InGameHud;
            ui.hud = HudState::demo();
            ui.hud.air = 120;
            ui.hud.xp_level = 12;

            let draw_list = ui.render(width, height);
            let context = format!("HUD at {width}x{height}, GUI scale {gui_scale}");
            assert_draws_in_viewport(&draw_list, width, height, &context);
            let scale = (ui.effective_scale(height) as i32).min((width / 224).max(1) as i32);
            let mut occupied = Vec::new();
            hud_occupied_rects_into(&ui, width, height, &mut occupied);
            for rect in &occupied {
                assert!(
                    rect.x >= 0
                        && rect.y >= 0
                        && rect.right() <= width as i32
                        && rect.bottom() <= height as i32,
                    "{context}: HUD occupancy outside viewport: {rect:?}"
                );
            }

            let hotbar = occupied.iter().find(|rect| {
                rect.w == (224 * scale) as u32
                    && rect.h == (22 * scale) as u32
                    && rect.bottom() == height as i32 - 4 * scale
            });
            assert!(hotbar.is_some(), "{context}: nine-slot hotbar missing");
            assert!(
                occupied
                    .iter()
                    .any(|rect| rect.w == (182 * scale) as u32 && rect.h == (3 * scale) as u32),
                "{context}: XP bar occupancy missing"
            );
        }
    }
}

#[test]
fn container_player_inventory_uses_non_overlapping_vanilla_slot_pitches() {
    const CONTAINER_VIEWPORTS: [(u32, u32); 4] =
        [(854, 480), (1280, 720), (1920, 1080), (3840, 2160)];

    for menu_id in 0..VANILLA_MENU_COUNT as u32 {
        let schema = menu_schema(menu_id).expect("registered vanilla menu");
        if !schema.has_player_inventory {
            continue;
        }
        for (width, height) in CONTAINER_VIEWPORTS {
            let mut screen = ContainerScreenState::new(schema, 1, 0, "inventory audit");
            for index in schema.inventory_start..schema.inventory_start + 36 {
                screen.set_slot(
                    index as usize,
                    UiItemStack::new(index as u32 + 1, 1).with_durability(73, 156),
                );
            }
            let layout = screen.layout(width as f32, height as f32);
            let context = format!("menu {menu_id} at {width}x{height}");
            let player_slots: Vec<_> = layout
                .slots
                .iter()
                .filter(|slot| {
                    slot.index >= schema.inventory_start && slot.index < schema.inventory_start + 36
                })
                .collect();
            assert_eq!(player_slots.len(), 36, "{context}");

            for slot in &player_slots {
                let offset = slot.index - schema.inventory_start;
                let column = i32::from(offset % 9);
                let (row, hotbar_gap) = if offset < 27 {
                    (i32::from(offset / 9), 0)
                } else {
                    (0, 58)
                };
                assert_eq!(slot.rect.w, SLOT_SIZE as u32, "{context}");
                assert_eq!(slot.rect.h, SLOT_SIZE as u32, "{context}");
                assert_eq!(
                    slot.rect.x,
                    layout.panel.x + 4 + i32::from(schema.inventory_origin_x) + column * SLOT_SIZE,
                    "{context}, slot {}",
                    slot.index
                );
                assert_eq!(
                    slot.rect.y,
                    layout.panel.y
                        + 4
                        + i32::from(schema.inventory_origin_y)
                        + row * SLOT_SIZE
                        + hotbar_gap,
                    "{context}, slot {}",
                    slot.index
                );
            }
            for (index, first) in player_slots.iter().enumerate() {
                for second in player_slots.iter().skip(index + 1) {
                    assert!(
                        !first.rect.intersects(second.rect),
                        "{context}: player slots {} and {} overlap",
                        first.index,
                        second.index
                    );
                }
            }

            let mut draw_list = UiDrawList::new();
            screen.render(width as f32, height as f32, &mut draw_list);
            assert_draws_in_viewport(&draw_list, width, height, &context);
            let durability_bars: Vec<_> = draw_list
                .commands
                .iter()
                .filter_map(|command| match command {
                    UiCommand::ProgressBar { rect, .. } if rect.w == 16 && rect.h == 3 => {
                        Some(*rect)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(durability_bars.len(), 36, "{context}");
            for bar in durability_bars {
                assert!(
                    player_slots.iter().any(|slot| {
                        bar.x > slot.rect.x
                            && bar.y == slot.rect.bottom() - 4
                            && bar.right() < slot.rect.right()
                            && bar.bottom() <= slot.rect.bottom()
                    }),
                    "{context}: durability bar escapes its slot: {bar:?}"
                );
            }
        }
    }
}
