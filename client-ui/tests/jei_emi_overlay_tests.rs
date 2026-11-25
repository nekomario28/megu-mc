// SPDX-License-Identifier: LGPL-3.0-or-later
//! Behavior tests for the JEI/EMI native side-by-side container overlay:
//! non-overlapping 3-zone geometry from `640×360` to `3840×2160`, exact
//! 18×18 bevel-slot layout + pagination, `R`/`U`/`A`/`Ctrl+O`/`Escape`/
//! wheel/double-click state transitions, `+`-hover missing-slot red
//! overlay, purple tooltip clamping + blue mod line, and parallel
//! `ItemSpriteSlot` emission matching every visible grid slot.

use megu_client_ui::draw::{UiColor, UiCommand, UiRect};
use megu_client_ui::jei_emi_overlay::{
    render_item_tooltip, ItemSpriteSlot, JeiEmiGeometry, JeiEmiOverlayState, LeftPanelMode,
    OverlayItem, OverlayKey, OverlayRecipe, OverlaySlot, OverlayWidgetId, PopupMode, PopupTab,
    RecipePopup, BEVEL_DARK, BEVEL_LIGHT, CRAFTABLE_GREEN, HOVER_HIGHLIGHT, ID_GREY,
    MISSING_TINT, MOD_NAME_BLUE, SEARCH_YELLOW, SLOT_INNER, TOOLTIP_EDGE_BOTTOM,
    TOOLTIP_EDGE_TOP,
};

const SIZES: [(u32, u32); 4] = [(640, 360), (1280, 720), (1920, 1080), (3840, 2160)];
const SCALES: [u32; 5] = [0, 1, 2, 3, 4];

/// Standard vanilla 176×166 container centered in the viewport.
fn container(w: u32, h: u32) -> UiRect {
    UiRect::new((w as i32 - 176) / 2, (h as i32 - 166) / 2, 176, 166)
}

fn item(id: &str, name: &str, mod_name: &str) -> OverlayItem {
    OverlayItem::new(id, name, mod_name)
}

/// Full fixture: catalog + crafting/smelting recipes.
fn fixture() -> JeiEmiOverlayState {
    let mut s = JeiEmiOverlayState::new();
    let mut items = vec![
        item("minecraft:iron_block", "Iron Block", "Minecraft"),
        item("minecraft:iron_ingot", "Iron Ingot", "Minecraft"),
        item("minecraft:iron_ore", "Iron Ore", "Minecraft"),
        item("minecraft:coal", "Coal", "Minecraft"),
        item("create:cogwheel", "Cogwheel", "Create"),
    ];
    // Pad the catalog so pagination kicks in even on 1080p grids.
    for i in 0..600 {
        items.push(item(
            &format!("minecraft:pad_{i:03}"),
            &format!("Pad Item {i}"),
            "Minecraft",
        ));
    }
    s.items = items;
    // 9x iron_ingot -> iron_block (shaped 3x3).
    let mut to_block = OverlayRecipe::new(
        "minecraft:iron_block",
        PopupTab::Crafting,
        OverlaySlot::new("minecraft:iron_block", 1),
    );
    to_block.inputs = (0..9)
        .map(|_| Some(OverlaySlot::new("minecraft:iron_ingot", 1)))
        .collect();
    s.recipes.push(to_block);
    // A second crafting recipe for the same output (pager coverage).
    let mut alt = OverlayRecipe::new(
        "test:iron_block_alt",
        PopupTab::Crafting,
        OverlaySlot::new("minecraft:iron_block", 1),
    );
    alt.inputs = vec![
        Some(OverlaySlot::new("minecraft:iron_ingot", 4)),
        Some(OverlaySlot::new("minecraft:coal", 2)),
        None,
    ];
    s.recipes.push(alt);
    // ore + coal -> ingot (smelting card: flame + XP + cook time).
    let mut smelt = OverlayRecipe::new(
        "minecraft:smelt_iron",
        PopupTab::Smelting,
        OverlaySlot::new("minecraft:iron_ingot", 1),
    );
    smelt.inputs = vec![
        Some(OverlaySlot::new("minecraft:iron_ore", 1)),
        Some(OverlaySlot::new("minecraft:coal", 1)),
    ];
    smelt.xp = 0.7;
    smelt.cook_ticks = 200;
    s.recipes.push(smelt);
    s
}

/// Find the physical-px center of a hit region for `wid`.
fn hit_center(hits: &[megu_client_ui::jei_emi_overlay::OverlayHitRegion], wid: OverlayWidgetId) -> Option<(i32, i32)> {
    hits.iter()
        .find(|h| h.id == wid)
        .map(|h| (h.rect.x + h.rect.w as i32 / 2, h.rect.y + h.rect.h as i32 / 2))
}

/// Hover the first right-grid item slot and return its filtered index.
fn hover_first_slot(s: &mut JeiEmiOverlayState, w: u32, h: u32) {
    let frame = s.render(w, h, container(w, h), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::ItemSlot(0))
        .expect("no ItemSlot(0) region");
    s.handle_mouse_move(cx, cy);
}

fn count_cmds(list: &megu_client_ui::draw::UiDrawList, pred: impl Fn(&UiCommand) -> bool) -> usize {
    list.commands.iter().filter(|c| pred(c)).count()
}

// ---------------------------------------------------------------------------
// 3-zone non-overlap
// ---------------------------------------------------------------------------

#[test]
fn sidebars_never_intersect_container_across_sizes_and_scales() {
    for (w, h) in SIZES {
        for s in SCALES {
            let c = container(w, h);
            let geo = JeiEmiGeometry::compute(w, h, c, s);
            if let Some(r) = geo.right {
                let p = geo.phys(r);
                assert!(
                    !p.intersects(c),
                    "right sidebar {p:?} intersects container {c:?} at {w}x{h} scale {s}"
                );
                assert!(p.x >= 0 && p.right() <= w as i32, "right sidebar off viewport: {p:?}");
                assert!(p.y >= 0 && p.bottom() <= h as i32, "right sidebar off viewport: {p:?}");
            }
            if let Some(l) = geo.left {
                let p = geo.phys(l);
                assert!(
                    !p.intersects(c),
                    "left sidebar {p:?} intersects container {c:?} at {w}x{h} scale {s}"
                );
                assert!(p.x >= 0 && p.right() <= w as i32, "left sidebar off viewport: {p:?}");
            }
            // Grid + bars stay inside the right sidebar.
            if let Some(r) = geo.right {
                for zone in [
                    geo.grid,
                    geo.page_prev,
                    geo.page_next,
                    geo.page_label,
                    geo.search_box,
                    geo.filter_button,
                ] {
                    assert!(
                        zone.x >= r.x
                            && zone.right() <= r.right()
                            && zone.y >= r.y
                            && zone.bottom() <= r.bottom(),
                        "zone {zone:?} escapes sidebar {r:?} at {w}x{h} scale {s}"
                    );
                }
            }
        }
    }
}

#[test]
fn sidebars_survive_odd_containers_without_overlap() {
    // Offset / oversized containers must never produce an overlap —
    // sidebars just hide when there is no room.
    let cases = [
        UiRect::new(0, 0, 176, 166),          // hard left
        UiRect::new(464, 0, 176, 166),        // hard right @640
        UiRect::new(0, 0, 640, 360),          // fullscreen
        UiRect::new(10, 10, 60, 60),          // tiny, way left
        UiRect::new(500, 250, 138, 108),      // bottom-right
    ];
    for (w, h) in SIZES {
        for c in cases {
            if c.right() > w as i32 || c.bottom() > h as i32 {
                continue;
            }
            let geo = JeiEmiGeometry::compute(w, h, c, 1);
            if let Some(r) = geo.right {
                assert!(!geo.phys(r).intersects(c), "right overlap at {w}x{h} c={c:?}");
            }
            if let Some(l) = geo.left {
                assert!(!geo.phys(l).intersects(c), "left overlap at {w}x{h} c={c:?}");
            }
        }
    }
}

#[test]
fn sidebar_hit_regions_never_intersect_container() {
    let mut s = fixture();
    for (w, h) in SIZES {
        let c = container(w, h);
        let frame = s.render(w, h, c, 0);
        for hit in &frame.hits {
            assert!(
                !hit.rect.intersects(c),
                "hit {hit:?} intersects container {c:?} at {w}x{h}"
            );
            assert!(
                hit.rect.x >= 0
                    && hit.rect.y >= 0
                    && hit.rect.right() <= w as i32
                    && hit.rect.bottom() <= h as i32,
                "hit {hit:?} outside viewport {w}x{h}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 18×18 slot grid + pagination
// ---------------------------------------------------------------------------

#[test]
fn grid_slots_are_exact_18x18_at_scale_1() {
    let mut s = fixture();
    let (w, h) = (1920, 1080);
    let frame = s.render(w, h, container(w, h), 1);
    let slots: Vec<UiRect> = frame
        .hits
        .iter()
        .filter(|h| matches!(h.id, OverlayWidgetId::ItemSlot(_)))
        .map(|h| h.rect)
        .collect();
    assert!(!slots.is_empty(), "no item slots emitted");
    for r in &slots {
        assert_eq!((r.w, r.h), (18, 18), "slot rect {r:?} not 18x18");
    }
    // Row-major: consecutive slots on one row step exactly 18px.
    let first = slots[0];
    let second = slots[1];
    assert_eq!(second.x - first.x, 18, "slot pitch must be 18px");
    assert_eq!(second.y, first.y);
}

#[test]
fn grid_slots_scale_with_gui_scale() {
    let mut s = fixture();
    let (w, h) = (1920, 1080);
    for scale in [2u32, 3, 4] {
        let frame = s.render(w, h, container(w, h), scale);
        let slot = frame
            .hits
            .iter()
            .find(|h| h.id == OverlayWidgetId::ItemSlot(0))
            .expect("no slot")
            .rect;
        assert_eq!(
            (slot.w, slot.h),
            (18 * scale, 18 * scale),
            "slot not 18*scale at gui_scale {scale}"
        );
    }
}

#[test]
fn pagination_math_and_pager_buttons() {
    let mut s = fixture();
    let (w, h) = (1920, 1080);
    let geo = JeiEmiGeometry::compute(w, h, container(w, h), 1);
    let per = geo.cols * geo.rows;
    assert!(per >= 9, "grid too small for fixture: {per}");
    let total = s.total_pages(&geo);
    assert_eq!(total, s.filtered_items().len().div_ceil(per));
    assert!(total > 1, "fixture must exceed one page ({total})");

    // Render caches the geometry, then wheel/pager turns the page.
    let frame = s.render(w, h, container(w, h), 1);
    assert_eq!(s.page, 0);
    s.handle_key(OverlayKey::WheelDown);
    assert_eq!(s.page, 1);
    s.handle_key(OverlayKey::WheelUp);
    assert_eq!(s.page, 0);
    // `>` / `<` hit regions drive the same transition.
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::PageNext).unwrap();
    s.handle_click(cx, cy, 0);
    assert_eq!(s.page, 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::PagePrev).unwrap();
    s.handle_click(cx, cy, 0);
    assert_eq!(s.page, 0);
    // Page clamps at the last page.
    s.page = total - 1;
    s.handle_key(OverlayKey::WheelDown);
    assert_eq!(s.page, total - 1);
    // The page counter text is emitted centered in the top bar.
    let frame = s.render(w, h, container(w, h), 1);
    let label = format!("{total} / {total}");
    assert!(
        frame
            .list
            .commands
            .iter()
            .any(|c| matches!(c, UiCommand::Text { text, .. } if text == &label)),
        "pager text '{label}' missing"
    );
}

#[test]
fn sprite_slots_match_every_visible_grid_slot() {
    let mut s = fixture();
    let (w, h) = (1920, 1080);
    let frame = s.render(w, h, container(w, h), 1);
    let geo = JeiEmiGeometry::compute(w, h, container(w, h), 1);
    let per = geo.cols * geo.rows;
    let expected = s.filtered_items().len().min(per);
    let grid_sprites: Vec<&ItemSpriteSlot> = frame
        .sprites
        .iter()
        .filter(|sp| sp.rect.w == 16 && sp.rect.h == 16)
        .collect();
    assert_eq!(
        grid_sprites.len(),
        expected,
        "sprite count must equal visible grid slots"
    );
    // Every sprite sits inside its corresponding 18x18 slot region.
    let slot_hits: Vec<UiRect> = frame
        .hits
        .iter()
        .filter(|h| matches!(h.id, OverlayWidgetId::ItemSlot(_)))
        .map(|h| h.rect)
        .collect();
    for (i, sp) in grid_sprites.iter().enumerate() {
        let slot = slot_hits[i];
        assert!(
            sp.rect.x > slot.x
                && sp.rect.y > slot.y
                && sp.rect.right() < slot.right()
                && sp.rect.bottom() < slot.bottom(),
            "sprite {:?} not inside slot {slot:?}",
            sp.rect
        );
    }
}

// ---------------------------------------------------------------------------
// Input handling: R / U / A / Ctrl+O / Escape / wheel / double-click
// ---------------------------------------------------------------------------

#[test]
fn key_r_opens_recipes_popup_for_hovered_item() {
    let mut s = fixture();
    hover_first_slot(&mut s, 1920, 1080);
    assert_eq!(s.hovered, Some(OverlayWidgetId::ItemSlot(0)));
    s.handle_key(OverlayKey::Char('r'));
    let pop = s.popup.clone().expect("R must open the popup");
    assert_eq!(pop.item_id, "minecraft:iron_block");
    assert_eq!(pop.mode, PopupMode::Recipes);
    assert_eq!(pop.tab, PopupTab::Crafting);
}

#[test]
fn key_u_opens_uses_popup_for_hovered_item() {
    let mut s = fixture();
    // iron_ingot is filtered index 1 — hover that slot.
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::ItemSlot(1)).unwrap();
    s.handle_mouse_move(cx, cy);
    s.handle_key(OverlayKey::Char('u'));
    let pop = s.popup.clone().expect("U must open the popup");
    assert_eq!(pop.item_id, "minecraft:iron_ingot");
    assert_eq!(pop.mode, PopupMode::Uses);
    // Uses of iron_ingot = the two iron_block crafting recipes.
    assert_eq!(pop.tab, PopupTab::Crafting);
}

#[test]
fn key_a_toggles_bookmark_for_hovered_item() {
    let mut s = fixture();
    hover_first_slot(&mut s, 1920, 1080);
    s.handle_key(OverlayKey::Char('a'));
    assert_eq!(s.bookmarks, vec!["minecraft:iron_block".to_string()]);
    s.handle_key(OverlayKey::Char('a'));
    assert!(s.bookmarks.is_empty());
}

#[test]
fn ctrl_o_toggles_overlay_visibility() {
    let mut s = fixture();
    assert!(s.visible);
    s.handle_key(OverlayKey::CtrlChar('o'));
    assert!(!s.visible);
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    assert!(frame.hits.is_empty() && frame.list.is_empty());
    s.handle_key(OverlayKey::CtrlChar('O'));
    assert!(s.visible);
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    assert!(!frame.hits.is_empty());
}

#[test]
fn escape_closes_popup_returning_to_container() {
    let mut s = fixture();
    hover_first_slot(&mut s, 1920, 1080);
    s.handle_key(OverlayKey::Char('r'));
    assert!(s.popup.is_some());
    s.handle_key(OverlayKey::Escape);
    assert!(s.popup.is_none(), "Escape must close the popup");
    // A second Escape (nothing open) leaves the overlay up.
    s.handle_key(OverlayKey::Escape);
    assert!(s.visible);
}

#[test]
fn double_click_search_toggles_yellow_highlight_mode() {
    let mut s = fixture();
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::SearchBox).unwrap();
    s.handle_click(cx, cy, 1000);
    assert!(s.search_focused);
    assert!(!s.highlight_search);
    s.handle_click(cx, cy, 1200); // within 400ms => double click
    assert!(s.highlight_search, "double-click must arm highlight mode");
    // With a search term, matches surface for the host to tint.
    s.search = "iron".to_string();
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    assert!(!frame.search_matches.is_empty());
    assert!(frame
        .search_matches
        .iter()
        .any(|id| id == "minecraft:iron_block"));
    // The box itself draws the yellow border.
    assert!(
        count_cmds(&frame.list, |c| matches!(
            c,
            UiCommand::BorderRect { color, .. } if *color == SEARCH_YELLOW
        )) > 0,
        "yellow search border missing"
    );
}

#[test]
fn single_click_or_slow_click_does_not_toggle_highlight() {
    let mut s = fixture();
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::SearchBox).unwrap();
    s.handle_click(cx, cy, 0);
    s.handle_click(cx, cy, 900); // >400ms: not a double click
    assert!(!s.highlight_search);
}

#[test]
fn search_field_focus_eats_rua_keystrokes() {
    let mut s = fixture();
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::SearchBox).unwrap();
    s.handle_click(cx, cy, 0);
    assert!(s.search_focused);
    s.handle_key(OverlayKey::Char('r'));
    s.handle_key(OverlayKey::Char('u'));
    assert_eq!(s.search, "ru", "focused search must eat r/u keys");
    assert!(s.popup.is_none());
    s.handle_key(OverlayKey::Backspace);
    assert_eq!(s.search, "r");
}

#[test]
fn item_click_opens_recipes_like_jei() {
    let mut s = fixture();
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::ItemSlot(0)).unwrap();
    s.handle_click(cx, cy, 0);
    let pop = s.popup.clone().expect("click must open recipes");
    assert_eq!(pop.item_id, "minecraft:iron_block");
    assert_eq!(pop.mode, PopupMode::Recipes);
}

#[test]
fn filter_toggle_shows_only_craftable_items() {
    let mut s = fixture();
    // Give the player the 9 ingots the iron_block recipe needs.
    s.inventory.insert("minecraft:iron_ingot".to_string(), 9);
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::FilterToggle).unwrap();
    s.handle_click(cx, cy, 0);
    assert!(s.craftable_only);
    let filtered: Vec<String> = s
        .filtered_items()
        .iter()
        .map(|&i| s.items[i].id.clone())
        .collect();
    assert_eq!(filtered, vec!["minecraft:iron_block".to_string()]);
}

// ---------------------------------------------------------------------------
// Recipe popup card
// ---------------------------------------------------------------------------

fn open_iron_block_popup(s: &mut JeiEmiOverlayState) {
    s.popup = Some(RecipePopup {
        item_id: "minecraft:iron_block".to_string(),
        mode: PopupMode::Recipes,
        tab: PopupTab::Crafting,
        page: 0,
    });
}

#[test]
fn popup_renders_vanilla_card_with_tabs_pager_and_tree_button() {
    let mut s = fixture();
    open_iron_block_popup(&mut s);
    let geo = JeiEmiGeometry::compute(1920, 1080, container(1920, 1080), 1);
    let (list, hits, _sprites) = s.render_recipe_popup(&geo);
    // Vanilla light-grey panel fill.
    assert!(
        count_cmds(&list, |c| matches!(
            c,
            UiCommand::FillRect { color, .. } if *color == SLOT_INNER || *color == UiColor::rgb(0xC6, 0xC6, 0xC6)
        )) > 0
    );
    // Ten category tabs + pager + tree button + slots all hittable.
    for i in 0..10 {
        assert!(
            hits.iter().any(|h| h.id == OverlayWidgetId::PopupTab(i)),
            "missing PopupTab({i})"
        );
    }
    for id in [
        OverlayWidgetId::PopupPrev,
        OverlayWidgetId::PopupNext,
        OverlayWidgetId::PopupTransfer,
        OverlayWidgetId::PopupTree,
        OverlayWidgetId::PopupOutput,
    ] {
        assert!(hits.iter().any(|h| h.id == id), "missing {id:?}");
    }
    // "1 / 2" pager (two crafting recipes produce iron_block).
    assert!(
        list.commands.iter().any(|c| matches!(
            c,
            UiCommand::Text { text, .. } if text == "1 / 2"
        )),
        "recipe pager text missing"
    );
}

#[test]
fn popup_pager_and_tab_switching() {
    let mut s = fixture();
    open_iron_block_popup(&mut s);
    // Full render caches popup hit regions for handle_click.
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let (cx, cy) = hit_center(&frame.hits, OverlayWidgetId::PopupNext).unwrap();
    s.handle_click(cx, cy, 0);
    assert_eq!(s.popup.as_ref().unwrap().page, 1);
    s.handle_click(cx, cy, 0); // clamps at total-1
    assert_eq!(s.popup.as_ref().unwrap().page, 1);
    s.activate(OverlayWidgetId::PopupTab(1), 0);
    let pop = s.popup.as_ref().unwrap();
    assert_eq!(pop.tab, PopupTab::Smelting);
    assert_eq!(pop.page, 0);
}

#[test]
fn plus_hover_red_tints_missing_slots() {
    let mut s = fixture();
    // Player has zero iron ingots -> all 9 grid cells missing.
    open_iron_block_popup(&mut s);
    s.hovered = Some(OverlayWidgetId::PopupTransfer);
    let geo = JeiEmiGeometry::compute(1920, 1080, container(1920, 1080), 1);
    let (list, _hits, sprites) = s.render_recipe_popup(&geo);
    let tinted = sprites
        .iter()
        .filter(|sp| sp.missing_overlay)
        .count();
    assert_eq!(tinted, 9, "all 9 missing ingredient slots must tint red");
    assert!(
        count_cmds(&list, |c| matches!(
            c,
            UiCommand::FillRect { color, .. } if *color == MISSING_TINT
        )) >= 9,
        "missing-slot red overlay missing"
    );
}

#[test]
fn plus_button_greens_when_craftable() {
    let mut s = fixture();
    s.inventory.insert("minecraft:iron_ingot".to_string(), 9);
    open_iron_block_popup(&mut s);
    s.hovered = Some(OverlayWidgetId::PopupTransfer);
    let geo = JeiEmiGeometry::compute(1920, 1080, container(1920, 1080), 1);
    let (list, _hits, sprites) = s.render_recipe_popup(&geo);
    assert!(
        count_cmds(&list, |c| matches!(
            c,
            UiCommand::FillRect { color, .. } if *color == CRAFTABLE_GREEN
        )) > 0,
        "craftable `+` must be green"
    );
    assert!(
        sprites.iter().all(|sp| !sp.missing_overlay),
        "no missing tint when craftable"
    );
}

#[test]
fn plus_click_emits_transfer_action() {
    let mut s = fixture();
    s.inventory.insert("minecraft:iron_ingot".to_string(), 9);
    open_iron_block_popup(&mut s);
    let actions = s.activate(OverlayWidgetId::PopupTransfer, 0);
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        megu_client_ui::jei_emi_overlay::OverlayAction::TransferRecipe {
            recipe_id,
            craftable,
        } => {
            assert_eq!(recipe_id, "minecraft:iron_block");
            assert!(craftable);
        }
    }
}

#[test]
fn furnace_card_has_input_flame_fuel_output_and_xp_readout() {
    let mut s = fixture();
    s.popup = Some(RecipePopup {
        item_id: "minecraft:iron_ingot".to_string(),
        mode: PopupMode::Recipes,
        tab: PopupTab::Smelting,
        page: 0,
    });
    let geo = JeiEmiGeometry::compute(1920, 1080, container(1920, 1080), 1);
    let (list, _hits, sprites) = s.render_recipe_popup(&geo);
    // input + fuel + output sprites.
    assert_eq!(sprites.len(), 3, "furnace card must emit 3 sprites");
    // XP + cook-time text.
    assert!(
        list.commands.iter().any(|c| matches!(
            c,
            UiCommand::Text { text, .. } if text.contains("XP") && text.contains("200")
        )),
        "smelting XP/ticks readout missing"
    );
    // Flame icon rasterized (orange pixels present).
    assert!(
        count_cmds(&list, |c| matches!(
            c,
            UiCommand::FillRect { color, .. } if *color == UiColor::rgb(0xEE, 0x88, 0x22)
        )) > 0,
        "flame icon missing"
    );
}

#[test]
fn view_recipe_tree_pins_emi_checklist_and_switches_left_mode() {
    let mut s = fixture();
    s.inventory.insert("minecraft:iron_ingot".to_string(), 5);
    open_iron_block_popup(&mut s);
    s.activate(OverlayWidgetId::PopupTree, 0);
    assert_eq!(s.left_mode, LeftPanelMode::CraftTree);
    let tree = s.pinned_tree.as_ref().expect("tree must pin");
    assert_eq!(tree.target_id, "minecraft:iron_block");
    assert_eq!(tree.materials.len(), 1);
    assert_eq!(tree.materials[0].item_id, "minecraft:iron_ingot");
    assert_eq!(tree.materials[0].need, 9);
    // Render shows the live have/need checklist.
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    assert!(
        frame.list.commands.iter().any(|c| matches!(
            c,
            UiCommand::Text { text, .. } if text.contains("5/9")
        )),
        "craft-tree have/need checklist missing"
    );
}

// ---------------------------------------------------------------------------
// Purple tooltip
// ---------------------------------------------------------------------------

#[test]
fn tooltip_clamps_inside_viewport() {
    let mut it = item("minecraft:diamond_pickaxe", "Diamond Pickaxe", "Minecraft");
    it.tags = vec!["minecraft:pickaxes".into()];
    let mut list = megu_client_ui::draw::UiDrawList::new();
    for (mx, my) in [(600i32, 300i32), (0, 0), (620, 350), (300, 180)] {
        let r = render_item_tooltip(&mut list, 640, 360, (mx, my), &it, false, 1);
        assert!(
            r.x >= 0 && r.y >= 0 && r.right() <= 640 && r.bottom() <= 360,
            "tooltip {r:?} escapes viewport for mouse ({mx},{my})"
        );
    }
}

#[test]
fn tooltip_has_purple_gradient_border_and_blue_mod_line() {
    let mut it = item("minecraft:diamond_pickaxe", "Diamond Pickaxe", "Minecraft");
    it.info_lines = vec!["Attack +5".into()];
    let mut list = megu_client_ui::draw::UiDrawList::new();
    render_item_tooltip(&mut list, 640, 360, (100, 100), &it, false, 1);
    // Gradient edges present.
    assert!(count_cmds(&list, |c| matches!(
        c,
        UiCommand::FillRect { color, .. } if *color == TOOLTIP_EDGE_TOP
    )) > 0);
    assert!(count_cmds(&list, |c| matches!(
        c,
        UiCommand::FillRect { color, .. } if *color == TOOLTIP_EDGE_BOTTOM
    )) > 0);
    // Line order: name(white) -> info(dim) -> mod (blue, last).
    let texts: Vec<&UiCommand> = list
        .commands
        .iter()
        .filter(|c| matches!(c, UiCommand::Text { .. }))
        .collect();
    assert_eq!(texts.len(), 3);
    match texts[2] {
        UiCommand::Text { text, color, .. } => {
            assert_eq!(text, "Minecraft");
            assert_eq!(*color, MOD_NAME_BLUE);
        }
        _ => unreachable!(),
    }
}

#[test]
fn tooltip_advanced_mode_shows_grey_namespaced_id() {
    let it = item("minecraft:diamond_pickaxe", "Diamond Pickaxe", "Minecraft");
    let mut list = megu_client_ui::draw::UiDrawList::new();
    render_item_tooltip(&mut list, 640, 360, (100, 100), &it, true, 1);
    let texts: Vec<&UiCommand> = list
        .commands
        .iter()
        .filter(|c| matches!(c, UiCommand::Text { .. }))
        .collect();
    // name -> id(grey) -> mod(blue)
    assert_eq!(texts.len(), 3);
    match texts[1] {
        UiCommand::Text { text, color, .. } => {
            assert_eq!(text, "minecraft:diamond_pickaxe");
            assert_eq!(*color, ID_GREY);
        }
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Full-frame sanity
// ---------------------------------------------------------------------------

#[test]
fn bevel_slot_palette_present_in_draw_list() {
    let mut s = fixture();
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    for color in [BEVEL_DARK, BEVEL_LIGHT, SLOT_INNER] {
        assert!(
            count_cmds(&frame.list, |c| matches!(
                c,
                UiCommand::FillRect { color: cc, .. } if *cc == color
            )) > 0,
            "bevel color {color:?} absent from frame"
        );
    }
}

#[test]
fn hover_paints_white_translucent_highlight() {
    let mut s = fixture();
    hover_first_slot(&mut s, 1920, 1080);
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    assert!(
        count_cmds(&frame.list, |c| matches!(
            c,
            UiCommand::FillRect { color, .. } if *color == HOVER_HIGHLIGHT
        )) > 0,
        "hover highlight missing"
    );
}

#[test]
fn bookmarks_grid_slots_after_pinning() {
    let mut s = fixture();
    s.bookmarks = vec!["test:pin_a".into(), "test:pin_b".into()];
    let frame = s.render(1920, 1080, container(1920, 1080), 1);
    let bm: Vec<UiRect> = frame
        .hits
        .iter()
        .filter(|h| matches!(h.id, OverlayWidgetId::BookmarkSlot(_)))
        .map(|h| h.rect)
        .collect();
    assert_eq!(bm.len(), 2);
    for r in &bm {
        assert_eq!((r.w, r.h), (18, 18));
    }
    // Bookmark slots get sprite bindings too (ids not in the grid
    // catalog, so the count is unambiguous).
    let bm_sprites = frame
        .sprites
        .iter()
        .filter(|sp| sp.item_id == "test:pin_a")
        .count();
    assert_eq!(bm_sprites, 1);
}

#[test]
fn render_all_sizes_no_panic_and_sprites_bounded() {
    let mut s = fixture();
    for (w, h) in SIZES {
        for scale in SCALES {
            let frame = s.render(w, h, container(w, h), scale);
            // Sprites only ever bind inside the viewport.
            for sp in &frame.sprites {
                assert!(
                    sp.rect.x >= 0
                        && sp.rect.y >= 0
                        && sp.rect.right() <= w as i32
                        && sp.rect.bottom() <= h as i32,
                    "sprite {sp:?} outside viewport {w}x{h} scale {scale}"
                );
            }
        }
    }
}
