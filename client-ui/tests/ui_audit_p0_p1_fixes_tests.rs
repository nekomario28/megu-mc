// SPDX-License-Identifier: LGPL-3.0-or-later
//! Regression coverage for the Alpha5 all-UI audit fixes owned by the
//! standalone screen/widget modules.

use megu_client_ui::container_screen::{
    menu_schema, ContainerScreenState, UiItemStack, WorkstationAction,
};
use megu_client_ui::creative_screen::{
    CreativeClick, CreativeInventoryAction, CreativeInventoryState, CreativeTab,
};
use megu_client_ui::draw::{
    palette, text_width, GuiSpriteAtlas, GuiSpriteEntry, UiCommand, UiDrawList,
};
use megu_client_ui::gameplay_screens::{
    BookEditorAction, BookEditorButton, BookEditorMode, BookEditorState, EndCreditsState,
};
use megu_client_ui::magic_hud::{
    build_magic_hud_draw_list_localized, CastBarState, MagicHudState,
};
use megu_client_ui::mod_sync_ui::{
    ModSyncDiffEntry, ModSyncDiffKind, ModSyncUiState, ModSyncWidgetId,
};
use megu_client_ui::recipe_browser::{ItemEntry, RecipeBrowserState, RecipeIndex, RecipeWidgetId};
use megu_client_ui::recipe_overlay::{render_item_tooltip, OverlayItem};
use megu_client_ui::state::UiLang;
use megu_client_ui::vanilla_widgets::{tooltip_row_budget, wrap_text_px};

fn text_commands(list: &UiDrawList) -> impl Iterator<Item = (i32, i32, u32, &str)> {
    list.commands.iter().filter_map(|cmd| match cmd {
        UiCommand::Text { x, y, scale, text, .. } => Some((*x, *y, *scale, text.as_str())),
        _ => None,
    })
}

fn click_container_pos(
    state: &mut ContainerScreenState,
    x: i32,
    y: i32,
) -> Option<WorkstationAction> {
    let panel = state.panel_rect(800.0, 600.0);
    state.click_at(panel.x + 4 + x, panel.y + 4 + y, 800.0, 600.0)
}

#[test]
fn recipe_browser_compact_layout_is_nonblank_at_200_and_160_logical_px() {
    let mut index = RecipeIndex::new();
    index.add_item(ItemEntry::new("minecraft:iron_ingot", "Iron Ingot"));
    index.add_item(ItemEntry::new("minecraft:stick", "Stick"));
    for i in 0..16 {
        let id = format!("demo:item_{i}");
        let name = format!("Demo Item {i}");
        index.add_item(ItemEntry::new(&id, &name));
    }
    let mut state = RecipeBrowserState::new(index);
    state.ui_scale = 4;

    // 800×600 at fixed GUI scale 4 is exactly the 200×150 logical case that
    // used to hit the hard blank-screen cutoff.
    let (draw, hits) = state.render(800, 600);
    assert!(draw.commands.len() > 10, "compact browser must not be blank");
    assert!(hits.iter().any(|h| h.id == RecipeWidgetId::SearchField));
    assert!(hits.iter().any(|h| matches!(h.id, RecipeWidgetId::ItemSlot(0))));
    for hit in &hits {
        assert!(hit.rect.x >= 0 && hit.rect.y >= 0);
        assert!(hit.rect.right() <= 800 && hit.rect.bottom() <= 600);
    }

    // The compact floor is 160 logical px (640 physical at scale 4).
    let (draw, hits) = state.render(640, 400);
    assert!(draw.commands.len() > 10);
    assert!(hits.iter().any(|h| h.id == RecipeWidgetId::SearchField));
    assert!(hits.iter().any(|h| matches!(h.id, RecipeWidgetId::ItemSlot(0))));
}

#[test]
fn creative_catalog_geometry_search_pick_and_real_hotbar_assignment() {
    let mut creative = CreativeInventoryState::default();
    assert!(CreativeTab::ALL.len() >= 10);

    // Building Blocks is populated without any host inventory sync, and the
    // registry id is the protocol-767 numeric id rather than a placeholder.
    assert_eq!(creative.tab, CreativeTab::BuildingBlocks);
    assert!(creative.filtered_items().any(|item| item.name == "minecraft:stone" && item.item_id == 1));

    // Search is the deduplicated union of every tab and resolves previously
    // wrong/missing vanilla names to their real numeric ids.
    creative.click(CreativeClick::Tab(CreativeTab::Search));
    creative.set_search("ender_eye");
    let matches: Vec<_> = creative.filtered_items().collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "minecraft:ender_eye");
    assert_eq!(matches[0].item_id, 1006);
    creative.set_search("cooked_beef");
    let beef = creative.filtered_items().next().expect("cooked beef in food catalog");
    assert_eq!(beef.item_id, 989);
    creative.set_search("ender_pearl");
    assert_eq!(creative.filtered_items().count(), 1, "multi-tab items dedupe in search");

    // Typed search input is live and UTF-8-safe.
    creative.set_search("");
    creative.type_search_char('石');
    assert_eq!(creative.search, "石");
    creative.search_backspace();
    assert!(creative.search.is_empty());

    // Exact shared hit-test geometry: the ninth hotbar slot ends before the
    // trash well starts, and centers hit their own regions only.
    let hotbar8 = creative.hotbar_slot_rect(8, 640, 360).unwrap();
    let trash = creative.trash_slot_rect(640, 360);
    assert!(!hotbar8.intersects(trash), "{hotbar8:?} overlaps {trash:?}");
    assert_eq!(
        creative.hit_test(
            hotbar8.x + hotbar8.w as i32 / 2,
            hotbar8.y + hotbar8.h as i32 / 2,
            640,
            360,
        ),
        Some(CreativeClick::Hotbar(8))
    );
    assert_eq!(
        creative.hit_test(
            trash.x + trash.w as i32 / 2,
            trash.y + trash.h as i32 / 2,
            640,
            360,
        ),
        Some(CreativeClick::Trash)
    );
    assert!(creative.search_box_rect(640, 360).bottom() <= creative.panel_rect(640, 360).bottom());

    // Built-in catalog clicks pick a carried stack first; the following
    // hotbar click assigns player-menu slot 36 + index, never sentinel -1.
    creative.click(CreativeClick::Tab(CreativeTab::BuildingBlocks));
    assert_eq!(creative.click(CreativeClick::Item(0)), None);
    let carried = creative.carried.clone().expect("catalog pick carries a stack");
    assert_eq!(carried.name, "minecraft:stone");
    assert_eq!(
        creative.click(CreativeClick::Hotbar(8)),
        Some(CreativeInventoryAction::SetCreativeSlot {
            slot: 44,
            item_id: 1,
            count: 64,
        })
    );
    assert!(creative.carried.is_none());
    assert_eq!(creative.hotbar[8].as_ref().unwrap().item_id, 1);
}

fn crafter_click(
    state: &mut ContainerScreenState,
    slot: i16,
    button: i8,
) -> Option<WorkstationAction> {
    let layout = state.layout(800.0, 600.0);
    let rect = layout.slots.iter().find(|s| s.index == slot).unwrap().rect;
    state.click_at_with_button(
        rect.x + rect.w as i32 / 2,
        rect.y + rect.h as i32 / 2,
        800.0,
        600.0,
        button,
    )
}

#[test]
fn crafter_toggle_is_left_click_empty_slot_empty_cursor_only() {
    let mut state = ContainerScreenState::new(menu_schema(7).unwrap(), 1, 1, "Crafter");
    assert_eq!(
        crafter_click(&mut state, 4, 0),
        Some(WorkstationAction::ToggleCrafterSlot { slot: 4, disabled: true })
    );
    assert!(state.is_crafter_slot_disabled(4));

    // Carrying an item must fall through to ordinary placement routing: no
    // toggle action and no local crafter-state mutation.
    state.cursor = Some(UiItemStack::new(1, 1));
    assert_eq!(crafter_click(&mut state, 5, 0), None);
    assert!(!state.is_crafter_slot_disabled(5));

    // A non-empty crafter slot likewise falls through for normal pickup.
    state.cursor = None;
    state.set_slot(6, UiItemStack::new(1, 1));
    assert_eq!(crafter_click(&mut state, 6, 0), None);
    assert!(!state.is_crafter_slot_disabled(6));

    // Right click never toggles, even when both slot and cursor are empty.
    assert_eq!(crafter_click(&mut state, 7, 1), None);
    assert!(!state.is_crafter_slot_disabled(7));
}

#[test]
fn loom_and_stonecutter_grid_scroll_helpers_clamp_to_valid_rows() {
    let mut loom = ContainerScreenState::new(menu_schema(18).unwrap(), 1, 1, "Loom");
    loom.loom_patterns = (0..30).map(|i| format!("pattern_{i}")).collect();
    loom.scroll_loom_grid(999);
    assert_eq!(loom.loom_scroll_row, 4, "30 patterns = 8 rows, 4 visible");
    loom.scroll_loom_grid(-999);
    assert_eq!(loom.loom_scroll_row, 0);

    let mut stonecutter =
        ContainerScreenState::new(menu_schema(24).unwrap(), 1, 1, "Stonecutter");
    stonecutter.stonecutter_recipes = (0..20).map(|i| format!("recipe_{i}")).collect();
    stonecutter.scroll_stonecutter_grid(999);
    assert_eq!(stonecutter.stonecutter_scroll_row, 2, "20 recipes = 5 rows, 3 visible");
    stonecutter.scroll_stonecutter_grid(-999);
    assert_eq!(stonecutter.stonecutter_scroll_row, 0);
}

#[test]
fn vanilla_container_skin_is_not_overpainted_by_procedural_slot_chrome() {
    let entry = GuiSpriteEntry {
        uv_min: [0.0, 0.0],
        uv_max: [1.0, 1.0],
        width: 176,
        height: 166,
        nine_slice_border: [0, 0, 0, 0],
    };
    let atlas = GuiSpriteAtlas::from_packed_with_container_panels(
        1,
        1,
        vec![0, 0, 0, 255],
        Vec::new(),
        vec![(14, entry)],
    );
    let screen = ContainerScreenState::new(menu_schema(14).unwrap(), 1, 1, "Furnace");
    let count_slot_wells = |draw: &UiDrawList| {
        draw.commands
            .iter()
            .filter(|cmd| matches!(cmd, UiCommand::FillRect { color, .. } if *color == palette::SLOT))
            .count()
    };

    let mut skinned = UiDrawList::new();
    screen.render_with_atlas(800.0, 600.0, None, &atlas, &mut skinned);
    assert_eq!(count_slot_wells(&skinned), 0, "skin artwork must remain visible");

    let mut fallback = UiDrawList::new();
    screen.render(800.0, 600.0, &mut fallback);
    assert!(count_slot_wells(&fallback) > 0, "vector fallback keeps slot wells");
}

fn click_enchant_row(state: &mut ContainerScreenState, row: i32) -> Option<WorkstationAction> {
    let rect = state
        .layout(800.0, 600.0)
        .buttons
        .into_iter()
        .find(|button| button.id == row)
        .unwrap()
        .rect;
    state.click_at(
        rect.x + rect.w as i32 / 2,
        rect.y + rect.h as i32 / 2,
        800.0,
        600.0,
    )
}

#[test]
fn enchanting_rows_use_one_affordability_predicate_and_resolve_clue_names() {
    let mut state = ContainerScreenState::new(menu_schema(13).unwrap(), 1, 1, "Enchanting");
    for row in 0..3i16 {
        state.set_property(row, (row + 1) * 3);
    }
    state.set_property(4, 7); // depth_strider clue id
    state.set_property(7, 2); // clue level II

    // Once the lapis slot has synchronized, an explicitly empty slot is a
    // real zero count rather than unknown legacy state.
    state.set_slot(1, UiItemStack::empty());
    assert!(!state.enchanting_row_enabled(0));
    assert_eq!(click_enchant_row(&mut state, 0), None);

    state.set_slot(
        1,
        UiItemStack::new(807, 3).with_namespaced_id("minecraft:lapis_lazuli"),
    );
    state.set_player_experience_level(Some(9));
    for row in 0..3 {
        assert!(state.enchanting_row_enabled(row));
    }
    assert_eq!(click_enchant_row(&mut state, 0), Some(WorkstationAction::ClickButton(0)));

    state.set_player_experience_level(Some(3));
    assert!(state.enchanting_row_enabled(0));
    assert!(!state.enchanting_row_enabled(1));
    assert_eq!(click_enchant_row(&mut state, 1), None);

    let mut draw = UiDrawList::new();
    state.render(800.0, 600.0, &mut draw);
    assert!(text_commands(&draw).any(|(_, _, _, text)| text == "Depth Strider II"));
    assert!(!text_commands(&draw).any(|(_, _, _, text)| text.contains("Ench 7")));
}

#[test]
fn beacon_initializes_from_properties_gates_payment_and_cancel_closes() {
    let mut state = ContainerScreenState::new(menu_schema(9).unwrap(), 1, 1, "Beacon");
    state.set_property(0, 4); // full pyramid unlocks every effect
    state.set_property(1, 0); // synchronized active primary: speed
    assert_eq!(state.beacon_primary, Some(0));

    // Explicitly synchronized empty and non-payment items both keep Confirm off.
    state.set_slot(0, UiItemStack::empty());
    assert!(!state.beacon_payment_valid());
    assert!(!state.beacon_confirm_enabled());
    assert_eq!(click_container_pos(&mut state, 173, 79), None);
    state.set_slot(0, UiItemStack::new(807, 1).with_namespaced_id("minecraft:lapis_lazuli"));
    assert!(!state.beacon_payment_valid());
    assert!(!state.beacon_confirm_enabled());

    state.set_slot(0, UiItemStack::new(811, 1).with_namespaced_id("minecraft:iron_ingot"));
    assert!(state.beacon_payment_valid());
    assert!(state.beacon_confirm_enabled());
    assert_eq!(
        click_container_pos(&mut state, 173, 79),
        Some(WorkstationAction::SetBeacon { primary: Some(0), secondary: None })
    );

    assert_eq!(
        click_container_pos(&mut state, 173, 97),
        Some(WorkstationAction::ClickButton(-9)),
        "Cancel uses the backward-compatible close sentinel"
    );
    assert!(state.take_beacon_close_request());
    assert!(!state.take_beacon_close_request());

    state.set_ui_language(UiLang::Ja);
    let mut draw = UiDrawList::new();
    state.render(800.0, 600.0, &mut draw);
    assert!(text_commands(&draw).any(|(_, _, _, text)| text == "決定"));
    assert!(text_commands(&draw).any(|(_, _, _, text)| text == "キャンセル"));
}

#[test]
fn book_editor_separates_edit_signing_and_written_modes_with_real_buttons() {
    let mut book = BookEditorState::new(7);
    book.type_char('A');
    assert_eq!(book.press_enter(), None, "Enter in page body must not save");
    assert_eq!(book.pages[0], "A\n");

    // Every explicit control has a distinct shared draw/hit-test rectangle.
    let mut rects = Vec::new();
    for button in BookEditorButton::ALL {
        let rect = book.button_rect(button, 640, 360);
        for other in &rects {
            assert!(!rect.intersects(*other), "{button:?} rect overlaps");
        }
        rects.push(rect);
    }

    assert_eq!(book.click_button(BookEditorButton::Sign), None);
    assert_eq!(book.mode, BookEditorMode::Signing);
    assert!(book.title_entry);
    assert_eq!(book.click_button(BookEditorButton::Sign), None, "empty title is disabled");
    book.type_char('T');
    let action = book.click_button(BookEditorButton::Sign);
    assert!(matches!(
        action,
        Some(BookEditorAction::Save { slot: 7, title: Some(title), .. }) if title == "T"
    ));
    assert_eq!(book.mode, BookEditorMode::Edit);
    assert!(!book.title_entry);

    let mut written = BookEditorState::new_written(3, vec!["正文".to_owned()], "Title", "Author");
    assert_eq!(written.mode, BookEditorMode::Written);
    written.type_char('X');
    assert_eq!(written.pages[0], "正文", "written books are read-only");
    assert_eq!(written.click_button(BookEditorButton::Done), Some(BookEditorAction::Close));
}

#[test]
fn end_credits_center_cjk_by_glyph_width_not_utf8_byte_len() {
    let mut credits = EndCreditsState {
        lines: vec!["aa".to_owned(), "日日".to_owned()],
        scroll: 100.0,
        speed: 12.0,
    };
    credits.speed = 0.0;
    let mut draw = UiDrawList::new();
    credits.render(200, 200, &mut draw);
    let texts: Vec<_> = text_commands(&draw).collect();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[0].0, 100 - text_width("aa", 1) as i32 / 2);
    assert_eq!(texts[1].0, 100 - text_width("日日", 1) as i32 / 2);
    assert_eq!(texts[1].0, 88, "two full-width glyphs are 24 logical px wide, not 6 UTF-8 bytes");
}

#[test]
fn mod_sync_digest_truncation_is_char_boundary_safe_and_validated() {
    let mut entry = ModSyncDiffEntry::new("cjk", "CJK Mod", "1.0", ModSyncDiffKind::Missing);
    entry.sha512_hex = Some("日本語日本語日本語".to_owned());
    assert_eq!(entry.short_sha512().unwrap(), "日本語日本語日本語");
    assert_eq!(entry.short_sha512_display().unwrap(), "日本語日本語日本語");
    assert!(!entry.sha512_hex_is_valid());

    entry.sha512_hex = Some("abcdef1234567".to_owned());
    assert_eq!(entry.short_sha512().unwrap(), "abcdef123456");
    assert!(entry.sha512_hex_is_valid());

    entry.sha512_hex = Some("日本語日本語日本語".to_owned());
    let mut ui = ModSyncUiState::new("server", "server-id", vec![entry]);
    ui.activate(ModSyncWidgetId::Row(0));
    let (draw, _) = ui.render(800, 600);
    assert!(text_commands(&draw).any(|(_, _, _, text)| text.contains("invalid fingerprint")));
}

#[test]
fn container_tooltip_wraps_cjk_rows_and_clamps_inside_framebuffer() {
    let mut state = ContainerScreenState::new(menu_schema(0).unwrap(), 1, 1, "Chest");
    let long_lore = "日本語日本語日本語日本語日本語";
    let item = UiItemStack::new(1, 1)
        .with_namespaced_id("minecraft:stone")
        .with_name("日本語アイテム")
        .with_lore((0..30).map(|_| long_lore.to_owned()));
    state.set_slot(0, item);
    state.hover(Some(0));

    let layout = state.layout(800.0, 180.0);
    let tooltip = layout.tooltip.expect("hovered item emits a tooltip");
    assert!(tooltip.x >= 0 && tooltip.y >= 0);
    assert!(tooltip.right() <= 800 && tooltip.bottom() <= 180);
    assert!(tooltip.w <= 220, "tooltip width is budgeted: {tooltip:?}");

    let mut draw = UiDrawList::new();
    state.render(800.0, 180.0, &mut draw);
    let lore_rows: Vec<_> = text_commands(&draw)
        .filter(|(x, y, scale, text)| {
            *scale == 1
                && text.chars().any(|ch| matches!(ch, '日' | '本' | '語'))
                && *x >= tooltip.x
                && *y >= tooltip.y
        })
        .collect();
    assert!(lore_rows.len() >= 2, "long CJK lore wraps into multiple rows");
    assert!(lore_rows.len() <= 17, "viewport row budget is enforced");
    for (x, y, scale, text) in lore_rows {
        assert!(x + text_width(text, scale) as i32 <= tooltip.right() - 2);
        assert!(y + 8 * scale as i32 <= tooltip.bottom());
    }
}

#[test]
fn overlay_and_vanilla_tooltips_wrap_cjk_and_stay_in_bounds() {
    let mut item = OverlayItem::new("minecraft:stone", "Stone", "Minecraft");
    item.info_lines.push("日本語の非常に長い説明文でツールチップを折り返します".to_owned());
    let mut draw = UiDrawList::new();
    let rect = render_item_tooltip(&mut draw, 180, 100, (170, 90), &item, true, 2);
    assert!(rect.x >= 0 && rect.y >= 0);
    assert!(rect.right() <= 180 && rect.bottom() <= 100);
    let rows = text_commands(&draw)
        .filter(|(x, y, _, _)| *x >= rect.x && *x < rect.right() && *y >= rect.y && *y < rect.bottom())
        .count();
    assert!(rows >= 2);
    for (x, y, scale, text) in text_commands(&draw) {
        if x >= rect.x && y >= rect.y {
            assert!(x + text_width(text, scale) as i32 <= rect.right());
            assert!(y + 8 * scale as i32 <= rect.bottom());
        }
    }

    let wrapped = wrap_text_px("日本語日本語", 32);
    assert_eq!(wrapped, vec!["日本", "語日", "本語"]);
    assert!(wrapped.iter().all(|line| text_width(line, 1) <= 32));
    assert_eq!(tooltip_row_budget(36, 3, 10), 3);
}

#[test]
fn magic_hud_has_japanese_localization_hooks() {
    let state = MagicHudState {
        mana: 30.0,
        max_mana: 100.0,
        active_cast: Some(CastBarState {
            spell_label: "Fireball".to_owned(),
            progress: 0.5,
            channeled: false,
            interrupted_flash_ticks: 0,
        }),
        ..Default::default()
    };
    let draw = build_magic_hud_draw_list_localized(&state, 640, 360, UiLang::Ja);
    assert!(text_commands(&draw).any(|(_, _, _, text)| text.contains("マナ")));
    assert!(text_commands(&draw).any(|(_, _, _, text)| text.contains("詠唱中")));
}
