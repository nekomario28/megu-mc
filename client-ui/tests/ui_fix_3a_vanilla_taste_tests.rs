// SPDX-License-Identifier: LGPL-3.0-or-later
//! Regression coverage for the vanilla-taste layout/CJK audit lane:
//! ultra-narrow chat safety, CJK caret measurement, digest UTF-8 safety,
//! scroll reachability, pause-grid hitboxes, compact Mods & Packs, and
//! live Built-in Modules settings synchronization.

use megu_client_ui::draw::{rasterize_draw_list, UiCommand};
use megu_client_ui::layout::layout_screen;
use megu_client_ui::mod_manager::{ModManagerFilter, ModManagerWidgetId};
use megu_client_ui::state::{
    HitRegion, LanguageSetting, ModSyncEntry, Screen, SettingsTab, UiInputEvent, UiState,
    WidgetId,
};

fn assert_no_overlaps(regions: &[HitRegion]) {
    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            assert!(
                !a.rect.intersects(b.rect),
                "hit regions overlap: {a:?} vs {b:?}"
            );
        }
    }
}

fn region(regions: &[HitRegion], id: WidgetId) -> &HitRegion {
    regions
        .iter()
        .find(|region| region.id == id)
        .unwrap_or_else(|| panic!("missing hit region {id:?}"))
}

fn assert_non_blank(rgba: &[u8]) {
    assert!(rgba.len().is_multiple_of(4));
    let (pixels, _) = rgba.as_chunks::<4>();
    let non_transparent = pixels.iter().filter(|p| p[3] != 0).count();
    assert!(non_transparent > 100, "frame is blank");
    let first = pixels[0];
    let different = pixels.iter().filter(|&&p| p != first).count();
    assert!(different > 100, "frame is a flat fill");
}

#[test]
fn narrow_80x600_open_chat_never_panics() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud.chat_input.open = true;
    ui.hud.chat_input.text = "狭い画面でもチャットはpanicしない".into();
    ui.hud.chat_input.cursor = ui.hud.chat_input.char_count();
    let (draw, _regions) = layout_screen(&ui, 80, 600);
    assert!(!draw.is_empty(), "narrow chat emitted no draw commands");
}

#[test]
fn japanese_chat_caret_uses_cjk_glyph_advance_and_scrolls() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud.chat_input.open = true;

    ui.hud.chat_input.text = "あ".into();
    ui.hud.chat_input.cursor = 1;
    let (draw, _) = layout_screen(&ui, 320, 240);
    let cjk_caret = draw
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::FillRect { rect, .. }
                if rect.w == 1 && rect.h == 7 && rect.y >= 220 => Some(rect.x),
            _ => None,
        })
        .max()
        .expect("chat caret fill missing");
    assert_eq!(cjk_caret, 16, "one CJK glyph must advance the caret by 12px from x=4");

    ui.hud.chat_input.text = "いろはにほへとちりぬるをわかよたれそ".into();
    ui.hud.chat_input.cursor = ui.hud.chat_input.char_count();
    let (draw, _) = layout_screen(&ui, 320, 240);
    let visible = draw
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            UiCommand::Text { x, y, text, .. } if *x == 4 && *y >= 220 => Some(text.as_str()),
            _ => None,
        })
        .next_back()
        .expect("visible chat input text missing");
    assert!(
        !visible.starts_with('い'),
        "long CJK draft did not scroll around the caret: {visible:?}"
    );
    assert!(visible.ends_with('そ'), "caret-tail text should stay visible: {visible:?}");
}

#[test]
fn sha256_short_digest_is_utf8_boundary_safe() {
    let entry = ModSyncEntry::added("cjk", "日本語日本語日本語");
    assert_eq!(entry.short_sha(), "");
    let valid = ModSyncEntry::added("hex", "aabbccddeeff00112233445566778899");
    assert_eq!(valid.short_sha(), "aabbccddeeff");
}

#[test]
fn about_screen_1200x900_scale4_has_zero_overlaps_en_and_ja() {
    for japanese in [false, true] {
        let mut ui = UiState::new();
        ui.screen = Screen::AboutMegu;
        ui.ui_scale = 4;
        if japanese {
            ui.settings.language = LanguageSetting::Japanese;
        }
        let (_draw, regions) = layout_screen(&ui, 1200, 900);
        assert!(!regions.is_empty(), "about screen has no hit regions");
        assert_no_overlaps(&regions);
    }
}

#[test]
fn mods_and_packs_800x600_scale4_is_non_blank_with_clickable_done() {
    let mut ui = UiState::new();
    ui.screen = Screen::ModManager;
    ui.ui_scale = 4;
    ui.mod_manager.entries = vec![
        megu_client_ui::mod_manager::ModManagerEntry::new("create", "Create", "0.5.1f"),
        megu_client_ui::mod_manager::ModManagerEntry::new("jei", "Just Enough Items", "15.3.0"),
    ];
    let draw = ui.render(800, 600);
    assert!(!draw.is_empty(), "compact Mods & Packs emitted no commands");
    assert_non_blank(&rasterize_draw_list(800, 600, &draw));
    let done = region(
        &ui.hit_regions,
        WidgetId::ModManager(ModManagerWidgetId::Done),
    );
    assert!(done.rect.right() <= 800 && done.rect.bottom() <= 600);
    let x = done.rect.x + done.rect.w as i32 / 2;
    let y = done.rect.y + done.rect.h as i32 / 2;
    let actions = ui.handle_input(UiInputEvent::Click { x, y });
    assert!(
        actions.iter().any(|action| matches!(action, megu_client_ui::state::UiAction::Back)),
        "clicking Done must close Mods & Packs, got {actions:?}"
    );
}

#[test]
fn controls_and_qol_scroll_reach_bottom_at_800x600_scale3_and_4() {
    for scale in [3u8, 4u8] {
        for (tab, bottom_id) in [
            (SettingsTab::Controls, WidgetId::VideoInvertY),
            (SettingsTab::Qol, WidgetId::QolBuiltin(2)),
        ] {
            let mut ui = UiState::new();
            ui.screen = Screen::Settings(tab);
            ui.ui_scale = scale;
            let _ = ui.render(800, 600);
            ui.scroll_menu(10_000);
            let _ = ui.render(800, 600);
            let bottom = region(&ui.hit_regions, bottom_id);
            assert!(
                bottom.rect.y >= 0
                    && bottom.rect.right() <= 800
                    && bottom.rect.bottom() <= 600,
                "scale {scale} {tab:?} bottom widget is unreachable: {bottom:?}"
            );
        }
    }
}

#[test]
fn pause_screen_uses_vanilla_two_column_disjoint_grid() {
    let mut ui = UiState::new();
    ui.screen = Screen::Pause;
    let (_draw, regions) = layout_screen(&ui, 800, 600);
    assert_no_overlaps(&regions);

    let scale = ui.effective_scale(600) as i32;
    let back = region(&regions, WidgetId::PauseBackToGame).rect;
    let advancements = region(&regions, WidgetId::PauseAdvancements).rect;
    let statistics = region(&regions, WidgetId::PauseStatistics).rect;
    let options = region(&regions, WidgetId::PauseSettings).rect;
    let invite = region(&regions, WidgetId::PauseInvite).rect;
    let save_quit = region(&regions, WidgetId::PauseSaveQuit).rect;

    assert_eq!(advancements.y, statistics.y, "row 1 split buttons must align");
    assert_eq!(options.y, invite.y, "row 2 split buttons must align");
    assert!(advancements.right() < statistics.x, "row 1 columns must be disjoint");
    assert!(options.right() < invite.x, "row 2 columns must be disjoint");
    assert_eq!(back.w / scale as u32, 204, "full-width rows use the vanilla 204px logical width");
    assert_eq!(save_quit.w, back.w, "Save & Quit keeps the full vanilla column width");
    assert!(
        (advancements.w as i32 - statistics.w as i32).abs() <= scale,
        "split columns differ by more than one scaled pixel"
    );
}

#[test]
fn builtin_module_toggles_mutate_and_reflect_live_settings() {
    let mut ui = UiState::new();
    ui.screen = Screen::ModManager;
    ui.mod_manager.activate(ModManagerWidgetId::FilterBuiltinModules);
    assert_eq!(ui.mod_manager.filter, ModManagerFilter::BuiltinModules);

    let initial = ui.settings.qol.recipe_overlay_enabled;
    let _ = ui.render(800, 600);
    let toggle = region(
        &ui.hit_regions,
        WidgetId::ModManager(ModManagerWidgetId::BuiltinToggle(0)),
    );
    let x = toggle.rect.x + toggle.rect.w as i32 / 2;
    let y = toggle.rect.y + toggle.rect.h as i32 / 2;
    let actions = ui.handle_input(UiInputEvent::Click { x, y });
    assert!(
        actions.iter().any(|action| matches!(action, megu_client_ui::state::UiAction::SettingsChanged)),
        "builtin toggle did not report a settings mutation: {actions:?}"
    );
    assert_ne!(ui.settings.qol.recipe_overlay_enabled, initial);

    let _ = ui.render(800, 600);
    assert_eq!(
        ui.mod_manager.builtin_module_enabled(0),
        ui.settings.qol.recipe_overlay_enabled,
        "the next frame must mirror the live settings flag"
    );

    ui.mod_manager.plamo_panel.auto_pack_enabled = true;
    ui.mod_manager.activate(ModManagerWidgetId::ListScrollDown);
    let _ = ui.render(800, 600);
    assert!(ui.mod_manager.builtin_module_enabled(4));
    let toggle = region(
        &ui.hit_regions,
        WidgetId::ModManager(ModManagerWidgetId::BuiltinToggle(4)),
    );
    let x = toggle.rect.x + toggle.rect.w as i32 / 2;
    let y = toggle.rect.y + toggle.rect.h as i32 / 2;
    let actions = ui.handle_input(UiInputEvent::Click { x, y });
    assert!(
        actions.iter().any(|action| matches!(
            action,
            megu_client_ui::state::UiAction::ModManager(
                megu_client_ui::mod_manager::ModManagerAction::ToggleAutoTranslationPack(false)
            )
        )),
        "PLaMo row must drive the real auto-pack toggle action: {actions:?}"
    );
    let _ = ui.render(800, 600);
    assert!(!ui.mod_manager.builtin_module_enabled(4));
}



#[test]
fn builtin_modules_japanese_labels_have_zero_tofu_glyphs() {
    use megu_client_ui::draw::glyph_is_covered;
    use megu_client_ui::mod_manager::{
        BUILTIN_MODULE_DESCRIPTIONS_JA, BUILTIN_MODULE_NAMES_JA,
    };
    let mut missing = std::collections::BTreeSet::new();
    for text in BUILTIN_MODULE_NAMES_JA
        .iter()
        .chain(BUILTIN_MODULE_DESCRIPTIONS_JA.iter())
        .chain(["内蔵モジュール", "内蔵", "設定"].iter())
    {
        for c in text.chars() {
            if !glyph_is_covered(c) {
                missing.insert(c);
            }
        }
    }
    assert!(missing.is_empty(), "built-in module tofu glyphs: {missing:?}");

    for text in [
        "看板のテキスト編集",
        "難易度: ノーマル",
        "保存してタイトルへ",
        "フレンドを招待",
    ] {
        assert!(
            text.chars().all(glyph_is_covered),
            "lane UI string contains tofu glyphs: {text}"
        );
    }
}

#[test]
fn builtin_modules_hitboxes_are_disjoint_at_compact_sizes() {
    for (w, h, scale) in [(800, 600, 1u8), (800, 600, 4u8), (400, 300, 1u8)] {
        let mut ui = UiState::new();
        ui.screen = Screen::ModManager;
        ui.ui_scale = scale;
        ui.mod_manager.activate(ModManagerWidgetId::FilterBuiltinModules);
        let _ = ui.render(w, h);
        assert_no_overlaps(&ui.hit_regions);
    }
}

#[test]
fn dynamic_max_health_rows_and_status_hearts_emit_sprite_or_fallback_rows() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud.health = 39.0;
    ui.hud.max_health = 40.0;
    ui.hud.heart_style = megu_client_ui::state::HeartStyle::Poisoned;
    let (draw, _) = layout_screen(&ui, 800, 600);
    // Two max-health rows must produce more heart-row cells than the vanilla
    // single row, even on the procedural no-atlas fallback (8×8 dark cells).
    let dark_cells = draw
        .commands
        .iter()
        .filter(|cmd| matches!(cmd, UiCommand::FillRect { rect, .. } if rect.w == 16 && rect.h == 16 && rect.y > 500))
        .count();
    assert!(dark_cells >= 20, "expected multiple max-health rows, got {dark_cells} cells");
}

#[test]
fn hold_tab_player_list_uses_multiple_vanilla_columns() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.hud.show_player_list = true;
    ui.hud.players = (0..30)
        .map(|i| (format!("Player{i:02}"), 20 + i as u16))
        .collect();
    let (draw, _) = layout_screen(&ui, 800, 600);
    let x_for = |needle: &str| {
        draw.commands
            .iter()
            .filter_map(|cmd| match cmd {
                UiCommand::Text { x, text, .. } if text == needle => Some(*x),
                _ => None,
            })
            .next()
            .unwrap_or_else(|| panic!("missing player label {needle}"))
    };
    assert!(
        x_for("Player00") != x_for("Player15"),
        "30 players must flow into a second vanilla tab-list column"
    );
}

#[test]
fn create_world_payload_carries_game_mode_and_difficulty() {
    let mut ui = UiState::new();
    ui.singleplayer.game_mode = megu_client_ui::state::SingleplayerGameMode::Hardcore;
    ui.singleplayer.difficulty = megu_client_ui::state::SingleplayerDifficulty::Hard;
    ui.singleplayer.new_name = "  Hardcore World  ".into();
    ui.singleplayer.seed_text = "12345".into();
    let payload = ui.create_world_payload();
    assert_eq!(payload.name, "Hardcore World");
    assert_eq!(payload.seed, Some(12345));
    assert_eq!(payload.game_mode, megu_client_ui::state::SingleplayerGameMode::Hardcore);
    assert_eq!(payload.difficulty, megu_client_ui::state::SingleplayerDifficulty::Hard);
}
