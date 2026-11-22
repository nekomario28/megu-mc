// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for the ModMenu-style Mods & Packs screen: forgiving
//! multi-token search with `@author`/`#tag` filters, filter pills, sort
//! modes, staged enable/disable with `ApplyChanges` restart semantics,
//! Modrinth missing-dependency and link actions, keyboard/gamepad routing,
//! and a layout that must never overlap hit regions or rasterize blank at
//! `640×360` through `3840×2160` in English and Japanese.

use mc_spine_client_ui::draw::{rasterize_draw_list, UiCommand, UiRect};
use mc_spine_client_ui::mod_menu::{
    ModAdaptationBadge, ModLoaderTag, ModMenuAction, ModMenuEntry, ModMenuFilter, ModMenuHitRegion,
    ModMenuSort, ModMenuState, ModMenuWidgetId, ModReloadBadge,
};
use mc_spine_client_ui::state::{GamepadButton, UiInputEvent, UiKey, UiLang};

const SIZES: [(u32, u32); 4] = [(640, 360), (1280, 720), (1920, 1080), (3840, 2160)];

fn mk(id: &str, name: &str, version: &str) -> ModMenuEntry {
    ModMenuEntry::new(id, name, version)
}

/// A varied mod list covering every tier, loader, reload mode, link
/// presence, library flag, and dependency state the UI distinguishes.
///
/// With `show_libraries` off and the default A-Z sort the visible order is:
/// brokenmod, create, create_aero, minimap, jei, nolink, oldmod
/// (megu_lib hidden).
fn fixture() -> ModMenuState {
    let mut create = mk("create", "Create", "0.5.1f");
    create.summary = "Rotation, contraptions and kinetic logistics.".into();
    create.authors = vec!["simibubi".into(), "Fozzie".into()];
    create.license = "MIT".into();
    create.loader = ModLoaderTag::Fabric;
    create.has_config = true;
    create.website_url = "https://example.com/create".into();
    create.issues_url = "https://example.com/create/issues".into();
    create.source_url = "https://example.com/create/src".into();
    create.modrinth_url = "https://modrinth.example/create".into();

    let mut aero = mk("create_aero", "Create Aeronautics", "1.2.0");
    aero.authors = vec!["Aero Team".into()];
    aero.summary = "Airships for the Create ecosystem.".into();

    let mut jei = mk("jei", "Just Enough Items", "15.0.0");
    jei.tier = ModAdaptationBadge::Tier3Fallback;
    jei.loader = ModLoaderTag::Fabric;
    jei.reload_mode = ModReloadBadge::ProcessRestart;
    jei.has_config = true;

    let mut broken = mk("brokenmod", "Broken Gadgets", "2.0");
    broken.enabled = false;
    broken.tier = ModAdaptationBadge::Tier2Shim;
    broken.loader = ModLoaderTag::Fabric;
    broken.reload_mode = ModReloadBadge::SoftRestart;
    broken.missing_deps = vec!["megu_core".into(), "fancy_lib".into()];
    broken.authors = vec!["Grief Guru".into()];
    broken.has_config = true;

    let mut lib = mk("megu_lib", "Megu Core Library", "3.1");
    lib.is_library = true;
    lib.loader = ModLoaderTag::Library;

    let mut old = mk("oldmod", "Old Contraption", "0.9");
    old.enabled = false;
    old.tier = ModAdaptationBadge::Superseded;

    let mut minimap = mk("minimap", "Fair Minimap", "4.4");
    minimap.tier = ModAdaptationBadge::ClientOnly;
    minimap.loader = ModLoaderTag::NeoForge;
    minimap.has_config = true;
    minimap.modrinth_url = "https://modrinth.example/minimap".into();

    let nolink = mk("nolink", "No Links", "1.0");

    ModMenuState::new(vec![create, aero, jei, broken, lib, old, minimap, nolink])
}

/// 25+ entries so the list pane needs scrolling at every test size.
fn big_fixture() -> ModMenuState {
    let mut entries = Vec::new();
    for i in 0..27 {
        let mut e = mk(
            &format!("mod_{i:02}"),
            &format!("Mod {i:02}"),
            &format!("1.{i}"),
        );
        if i % 9 == 0 {
            e.is_library = true;
            e.loader = ModLoaderTag::Library;
        }
        if i % 7 == 0 {
            e.enabled = false;
        }
        entries.push(e);
    }
    ModMenuState::new(entries)
}

/// Visible mod ids in display order.
fn ids(state: &ModMenuState) -> Vec<String> {
    state
        .visible_indices()
        .iter()
        .map(|&i| state.entries[i].mod_id.clone())
        .collect()
}

fn type_str(state: &mut ModMenuState, w: u32, h: u32, text: &str) {
    for c in text.chars() {
        state.handle_input(UiInputEvent::CharTyped(c), w, h);
    }
}

fn key(state: &mut ModMenuState, w: u32, h: u32, k: UiKey) -> Vec<ModMenuAction> {
    state.handle_input(UiInputEvent::KeyPressed(k), w, h)
}

fn find(hits: &[ModMenuHitRegion], id: ModMenuWidgetId) -> Option<UiRect> {
    hits.iter().find(|r| r.id == id).map(|r| r.rect)
}

/// Click the center of `id`'s hit region in a `w`×`h` frame.
fn click(state: &mut ModMenuState, w: u32, h: u32, id: ModMenuWidgetId) -> Vec<ModMenuAction> {
    let (_, hits) = state.render(w, h);
    let rect = find(&hits, id).unwrap_or_else(|| panic!("no hit region for {id:?}"));
    let (x, y) = (rect.x + rect.w as i32 / 2, rect.y + rect.h as i32 / 2);
    state.handle_input(UiInputEvent::Click { x, y }, w, h)
}

fn assert_no_overlaps(regions: &[ModMenuHitRegion]) {
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

fn assert_non_blank(rgba: &[u8]) {
    assert!(rgba.len().is_multiple_of(4));
    let (pixels, _) = rgba.as_chunks::<4>();
    let non_transparent = pixels.iter().filter(|p| p[3] != 0).count();
    assert!(non_transparent > 100, "frame is blank");
    let first = pixels[0];
    let different = pixels.iter().filter(|&&p| p != first).count();
    assert!(different > 100, "frame is a flat fill");
}

fn texts(state: &ModMenuState, w: u32, h: u32) -> Vec<String> {
    state
        .render(w, h)
        .0
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[test]
fn search_substring_multi_token_and_case_insensitive() {
    let (w, h) = (1280, 720);
    let mut s = fixture();

    // Case-insensitive substring on name.
    type_str(&mut s, w, h, "CREATE");
    assert_eq!(ids(&s), ["create", "create_aero"]);

    // Multi-token AND semantics: an entry must match every token.
    s.search = "create aero".into();
    assert_eq!(ids(&s), ["create_aero"]);

    // Substring across name and summary.
    s.search = "contraption".into();
    assert_eq!(ids(&s), ["oldmod", "create"]);

    // Loader label is searchable.
    s.search = "fabric".into();
    assert_eq!(ids(&s), ["brokenmod", "create", "jei"]);

    // mod_id is searchable even when the name differs.
    s.search = "jei".into();
    assert_eq!(ids(&s), ["jei"]);

    // Backspace edits the query live.
    s.search.clear();
    type_str(&mut s, w, h, "jei");
    assert_eq!(ids(&s), ["jei"]);
    key(&mut s, w, h, UiKey::Backspace);
    key(&mut s, w, h, UiKey::Backspace);
    assert_eq!(s.search, "j");
    key(&mut s, w, h, UiKey::Backspace);
    assert_eq!(s.search, "");
    assert_eq!(ids(&s).len(), 7);
}

#[test]
fn search_author_and_tag_prefixes() {
    let mut s = fixture();

    s.search = "@simibubi".into();
    assert_eq!(ids(&s), ["create"]);

    s.search = "#missing".into();
    assert_eq!(ids(&s), ["brokenmod"]);

    s.search = "#native".into();
    assert_eq!(ids(&s), ["create", "create_aero", "nolink"]);

    s.search = "#disabled".into();
    assert_eq!(ids(&s), ["brokenmod", "oldmod"]);

    s.search = "#fallback".into();
    assert_eq!(ids(&s), ["jei"]);

    // `#lib` finds libraries even while the pill hides them.
    assert!(!s.show_libraries);
    s.search = "#lib".into();
    assert_eq!(ids(&s), ["megu_lib"]);

    // Unknown tags match nothing (fail closed).
    s.search = "#bogus".into();
    assert!(ids(&s).is_empty());

    // Tags combine with free text.
    s.search = "#disabled broken".into();
    assert_eq!(ids(&s), ["brokenmod"]);
}

#[test]
fn search_relevance_ranks_exact_and_prefix_first() {
    let mut s = fixture();
    // "create" is an exact name match for `create` and a prefix match for
    // "Create Aeronautics" — exact sorts first.
    s.search = "create".into();
    assert_eq!(ids(&s), ["create", "create_aero"]);

    // A weaker summary match still lists, but behind name matches.
    s.search = "create contraption".into();
    // "contraption" matches create's summary; "create" prefix-matches the
    // name — combined score beats an entry matching only one token.
    assert_eq!(ids(&s), ["create"]);
}

#[test]
fn search_clear_button_restores_list() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    type_str(&mut s, w, h, "zzz");
    assert!(ids(&s).is_empty());

    // The clear button only exists while there is text to clear.
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::SearchClear).is_some());

    click(&mut s, w, h, ModMenuWidgetId::SearchClear);
    assert_eq!(s.search, "");
    assert_eq!(ids(&s).len(), 7);
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::SearchClear).is_none());
}

#[test]
fn typing_focuses_search_and_click_unfocuses() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    assert!(s.search_focused());

    // Clicking a row takes focus off the search field.
    click(&mut s, w, h, ModMenuWidgetId::ModRow(1));
    assert!(!s.search_focused());

    // Typing refocuses the field and appends.
    type_str(&mut s, w, h, "jei");
    assert!(s.search_focused());
    assert_eq!(s.search, "jei");

    // Clicking the field itself keeps typing focus.
    click(&mut s, w, h, ModMenuWidgetId::SearchField);
    assert!(s.search_focused());
}

// ---------------------------------------------------------------------------
// Filter pills and sort
// ---------------------------------------------------------------------------

#[test]
fn filter_pills() {
    let mut s = fixture();
    assert_eq!(ids(&s).len(), 7);

    s.activate(ModMenuWidgetId::FilterEnabled);
    assert_eq!(s.filter, ModMenuFilter::Enabled);
    assert_eq!(
        ids(&s),
        ["create", "create_aero", "minimap", "jei", "nolink"]
    );

    s.activate(ModMenuWidgetId::FilterIssues);
    assert_eq!(ids(&s), ["brokenmod", "jei"]);

    s.activate(ModMenuWidgetId::FilterNative);
    assert_eq!(ids(&s), ["create", "create_aero", "nolink"]);

    s.activate(ModMenuWidgetId::FilterAll);
    assert_eq!(ids(&s).len(), 7);
}

#[test]
fn libraries_pill_toggle() {
    let mut s = fixture();
    assert_eq!(ids(&s).len(), 7);

    s.activate(ModMenuWidgetId::ToggleLibraries);
    assert!(s.show_libraries);
    // "megu core library" sorts between "just enough items" and "no links".
    assert_eq!(
        ids(&s),
        [
            "brokenmod",
            "create",
            "create_aero",
            "minimap",
            "jei",
            "megu_lib",
            "nolink",
            "oldmod"
        ]
    );

    s.activate(ModMenuWidgetId::ToggleLibraries);
    assert!(!s.show_libraries);
    assert_eq!(ids(&s).len(), 7);
}

#[test]
fn sort_modes() {
    let mut s = fixture();
    assert_eq!(s.sort, ModMenuSort::NameAsc);
    assert_eq!(
        ids(&s),
        [
            "brokenmod",
            "create",
            "create_aero",
            "minimap",
            "jei",
            "nolink",
            "oldmod"
        ]
    );

    s.activate(ModMenuWidgetId::SortMode);
    assert_eq!(s.sort, ModMenuSort::NameDesc);
    assert_eq!(
        ids(&s),
        [
            "oldmod",
            "nolink",
            "jei",
            "minimap",
            "create_aero",
            "create",
            "brokenmod"
        ]
    );

    s.activate(ModMenuWidgetId::SortMode);
    assert_eq!(s.sort, ModMenuSort::Status);
    // Issues first (missing deps / Tier 3), then enabled, then disabled.
    assert_eq!(
        ids(&s),
        [
            "brokenmod",
            "jei",
            "create",
            "create_aero",
            "minimap",
            "nolink",
            "oldmod"
        ]
    );

    s.activate(ModMenuWidgetId::SortMode);
    assert_eq!(s.sort, ModMenuSort::NameAsc);
}

// ---------------------------------------------------------------------------
// Enable/disable + Apply
// ---------------------------------------------------------------------------

#[test]
fn toggle_and_apply_no_restart() {
    let mut s = fixture();
    // Row 1 is "create" (HotReload, enabled).
    s.activate(ModMenuWidgetId::ModRow(1));
    assert_eq!(
        s.selected_entry().map(|e| e.mod_id.as_str()),
        Some("create")
    );

    s.activate(ModMenuWidgetId::ToggleEnabled);
    assert!(s.is_dirty());
    assert_eq!(s.pending_change("create"), Some(false));
    assert_eq!(s.effective_enabled("create"), Some(false));

    let actions = s.activate(ModMenuWidgetId::ApplyChanges);
    assert_eq!(
        actions,
        [ModMenuAction::ApplyChanges {
            enabled_ids: vec![],
            disabled_ids: vec!["create".to_owned()],
            requires_soft_restart: false,
        }]
    );
    assert!(!s.is_dirty());
    assert_eq!(s.effective_enabled("create"), Some(false));
    assert!(
        !s.entries
            .iter()
            .find(|e| e.mod_id == "create")
            .unwrap()
            .enabled
    );
}

#[test]
fn apply_requires_soft_restart_for_soft_restart_mods() {
    let mut s = fixture();
    // Row 0 is "brokenmod" (SoftRestart, disabled + missing deps).
    s.activate(ModMenuWidgetId::ModRow(0));
    s.activate(ModMenuWidgetId::ToggleEnabled);

    let actions = s.activate(ModMenuWidgetId::ApplyChanges);
    assert_eq!(
        actions,
        [ModMenuAction::ApplyChanges {
            enabled_ids: vec!["brokenmod".to_owned()],
            disabled_ids: vec![],
            requires_soft_restart: true,
        }]
    );
}

#[test]
fn apply_label_reflects_restart_cost() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    // Only a HotReload mod staged → "(No Restart)".
    s.activate(ModMenuWidgetId::ModRow(1)); // create
    s.activate(ModMenuWidgetId::ToggleEnabled);
    assert!(texts(&s, w, h).iter().any(|t| t.contains("No Restart")));

    // A SoftRestart mod staged → "(Soft Reload)".
    s.activate(ModMenuWidgetId::ApplyChanges);
    s.activate(ModMenuWidgetId::ModRow(0)); // brokenmod
    s.activate(ModMenuWidgetId::ToggleEnabled);
    assert!(texts(&s, w, h).iter().any(|t| t.contains("Soft Reload")));
}

#[test]
fn toggling_twice_cancels_the_pending_change() {
    let mut s = fixture();
    s.activate(ModMenuWidgetId::ModRow(1));
    s.activate(ModMenuWidgetId::ToggleEnabled);
    assert!(s.is_dirty());
    s.activate(ModMenuWidgetId::ToggleEnabled);
    assert!(!s.is_dirty());
    assert_eq!(s.effective_enabled("create"), Some(true));
    assert!(s.activate(ModMenuWidgetId::ApplyChanges).is_empty());
}

#[test]
fn apply_button_absent_when_nothing_staged() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::ApplyChanges).is_none());
    assert!(s.activate(ModMenuWidgetId::ApplyChanges).is_empty());
}

// ---------------------------------------------------------------------------
// Missing dependencies, links, configure
// ---------------------------------------------------------------------------

#[test]
fn missing_deps_download_emits_exact_ids_only_on_click() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    // Row 0 = brokenmod with missing_deps [megu_core, fancy_lib].
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::DownloadMissingDeps).is_some());

    // Nothing is emitted until the button is actually clicked.
    let actions = click(&mut s, w, h, ModMenuWidgetId::DownloadMissingDeps);
    assert_eq!(
        actions,
        [ModMenuAction::DownloadMissingFromModrinth {
            mod_id: "brokenmod".to_owned(),
            missing_deps: vec!["megu_core".to_owned(), "fancy_lib".to_owned()],
        }]
    );

    // A mod without missing deps has no button and emits nothing.
    s.activate(ModMenuWidgetId::ModRow(1)); // create
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::DownloadMissingDeps).is_none());
    assert!(s.activate(ModMenuWidgetId::DownloadMissingDeps).is_empty());
}

#[test]
fn link_buttons_render_only_when_url_present() {
    let (w, h) = (1280, 720);
    let mut s = fixture();

    // "nolink" (index 5): every URL empty → no link buttons at all.
    s.activate(ModMenuWidgetId::ModRow(5));
    assert_eq!(
        s.selected_entry().map(|e| e.mod_id.as_str()),
        Some("nolink")
    );
    let (_, hits) = s.render(w, h);
    for id in [
        ModMenuWidgetId::LinkWebsite,
        ModMenuWidgetId::LinkIssues,
        ModMenuWidgetId::LinkSource,
        ModMenuWidgetId::LinkModrinth,
    ] {
        assert!(find(&hits, id).is_none(), "{id:?} should be hidden");
    }

    // "minimap" (index 3): only a Modrinth URL.
    s.activate(ModMenuWidgetId::ModRow(3));
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::LinkModrinth).is_some());
    assert!(find(&hits, ModMenuWidgetId::LinkWebsite).is_none());
    assert!(find(&hits, ModMenuWidgetId::LinkIssues).is_none());
    assert!(find(&hits, ModMenuWidgetId::LinkSource).is_none());

    let actions = click(&mut s, w, h, ModMenuWidgetId::LinkModrinth);
    assert_eq!(
        actions,
        [ModMenuAction::OpenUrl(
            "https://modrinth.example/minimap".to_owned()
        )]
    );

    // "create" has all four links; Website opens its URL.
    s.activate(ModMenuWidgetId::ModRow(1));
    let (_, hits) = s.render(w, h);
    for id in [
        ModMenuWidgetId::LinkWebsite,
        ModMenuWidgetId::LinkIssues,
        ModMenuWidgetId::LinkSource,
        ModMenuWidgetId::LinkModrinth,
    ] {
        assert!(find(&hits, id).is_some(), "{id:?} should be visible");
    }
    let actions = click(&mut s, w, h, ModMenuWidgetId::LinkWebsite);
    assert_eq!(
        actions,
        [ModMenuAction::OpenUrl(
            "https://example.com/create".to_owned()
        )]
    );
}

#[test]
fn configure_only_enabled_with_config() {
    let (w, h) = (1280, 720);
    let mut s = fixture();

    // "nolink" has no config → disabled, no region, no action.
    s.activate(ModMenuWidgetId::ModRow(5));
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::ConfigureMod).is_none());
    assert!(s.activate(ModMenuWidgetId::ConfigureMod).is_empty());

    // "create" has a config → clickable and emits the mod id.
    s.activate(ModMenuWidgetId::ModRow(1));
    let actions = click(&mut s, w, h, ModMenuWidgetId::ConfigureMod);
    assert_eq!(actions, [ModMenuAction::ConfigureMod("create".to_owned())]);
}

#[test]
fn footer_actions() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    assert_eq!(
        click(&mut s, w, h, ModMenuWidgetId::AddFromModrinth),
        [ModMenuAction::OpenModrinthBrowser]
    );
    assert_eq!(
        click(&mut s, w, h, ModMenuWidgetId::OpenModsFolder),
        [ModMenuAction::OpenModsFolder]
    );
    assert_eq!(
        click(&mut s, w, h, ModMenuWidgetId::Done),
        [ModMenuAction::Close]
    );
}

// ---------------------------------------------------------------------------
// Layout integrity + rasterization
// ---------------------------------------------------------------------------

#[test]
fn layout_has_zero_overlaps_and_non_blank_pixels() {
    for &(w, h) in &SIZES {
        for lang in [UiLang::En, UiLang::Ja] {
            for mut state in [ModMenuState::new(vec![]), fixture(), big_fixture()] {
                state.lang = lang;
                let (list, hits) = state.render(w, h);
                assert!(!list.is_empty(), "{w}x{h} {lang:?} produced no commands");
                assert_no_overlaps(&hits);
                let rgba = rasterize_draw_list(w, h, &list);
                assert_eq!(rgba.len(), (w * h * 4) as usize);
                assert_non_blank(&rgba);
            }
        }
    }
}

#[test]
fn scrollable_list_shows_scroll_controls_and_clipped_rows() {
    let (w, h) = (1280, 720);
    let mut s = big_fixture(); // 27 entries, libraries hidden → 24 visible
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, ModMenuWidgetId::ListScrollUp).is_some());
    assert!(find(&hits, ModMenuWidgetId::ListScrollDown).is_some());
    // Only one page of rows gets a region.
    let rows = hits
        .iter()
        .filter(|r| matches!(r.id, ModMenuWidgetId::ModRow(_)))
        .count();
    assert!(rows < 24, "rows should be clipped to one page, got {rows}");

    s.activate(ModMenuWidgetId::ListScrollDown);
    s.activate(ModMenuWidgetId::ListScrollDown);
    assert_eq!(s.scroll_offset(), 2);
    s.activate(ModMenuWidgetId::ListScrollUp);
    assert_eq!(s.scroll_offset(), 1);

    // Row clicks select by visible index.
    click(&mut s, w, h, ModMenuWidgetId::ModRow(3));
    assert_eq!(s.selected_row(), 3);
}

#[test]
fn click_resolves_through_widget_at() {
    let (w, h) = (1280, 720);
    let s = fixture();
    let (_, hits) = s.render(w, h);
    let rect = find(&hits, ModMenuWidgetId::ModRow(2)).unwrap();
    let (x, y) = (rect.x + rect.w as i32 / 2, rect.y + rect.h as i32 / 2);
    assert_eq!(
        s.widget_at(w, h, x, y),
        Some(ModMenuWidgetId::ModRow(2)),
        "click should land on the third visible row"
    );
}

// ---------------------------------------------------------------------------
// Keyboard & gamepad navigation
// ---------------------------------------------------------------------------

#[test]
fn keyboard_navigation() {
    let (w, h) = (1280, 720);
    let mut s = fixture();

    // Search starts focused: Enter commits it, then Enter toggles.
    key(&mut s, w, h, UiKey::Enter);
    assert!(!s.search_focused());

    key(&mut s, w, h, UiKey::Down);
    assert_eq!(s.selected_row(), 1);
    key(&mut s, w, h, UiKey::Down);
    assert_eq!(s.selected_row(), 2);
    key(&mut s, w, h, UiKey::Up);
    assert_eq!(s.selected_row(), 1);

    // Wrap-around: from row 0, Up lands on the last row.
    key(&mut s, w, h, UiKey::Up);
    key(&mut s, w, h, UiKey::Up);
    assert_eq!(s.selected_row(), 6);

    // Enter toggles the selected mod when search isn't focused.
    key(&mut s, w, h, UiKey::Enter);
    assert!(s.is_dirty());
    assert_eq!(s.pending_change("oldmod"), Some(true));

    // Tab hands focus back to the search field.
    key(&mut s, w, h, UiKey::Tab);
    assert!(s.search_focused());

    // Escape closes the screen.
    assert_eq!(key(&mut s, w, h, UiKey::Escape), [ModMenuAction::Close]);
}

#[test]
fn gamepad_navigation() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    let pad = |s: &mut ModMenuState, b: GamepadButton| {
        s.handle_input(UiInputEvent::GamepadPressed(b), w, h)
    };

    // South commits the focused search field (mirrors Enter).
    pad(&mut s, GamepadButton::South);
    assert!(!s.search_focused());

    pad(&mut s, GamepadButton::DpadDown);
    assert_eq!(s.selected_row(), 1);
    pad(&mut s, GamepadButton::DpadUp);
    assert_eq!(s.selected_row(), 0);

    // South toggles the selected mod once focus is off the search field.
    pad(&mut s, GamepadButton::South);
    assert!(s.is_dirty());
    assert_eq!(s.pending_change("brokenmod"), Some(true));

    // East and Start both back out.
    assert_eq!(pad(&mut s, GamepadButton::East), [ModMenuAction::Close]);
    assert_eq!(pad(&mut s, GamepadButton::Start), [ModMenuAction::Close]);
}

#[test]
fn gamepad_dpad_left_right_pages_the_list() {
    let (w, h) = (1920, 1080);
    let mut s = big_fixture(); // 24 visible rows after hiding libraries
    s.activate(ModMenuWidgetId::ModRow(0));
    let page = {
        let (_, hits) = s.render(w, h);
        hits.iter()
            .filter(|r| matches!(r.id, ModMenuWidgetId::ModRow(_)))
            .count()
    };
    s.handle_input(UiInputEvent::GamepadPressed(GamepadButton::DpadRight), w, h);
    assert_eq!(s.selected_row(), page);
    s.handle_input(UiInputEvent::GamepadPressed(GamepadButton::DpadLeft), w, h);
    assert_eq!(s.selected_row(), 0);
}
