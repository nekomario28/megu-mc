// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Tests for the Polymorph-style crafting-conflict switcher
//! (`conflict_widget`) and the mod-sync diff consent screen
//! (`mod_sync_ui`): hide-unless-conflicted semantics, `< k/N >` cycling
//! with per-signature preference memory, the expandable candidate popup,
//! diff badges/opt-in checkboxes/SHA-512 marks, the isolated-profile
//! consent flow, scrollable 100+ row lists, viewport clamping,
//! overlap-free hit regions, and zero-tofu EN/JA text.

use megu_client_ui::conflict_widget::{
    ConflictCandidate, ConflictHitRegion, ConflictWidget, ConflictWidgetAction, ConflictWidgetId,
};
use megu_client_ui::draw::{glyph_is_covered, text_height, text_width, UiCommand, UiDrawList, UiRect};
use megu_client_ui::mod_sync_ui::{
    ModSyncDiffEntry, ModSyncDiffKind, ModSyncSource, ModSyncUiAction, ModSyncUiState,
    ModSyncWidgetId,
};
use megu_client_ui::state::{GamepadButton, UiInputEvent, UiKey, UiLang};

const SIZES: [(u32, u32); 4] = [(640, 360), (1280, 720), (1920, 1080), (3840, 2160)];

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

/// Every emitted command must have finite, viewport-clamped geometry.
fn assert_commands_in_viewport(list: &UiDrawList, w: u32, h: u32) {
    for cmd in &list.commands {
        match cmd {
            UiCommand::FillRect { rect, .. }
            | UiCommand::BorderRect { rect, .. }
            | UiCommand::ProgressBar { rect, .. } => {
                assert!(rect.w > 0 && rect.h > 0, "empty rect: {rect:?}");
                assert!(
                    rect.right() <= w as i32 && rect.bottom() <= h as i32,
                    "rect escapes {w}x{h}: {rect:?}"
                );
            }
            UiCommand::Text {
                x,
                y,
                scale,
                text,
                ..
            } => {
                let tw = i64::from(text_width(text, *scale));
                let th = i64::from(text_height(text, *scale));
                assert!(
                    i64::from(*x) + tw <= i64::from(w) + 1,
                    "text escapes right edge at {x}: {text:?} ({tw}px)"
                );
                assert!(
                    i64::from(*y) + th <= i64::from(h),
                    "text escapes bottom edge at {y}: {text:?}"
                );
            }
        }
    }
}

/// No two emitted hit regions may overlap.
fn assert_regions_non_overlapping(regions: &[UiRect]) {
    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            assert!(!a.intersects(*b), "overlap: {a:?} vs {b:?}");
        }
    }
}

fn text_strings(list: &UiDrawList) -> Vec<String> {
    list.commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn joined_text(list: &UiDrawList) -> String {
    text_strings(list).join("\n")
}

/// Zero-tofu check: every emitted character has a real glyph.
fn assert_zero_tofu(list: &UiDrawList) {
    for text in text_strings(list) {
        for c in text.chars() {
            assert!(glyph_is_covered(c), "tofu glyph {c:?} in {text:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Conflict widget fixtures + click helpers
// ---------------------------------------------------------------------------

fn candidates() -> Vec<ConflictCandidate> {
    vec![
        ConflictCandidate::new("minecraft:iron_pickaxe", "Iron Pickaxe", "minecraft"),
        ConflictCandidate::new("create:brass_pickaxe", "Brass Pickaxe", "create"),
        ConflictCandidate::new("thermal:flux_pick", "Flux Pick", "thermal"),
    ]
}

fn widget_with(sig: u64) -> ConflictWidget {
    let mut w = ConflictWidget::new();
    w.set_candidates(sig, candidates());
    w
}

fn conflict_render(
    w: &ConflictWidget,
    anchor: UiRect,
    vw: u32,
    vh: u32,
) -> (UiDrawList, Vec<ConflictHitRegion>) {
    let mut list = UiDrawList::new();
    let hits = w.render(anchor, 1, vw, vh, &mut list);
    (list, hits)
}

/// Click the center of `id`'s hit region and return emitted actions.
fn conflict_click(
    w: &mut ConflictWidget,
    anchor: UiRect,
    id: ConflictWidgetId,
) -> Vec<ConflictWidgetAction> {
    let (_, hits) = conflict_render(w, anchor, 640, 360);
    let r = hits
        .iter()
        .find(|h| h.id == id)
        .unwrap_or_else(|| panic!("missing region {id:?}"))
        .rect;
    w.handle_input(
        UiInputEvent::Click {
            x: r.x + r.w as i32 / 2,
            y: r.y + r.h as i32 / 2,
        },
        anchor,
        1,
        640,
        360,
    )
}

fn region(
    hits: &[ConflictHitRegion],
    id: ConflictWidgetId,
) -> UiRect {
    hits.iter()
        .find(|h| h.id == id)
        .unwrap_or_else(|| panic!("missing region {id:?}"))
        .rect
}

// ---------------------------------------------------------------------------
// Conflict widget: visibility gating
// ---------------------------------------------------------------------------

#[test]
fn conflict_hidden_with_zero_or_one_candidate() {
    let anchor = UiRect::new(120, 120, 18, 18);
    for count in [0usize, 1] {
        let mut w = ConflictWidget::new();
        w.set_candidates(7, candidates()[..count].to_vec());
        assert!(!w.is_active());
        let (list, hits) = conflict_render(&w, anchor, 640, 360);
        assert!(list.is_empty(), "{count} candidates emitted draw commands");
        assert!(hits.is_empty(), "{count} candidates emitted hit regions");
        // Input is inert too — no phantom actions.
        assert!(w
            .handle_input(UiInputEvent::Click { x: 0, y: 0 }, anchor, 1, 640, 360)
            .is_empty());
        assert!(w
            .handle_input(UiInputEvent::KeyPressed(UiKey::Right), anchor, 1, 640, 360)
            .is_empty());
    }
}

#[test]
fn conflict_shows_strip_with_two_or_more_candidates() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let w = widget_with(1);
    assert!(w.is_active());
    let (list, hits) = conflict_render(&w, anchor, 640, 360);
    assert!(!list.is_empty());
    // [ < ] 1/3 [ > ]
    for id in [
        ConflictWidgetId::Prev,
        ConflictWidgetId::Counter,
        ConflictWidgetId::Next,
    ] {
        region(&hits, id);
    }
    let text = joined_text(&list);
    assert!(text.contains("1/3"), "counter label missing: {text}");
    // Output name + @mod badge.
    assert!(text.contains("Iron Pickaxe"), "output name missing: {text}");
    assert!(text.contains("@minecraft"), "mod badge missing: {text}");
}

// ---------------------------------------------------------------------------
// Conflict widget: selection + preference memory
// ---------------------------------------------------------------------------

#[test]
fn conflict_arrows_cycle_with_wrap_and_emit_select_recipe() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = widget_with(1);
    let [act] = conflict_click(&mut w, anchor, ConflictWidgetId::Next)
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "create:brass_pickaxe".into(),
            index: 1
        }
    );
    conflict_click(&mut w, anchor, ConflictWidgetId::Next);
    let [act] = conflict_click(&mut w, anchor, ConflictWidgetId::Next)
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "minecraft:iron_pickaxe".into(),
            index: 0
        },
        "next must wrap to 0"
    );
    let [act] = conflict_click(&mut w, anchor, ConflictWidgetId::Prev)
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "thermal:flux_pick".into(),
            index: 2
        },
        "prev must wrap to last"
    );
}

#[test]
fn conflict_remembers_preference_per_input_signature() {
    let mut w = widget_with(0xAA);
    w.select_index(2);
    assert_eq!(w.preferred_recipe(0xAA), Some("thermal:flux_pick"));

    // Same signature refills -> remembered recipe is restored.
    w.set_candidates(0xAA, candidates());
    assert_eq!(w.selected(), 2);
    // A different signature starts fresh.
    w.set_candidates(0xBB, candidates());
    assert_eq!(w.selected(), 0);
    // Pick something else for the new signature; the old key is intact.
    w.select_index(1);
    w.set_candidates(0xAA, candidates());
    assert_eq!(w.selected(), 2);
    w.set_candidates(0xBB, candidates());
    assert_eq!(w.selected(), 1);

    w.clear_preferences();
    w.set_candidates(0xAA, candidates());
    assert_eq!(w.selected(), 0);
}

#[test]
fn conflict_popup_lists_all_candidates_and_row_click_selects() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = widget_with(1);
    // The k/N counter toggles the expanded list.
    conflict_click(&mut w, anchor, ConflictWidgetId::Counter);
    assert!(w.expanded());
    let (list, hits) = conflict_render(&w, anchor, 640, 360);
    for i in 0..3 {
        region(&hits, ConflictWidgetId::PopupRow(i));
    }
    let text = joined_text(&list);
    for name in ["Iron Pickaxe", "Brass Pickaxe", "Flux Pick"] {
        assert!(text.contains(name), "popup missing {name}: {text}");
    }
    assert!(text.contains("@thermal"), "popup missing mod badge: {text}");
    // Direct pick: click row 2 -> SelectRecipe + popup closes + memory.
    let [act] = conflict_click(&mut w, anchor, ConflictWidgetId::PopupRow(2))
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "thermal:flux_pick".into(),
            index: 2
        }
    );
    assert!(!w.expanded());
    assert_eq!(w.preferred_recipe(1), Some("thermal:flux_pick"));
}

#[test]
fn conflict_popup_scrolls_long_lists() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = ConflictWidget::new();
    let many: Vec<ConflictCandidate> = (0..12)
        .map(|i| {
            ConflictCandidate::new(
                format!("mod{j}:recipe{i}", j = i % 3),
                format!("Output {i}"),
                format!("mod{}", i % 3),
            )
        })
        .collect();
    w.set_candidates(9, many);
    w.activate(ConflictWidgetId::Counter);
    assert!(w.expanded());
    let (_, hits) = conflict_render(&w, anchor, 640, 360);
    // 8-row window over 12 candidates -> scroll buttons exist.
    region(&hits, ConflictWidgetId::PopupScrollUp);
    region(&hits, ConflictWidgetId::PopupScrollDown);
    region(&hits, ConflictWidgetId::PopupRow(0));
    // Scroll down moves the window regardless of the selected row.
    w.activate(ConflictWidgetId::PopupScrollDown);
    let (_, hits) = conflict_render(&w, anchor, 640, 360);
    assert!(hits.iter().any(|h| h.id == ConflictWidgetId::PopupRow(8)));
    assert!(hits.iter().all(|h| h.id != ConflictWidgetId::PopupRow(0)));
    // Direct selection past the window pulls it back into view.
    w.select_index(0);
    let (_, hits) = conflict_render(&w, anchor, 640, 360);
    assert!(hits.iter().any(|h| h.id == ConflictWidgetId::PopupRow(0)));
}

// ---------------------------------------------------------------------------
// Conflict widget: keyboard / gamepad
// ---------------------------------------------------------------------------

#[test]
fn conflict_keyboard_cycles_and_controls_popup() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = widget_with(1);
    let key = |w: &mut ConflictWidget, k: UiKey| {
        w.handle_input(UiInputEvent::KeyPressed(k), anchor, 1, 640, 360)
    };
    let [act] = key(&mut w, UiKey::Right).try_into().unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "create:brass_pickaxe".into(),
            index: 1
        }
    );
    let [act] = key(&mut w, UiKey::Left).try_into().unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "minecraft:iron_pickaxe".into(),
            index: 0
        }
    );
    // Down opens the list; Up/Down then walk the selection.
    key(&mut w, UiKey::Down);
    assert!(w.expanded());
    let [act] = key(&mut w, UiKey::Down).try_into().unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "create:brass_pickaxe".into(),
            index: 1
        }
    );
    // Escape closes without another selection.
    assert!(key(&mut w, UiKey::Escape).is_empty());
    assert!(!w.expanded());
    // Enter toggles the list; Tab cycles forward.
    key(&mut w, UiKey::Enter);
    assert!(w.expanded());
    key(&mut w, UiKey::Enter);
    assert!(!w.expanded());
    let [act] = key(&mut w, UiKey::Tab).try_into().unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "thermal:flux_pick".into(),
            index: 2
        },
        "tab cycles forward from the current selection"
    );
}

#[test]
fn conflict_gamepad_maps_to_widget() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = widget_with(1);
    let pad = |w: &mut ConflictWidget, b: GamepadButton| {
        w.handle_input(UiInputEvent::GamepadPressed(b), anchor, 1, 640, 360)
    };
    let [act] = pad(&mut w, GamepadButton::DpadRight).try_into().unwrap();
    assert_eq!(
        act,
        ConflictWidgetAction::SelectRecipe {
            recipe_id: "create:brass_pickaxe".into(),
            index: 1
        }
    );
    pad(&mut w, GamepadButton::South); // opens list
    assert!(w.expanded());
    pad(&mut w, GamepadButton::East); // closes it
    assert!(!w.expanded());
}

#[test]
fn conflict_click_outside_closes_popup() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = widget_with(1);
    w.toggle_expanded();
    assert!(w.expanded());
    let acts = w.handle_input(UiInputEvent::Click { x: 5, y: 5 }, anchor, 1, 640, 360);
    assert!(acts.is_empty());
    assert!(!w.expanded());
}

// ---------------------------------------------------------------------------
// Conflict widget: anchoring + viewport safety
// ---------------------------------------------------------------------------

#[test]
fn conflict_flips_left_when_anchor_touches_right_edge() {
    let w = widget_with(1);
    let anchor = UiRect::new(640 - 40, 120, 18, 18);
    let (_, hits) = conflict_render(&w, anchor, 640, 360);
    let counter = region(&hits, ConflictWidgetId::Counter);
    assert!(
        counter.right() <= anchor.x,
        "strip should sit left of the slot: {counter:?} vs {anchor:?}"
    );
}

#[test]
fn conflict_popup_flips_above_when_no_room_below() {
    let mut w = widget_with(1);
    w.toggle_expanded();
    let anchor = UiRect::new(120, 330, 18, 18);
    let (_, hits) = conflict_render(&w, anchor, 640, 360);
    let counter = region(&hits, ConflictWidgetId::Counter);
    let row0 = region(&hits, ConflictWidgetId::PopupRow(0));
    assert!(
        row0.bottom() <= counter.y || row0.y < counter.y,
        "popup should open above the strip: row {row0:?} counter {counter:?}"
    );
}

#[test]
fn conflict_commands_and_regions_stay_in_viewport() {
    let mut w = widget_with(1);
    for &(vw, vh) in &SIZES[..2] {
        for anchor in [
            UiRect::new(60, 60, 18, 18),
            UiRect::new((vw - 40) as i32, 60, 18, 18),
            UiRect::new(60, (vh - 40) as i32, 18, 18),
            UiRect::new(4, 4, 18, 18),
        ] {
            for expanded in [false, true] {
                if expanded != w.expanded() {
                    w.toggle_expanded();
                }
                let (list, hits) = conflict_render(&w, anchor, vw, vh);
                assert_commands_in_viewport(&list, vw, vh);
                for h in &hits {
                    assert!(h.rect.x >= 0 && h.rect.y >= 0, "hit {h:?} off-screen");
                    assert!(
                        h.rect.right() <= vw as i32 && h.rect.bottom() <= vh as i32,
                        "hit {h:?} escapes {vw}x{vh}"
                    );
                }
                let rects: Vec<UiRect> = hits.iter().map(|h| h.rect).collect();
                assert_regions_non_overlapping(&rects);
            }
            if w.expanded() {
                w.toggle_expanded();
            }
        }
    }
}

#[test]
fn conflict_widget_has_zero_tofu_in_english_and_japanese() {
    let anchor = UiRect::new(120, 120, 18, 18);
    for lang in [UiLang::En, UiLang::Ja] {
        let mut w = widget_with(1);
        w.lang = lang;
        w.toggle_expanded();
        let (list, _) = conflict_render(&w, anchor, 640, 360);
        assert_zero_tofu(&list);
    }
}

#[test]
fn conflict_hostile_strings_stay_single_line_and_on_screen() {
    let anchor = UiRect::new(120, 120, 18, 18);
    let mut w = ConflictWidget::new();
    w.set_candidates(
        1,
        vec![
            ConflictCandidate::new("a:b", "Evil\nName\tOverflow", "mod\nx"),
            ConflictCandidate::new("c:d", "Nice", "nice"),
        ],
    );
    w.toggle_expanded();
    let (list, hits) = conflict_render(&w, anchor, 640, 360);
    assert_commands_in_viewport(&list, 640, 360);
    let rects: Vec<UiRect> = hits.iter().map(|h| h.rect).collect();
    assert_regions_non_overlapping(&rects);
}

// ---------------------------------------------------------------------------
// Mod-sync fixtures + click helpers
// ---------------------------------------------------------------------------

fn diff_entry(id: &str, name: &str, kind: ModSyncDiffKind) -> ModSyncDiffEntry {
    ModSyncDiffEntry::new(id, name, "1.0.0", kind)
}

fn sync_fixture() -> ModSyncUiState {
    let mut create = diff_entry("create", "Create", ModSyncDiffKind::Missing);
    create.license_spdx = "MIT".into();
    create.sha512_hex = Some("abcdef0123456789".repeat(4));
    create.sha512_verified = true;
    create.download_size = 4_200_000;
    create.summary_en = "Rotation contraptions.".into();
    create.summary_ja = "回転する機械。".into();

    let mut jei = diff_entry("jei", "Just Enough Items", ModSyncDiffKind::VersionMismatch);
    jei.client_version = Some("14.9.0".into());
    jei.server_version = "15.0.0".into();
    jei.license_spdx = "MIT".into();
    jei.sha512_hex = Some("0123".repeat(16));

    let mut opt = diff_entry("appleskin", "AppleSkin", ModSyncDiffKind::Optional);
    opt.summary_en = "Food stats.".into();

    let mut blocked = diff_entry("optifine", "OptiFine", ModSyncDiffKind::Incompatible);
    blocked.client_version = Some("HD_U".into());
    blocked.source = ModSyncSource::ManualRequired;

    let mut matched = diff_entry("megu_core", "Megu Core", ModSyncDiffKind::Matched);
    matched.client_version = Some("1.0.0".into());

    let mut guided = diff_entry("cfmod", "CF Gadgets", ModSyncDiffKind::Missing);
    guided.source = ModSyncSource::CurseForgeGuided;
    guided.curseforge_url = Some("https://example.invalid/cfmod".into());
    guided.summary_en = "Needs a manual grab.".into();

    ModSyncUiState::new(
        "Test Server",
        "test-srv",
        vec![create, jei, opt, blocked, matched, guided],
    )
}

fn sync_render(s: &ModSyncUiState, w: u32, h: u32) -> (UiDrawList, Vec<UiRect>, Vec<ModSyncWidgetId>) {
    let (list, hits) = s.render(w, h);
    let rects = hits.iter().map(|h| h.rect).collect();
    let ids = hits.iter().map(|h| h.id).collect();
    (list, rects, ids)
}

fn sync_click(s: &mut ModSyncUiState, id: ModSyncWidgetId) -> Vec<ModSyncUiAction> {
    let (_, hits) = s.render(1280, 720);
    let r = hits
        .iter()
        .find(|h| h.id == id)
        .unwrap_or_else(|| panic!("missing region {id:?}"))
        .rect;
    s.handle_input(
        UiInputEvent::Click {
            x: r.x + r.w as i32 / 2,
            y: r.y + r.h as i32 / 2,
        },
        1280,
        720,
    )
}

// ---------------------------------------------------------------------------
// Mod-sync: rendering + consent flow
// ---------------------------------------------------------------------------

#[test]
fn modsync_renders_inside_viewport_at_every_size() {
    let s = sync_fixture();
    for &(w, h) in &SIZES {
        let (list, rects, _) = sync_render(&s, w, h);
        assert!(!list.is_empty(), "blank frame at {w}x{h}");
        assert_commands_in_viewport(&list, w, h);
        for r in &rects {
            assert!(r.x >= 0 && r.y >= 0, "hit {r:?} off-screen");
            assert!(r.right() <= w as i32 && r.bottom() <= h as i32);
        }
        assert_regions_non_overlapping(&rects);
    }
}

#[test]
fn modsync_header_shows_profile_isolation_and_counts() {
    let s = sync_fixture();
    let (list, _, _) = sync_render(&s, 1280, 720);
    let text = joined_text(&list);
    assert!(
        text.contains("profiles/test-srv/mods"),
        "isolated profile path missing: {text}"
    );
    assert!(
        text.contains("never changed"),
        "isolation disclaimer missing: {text}"
    );
    for frag in ["new", "to update", "optional", "blocked", "in sync"] {
        assert!(text.contains(frag), "counts line missing {frag}: {text}");
    }
}

#[test]
fn modsync_profile_dir_is_sanitized() {
    let mut s = ModSyncUiState::default();
    s.server_id = "../evil".into();
    assert_eq!(s.profile_dir(), "profiles/_evil/mods");
    s.server_id = "..".into();
    assert_eq!(s.profile_dir(), "profiles/default/mods");
    s.server_id = "a/b\\c".into();
    assert_eq!(s.profile_dir(), "profiles/a_b_c/mods");
    s.server_id = "hub.example.net".into();
    assert_eq!(s.profile_dir(), "profiles/hub.example.net/mods");
    s.server_id = String::new();
    assert_eq!(s.profile_dir(), "profiles/default/mods");
}

#[test]
fn modsync_kind_badges_and_sha_marks_render() {
    let s = sync_fixture();
    let (list, _, _) = sync_render(&s, 1280, 720);
    let text = joined_text(&list);
    for badge in ["NEW", "UPDATE", "OPTION", "BLOCKED", "OK"] {
        assert!(text.contains(badge), "badge {badge} missing: {text}");
    }
    assert!(text.contains("SHA-512 OK"), "verified mark missing: {text}");
    assert!(text.contains("SHA-512 ?"), "unverified mark missing: {text}");
    assert!(text.contains("MIT"), "license tag missing: {text}");
    assert!(text.contains("CurseForge"), "source badge missing: {text}");
}

#[test]
fn modsync_optional_optin_is_explicit_and_fail_closed() {
    let mut s = sync_fixture();
    let opt_idx = 2usize; // "appleskin" row in fixture order
    assert!(!s.optional_opted("appleskin"));
    // Only Optional rows may toggle.
    s.toggle_optional("jei");
    assert!(s.plan().optional_ids.is_empty());
    s.activate(ModSyncWidgetId::OptionalCheck(opt_idx));
    assert!(s.optional_opted("appleskin"));
    assert_eq!(s.plan().optional_ids, ["appleskin"]);
    // Toggling back removes it.
    s.activate(ModSyncWidgetId::OptionalCheck(opt_idx));
    assert!(!s.optional_opted("appleskin"));
    // The opt-in checkbox is a distinct, non-overlapping hit region.
    let (_, rects, ids) = sync_render(&s, 1280, 720);
    assert!(ids.contains(&ModSyncWidgetId::OptionalCheck(opt_idx)));
    assert_regions_non_overlapping(&rects);
}

#[test]
fn modsync_approve_emits_full_plan() {
    let mut s = sync_fixture();
    s.activate(ModSyncWidgetId::OptionalCheck(2));
    let [act] = s
        .activate(ModSyncWidgetId::ApproveSync)
        .try_into()
        .unwrap();
    let ModSyncUiAction::ApproveAndSync {
        sync_ids,
        optional_ids,
    } = act
    else {
        panic!("expected ApproveAndSync, got {act:?}")
    };
    // Required rows are in; Matched and Incompatible rows are out.
    for id in ["create", "jei", "cfmod", "appleskin"] {
        assert!(sync_ids.iter().any(|m| m == id), "missing {id} in {sync_ids:?}");
    }
    assert!(!sync_ids.iter().any(|m| m == "megu_core"));
    assert!(!sync_ids.iter().any(|m| m == "optifine"));
    assert_eq!(optional_ids, ["appleskin"]);
    // Incompatible rows land on the blocked list for the host to see.
    assert_eq!(s.plan().blocked_ids, ["optifine"]);
}

#[test]
fn modsync_decline_paths_all_disconnect() {
    let mut s = sync_fixture();
    let [act] = s.activate(ModSyncWidgetId::Decline).try_into().unwrap();
    assert_eq!(act, ModSyncUiAction::DeclineAndDisconnect);
    let [act] = s
        .handle_input(UiInputEvent::KeyPressed(UiKey::Escape), 1280, 720)
        .try_into()
        .unwrap();
    assert_eq!(act, ModSyncUiAction::DeclineAndDisconnect);
    let [act] = s
        .handle_input(UiInputEvent::GamepadPressed(GamepadButton::East), 1280, 720)
        .try_into()
        .unwrap();
    assert_eq!(act, ModSyncUiAction::DeclineAndDisconnect);
    let [act] = s
        .handle_input(UiInputEvent::GamepadPressed(GamepadButton::Start), 1280, 720)
        .try_into()
        .unwrap();
    assert_eq!(act, ModSyncUiAction::DeclineAndDisconnect);
}

#[test]
fn modsync_approve_is_never_reachable_by_keyboard() {
    let mut s = sync_fixture();
    for key in [
        UiKey::Up,
        UiKey::Down,
        UiKey::Left,
        UiKey::Right,
        UiKey::Enter,
        UiKey::Space,
        UiKey::Tab,
        UiKey::Backspace,
    ] {
        let acts = s.handle_input(UiInputEvent::KeyPressed(key), 1280, 720);
        assert!(
            acts.iter()
                .all(|a| !matches!(a, ModSyncUiAction::ApproveAndSync { .. })),
            "{key:?} emitted an approval"
        );
    }
    // Gamepad South toggles details but cannot approve either.
    let acts = s.handle_input(UiInputEvent::GamepadPressed(GamepadButton::South), 1280, 720);
    assert!(acts.is_empty());
}

#[test]
fn modsync_row_expansion_shows_details() {
    let mut s = sync_fixture();
    sync_click(&mut s, ModSyncWidgetId::Row(0));
    assert_eq!(s.expanded_row(), Some(0));
    let (list, _, _ids) = sync_render(&s, 1280, 720);
    let text = joined_text(&list);
    assert!(text.contains("Version"), "version line missing: {text}");
    assert!(text.contains("verified"), "sha detail missing: {text}");
    assert!(text.contains("Modrinth"), "source detail missing: {text}");
    assert!(text.contains("MIT"), "license detail missing: {text}");
    assert!(text.contains("Rotation contraptions"), "summary missing: {text}");
    // Clicking the same row collapses it again.
    sync_click(&mut s, ModSyncWidgetId::Row(0));
    assert_eq!(s.expanded_row(), None);
    // Enter/Space expand the keyboard-selected row too.
    s.handle_input(UiInputEvent::KeyPressed(UiKey::Enter), 1280, 720);
    assert_eq!(s.expanded_row(), Some(0));
}

#[test]
fn modsync_guided_rows_open_curseforge_page() {
    let mut s = sync_fixture();
    // Footer shortcut -> first guided entry.
    let [act] = s
        .activate(ModSyncWidgetId::CurseForgePage)
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ModSyncUiAction::OpenCurseForgePage {
            mod_id: "cfmod".into(),
            url: "https://example.invalid/cfmod".into()
        }
    );
    // Expanded row button -> that row's page.
    s.activate(ModSyncWidgetId::Row(5));
    let [act] = s
        .activate(ModSyncWidgetId::RowCurseForge(5))
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ModSyncUiAction::OpenCurseForgePage {
            mod_id: "cfmod".into(),
            url: "https://example.invalid/cfmod".into()
        }
    );
    // No guided entries -> no footer button and no action.
    let mut plain = sync_fixture();
    plain.entries.retain(|e| e.source != ModSyncSource::CurseForgeGuided);
    let (_, _, ids) = sync_render(&plain, 1280, 720);
    assert!(!ids.contains(&ModSyncWidgetId::CurseForgePage));
    assert!(plain.activate(ModSyncWidgetId::CurseForgePage).is_empty());
}

#[test]
fn modsync_manual_and_incompatible_rows_explain_themselves() {
    let mut s = sync_fixture();
    s.activate(ModSyncWidgetId::Row(3)); // optifine: ManualRequired + Incompatible
    let (list, _, _) = sync_render(&s, 1280, 720);
    let text = joined_text(&list);
    assert!(text.contains("supply this file yourself"), "manual note missing: {text}");
    assert!(text.contains("cannot be used"), "incompatible note missing: {text}");
}

#[test]
fn modsync_scrolls_100_plus_mods_cleanly() {
    let mut entries: Vec<ModSyncDiffEntry> = (0..150)
        .map(|i| {
            let mut e = diff_entry(
                &format!("mod{i}"),
                &format!("Mod Number {i}"),
                match i % 5 {
                    0 => ModSyncDiffKind::Missing,
                    1 => ModSyncDiffKind::VersionMismatch,
                    2 => ModSyncDiffKind::Optional,
                    3 => ModSyncDiffKind::Incompatible,
                    _ => ModSyncDiffKind::Matched,
                },
            );
            e.client_version = Some("0.9.0".into());
            e
        })
        .collect();
    entries.push(diff_entry("tail", "Tail Mod", ModSyncDiffKind::Missing));
    let mut s = ModSyncUiState::new("Big Server", "big", entries);
    for &(w, h) in &SIZES[..2] {
        let (list, rects, ids) = sync_render(&s, w, h);
        assert_commands_in_viewport(&list, w, h);
        assert_regions_non_overlapping(&rects);
        assert!(ids.contains(&ModSyncWidgetId::ListScrollUp));
        assert!(ids.contains(&ModSyncWidgetId::ListScrollDown));
    }
    // Scroll buttons move the window; keyboard wraps to the last row.
    let before = s.scroll_offset();
    s.activate(ModSyncWidgetId::ListScrollDown);
    assert!(s.scroll_offset() > before);
    s.activate(ModSyncWidgetId::ListScrollUp);
    assert_eq!(s.scroll_offset(), before);
    s.handle_input(UiInputEvent::KeyPressed(UiKey::Up), 1280, 720);
    assert_eq!(s.selected_row(), 150);
    // The tail row is now inside the visible window.
    let (_, _, ids) = sync_render(&s, 1280, 720);
    assert!(ids.contains(&ModSyncWidgetId::Row(150)));
    // Paging moves by viewport rows without overshooting the list.
    s.handle_input(UiInputEvent::KeyPressed(UiKey::Left), 1280, 720);
    assert!(s.selected_row() < 150);
    // Expansion in the middle of a long list still renders sanely.
    s.activate(ModSyncWidgetId::Row(40));
    let (list, rects, _) = sync_render(&s, 640, 360);
    assert_commands_in_viewport(&list, 640, 360);
    assert_regions_non_overlapping(&rects);
}

#[test]
fn modsync_empty_diff_shows_join_and_clean_plan() {
    let mut s = ModSyncUiState::new("Empty", "empty", vec![]);
    let (list, _, _) = sync_render(&s, 1280, 720);
    let text = joined_text(&list);
    assert!(text.contains("Nothing to sync"), "{text}");
    assert!(text.contains("Join Server"), "{text}");
    let [act] = s
        .activate(ModSyncWidgetId::ApproveSync)
        .try_into()
        .unwrap();
    assert_eq!(
        act,
        ModSyncUiAction::ApproveAndSync {
            sync_ids: vec![],
            optional_ids: vec![]
        }
    );
}

#[test]
fn modsync_selection_wraps_and_widget_at_hit_tests() {
    let mut s = sync_fixture();
    s.handle_input(UiInputEvent::KeyPressed(UiKey::Down), 1280, 720);
    assert_eq!(s.selected_row(), 1);
    s.handle_input(UiInputEvent::KeyPressed(UiKey::Up), 1280, 720);
    assert_eq!(s.selected_row(), 0);
    s.handle_input(UiInputEvent::KeyPressed(UiKey::Up), 1280, 720);
    assert_eq!(s.selected_row(), s.entries.len() - 1);
    // widget_at matches the emitted regions.
    let (_, hits) = s.render(1280, 720);
    let approve = hits
        .iter()
        .find(|h| h.id == ModSyncWidgetId::ApproveSync)
        .unwrap()
        .rect;
    assert_eq!(
        s.widget_at(
            1280,
            720,
            approve.x + approve.w as i32 / 2,
            approve.y + approve.h as i32 / 2
        ),
        Some(ModSyncWidgetId::ApproveSync)
    );
}

#[test]
fn modsync_has_zero_tofu_in_english_and_japanese() {
    for lang in [UiLang::En, UiLang::Ja] {
        let mut s = sync_fixture();
        s.lang = lang;
        s.activate(ModSyncWidgetId::Row(0));
        s.activate(ModSyncWidgetId::Row(3));
        for &(w, h) in &SIZES {
            let (list, _) = s.render(w, h);
            assert_zero_tofu(&list);
        }
    }
}

#[test]
fn modsync_japanese_labels_render() {
    let mut s = sync_fixture();
    s.lang = UiLang::Ja;
    let (list, _, _) = sync_render(&s, 1280, 720);
    let text = joined_text(&list);
    assert!(text.contains("サーバーMod同期"), "ja title missing: {text}");
    assert!(text.contains("承認して同期"), "ja approve missing: {text}");
    assert!(text.contains("拒否して切断"), "ja decline missing: {text}");
    assert!(text.contains("変更されません"), "ja isolation missing: {text}");
    for badge in ["追加", "更新", "オプション", "利用不可", "一致"] {
        assert!(text.contains(badge), "ja badge {badge} missing: {text}");
    }
}

#[test]
fn modsync_hostile_strings_cannot_overflow_rows() {
    let mut evil = diff_entry("evil", "Evil\nMod\tName With A Very Long Title", ModSyncDiffKind::Missing);
    evil.summary_en = "Bad\nText\0Here ".repeat(20);
    evil.license_spdx = "MIT\nOR\nEVIL".into();
    let mut s = ModSyncUiState::new("Srv", "srv", vec![evil]);
    s.activate(ModSyncWidgetId::Row(0));
    for &(w, h) in &SIZES {
        let (list, rects, _) = sync_render(&s, w, h);
        assert_commands_in_viewport(&list, w, h);
        assert_regions_non_overlapping(&rects);
    }
}
