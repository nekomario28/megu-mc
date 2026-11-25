// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Tests for the v0.1.0-alpha.4 visual chunk-claim map and per-player
//! permissions screen (`claims_ui`).

use megu_client_ui::claims_ui::{
    ChunkClaimState, ClaimChunkCell, ClaimCostPreview, ClaimPermission, ClaimsHitRegion, ClaimsTab,
    ClaimsUiAction, ClaimsUiState, ClaimsWidgetId, PlayerTrustEntry, TrustPermissions,
    CLAIMS_GRID_DIM,
};
use megu_client_ui::draw::{glyph_is_covered, UiCommand, UiRect};
use megu_client_ui::state::{GamepadButton, UiInputEvent, UiKey, UiLang};

const W: u32 = 640;
const H: u32 = 360;
const SIZES: [(u32, u32); 2] = [(640, 360), (1280, 720)];

fn texts(list: &megu_client_ui::draw::UiDrawList) -> Vec<String> {
    list.commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn any_text_contains(list: &megu_client_ui::draw::UiDrawList, needle: &str) -> bool {
    texts(list).iter().any(|t| t.contains(needle))
}

fn region_for(
    hits: &[ClaimsHitRegion],
    pred: impl Fn(ClaimsWidgetId) -> bool,
) -> Option<&ClaimsHitRegion> {
    hits.iter().find(|h| pred(h.id))
}

fn cell_region(hits: &[ClaimsHitRegion], i: u8) -> UiRect {
    region_for(hits, |id| id == ClaimsWidgetId::ChunkCell(i))
        .unwrap_or_else(|| panic!("no hit region for cell {i}"))
        .rect
}

fn center_of(r: UiRect) -> (i32, i32) {
    (r.x + r.w as i32 / 2, r.y + r.h as i32 / 2)
}

fn roster() -> Vec<PlayerTrustEntry> {
    let mut a = PlayerTrustEntry::new("uuid-alpha-0001", "Alpha");
    a.online = true;
    a.permissions.containers = true;
    let mut b = PlayerTrustEntry::new("uuid-bravo-0002", "Bravo");
    b.permissions = TrustPermissions::all_allowed();
    let c = PlayerTrustEntry::new("uuid-charlie-3", "charlie");
    vec![a, b, c]
}

// ---------------------------------------------------------------------------
// Map rendering
// ---------------------------------------------------------------------------

#[test]
fn map_renders_81_cells_centered_on_player() {
    let mut st = ClaimsUiState::new();
    st.set_center(100, -40);
    let (_, hits) = st.render(W, H);
    let cells: Vec<_> = hits
        .iter()
        .filter(|h| matches!(h.id, ClaimsWidgetId::ChunkCell(_)))
        .collect();
    assert_eq!(cells.len(), CLAIMS_GRID_DIM * CLAIMS_GRID_DIM);
    // Grid index 40 (center of 9x9) maps to the player's chunk.
    assert_eq!(st.grid_coord(40), Some((100, -40)));
    // Corners of the grid are center +/- 4.
    assert_eq!(st.grid_coord(0), Some((96, -44)));
    assert_eq!(st.grid_coord(80), Some((104, -36)));
    assert_eq!(st.grid_index(100, -40), Some(40));
    assert_eq!(st.grid_index(96, -44), Some(0));
    assert_eq!(st.grid_index(95, -40), None);
}

#[test]
fn map_shows_compass_labels_and_player_marker() {
    let st = ClaimsUiState::new();
    let (list, _) = st.render(W, H);
    let t = texts(&list);
    for label in ["N", "E", "S", "W", "@"] {
        assert!(t.iter().any(|s| s == label), "missing map marker {label:?}");
    }
}

#[test]
fn chunk_states_have_distinct_accessible_colors() {
    let states = [
        ChunkClaimState::Unclaimed,
        ChunkClaimState::OwnedBySelf,
        ChunkClaimState::Trusted,
        ChunkClaimState::OwnedByOther("x".into()),
        ChunkClaimState::SpawnProtected,
    ];
    let colors: Vec<_> = states.iter().map(|s| s.fill_color()).collect();
    for i in 0..colors.len() {
        for j in i + 1..colors.len() {
            assert_ne!(colors[i], colors[j], "states {i}/{j} share a color");
        }
    }
    // And the map actually paints a claimed cell in its state color:
    // unclaimed -> own produces a different fill at the same cell.
    let mut st = ClaimsUiState::new();
    let cell_paint = |st: &ClaimsUiState| {
        let (list, hits) = st.render(W, H);
        let r = cell_region(&hits, 40);
        list.commands
            .iter()
            .filter_map(|c| match c {
                UiCommand::FillRect { rect, color } if *rect == r => Some(*color),
                _ => None,
            })
            .next_back()
    };
    let unclaimed = cell_paint(&st).expect("cell has a fill");
    st.chunks.insert(
        (0, 0),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: false,
            sub_claim: None,
        },
    );
    let owned = cell_paint(&st).expect("cell has a fill");
    assert_eq!(unclaimed, ChunkClaimState::Unclaimed.fill_color());
    assert_eq!(owned, ChunkClaimState::OwnedBySelf.fill_color());
    assert_ne!(unclaimed, owned);
}

// ---------------------------------------------------------------------------
// Claim / unclaim actions
// ---------------------------------------------------------------------------

#[test]
fn left_click_unclaimed_cell_emits_claim() {
    let mut st = ClaimsUiState::new();
    st.set_center(10, 20);
    let (_, hits) = st.render(W, H);
    let r = cell_region(&hits, 17); // row 1, col 8 -> (14, 17)
    let (x, y) = center_of(r);
    let acts = st.handle_input(UiInputEvent::Click { x, y }, W, H);
    assert_eq!(
        acts,
        vec![ClaimsUiAction::ClaimChunk { cx: 14, cz: 17 }],
        "left click on an unclaimed cell must emit ClaimChunk"
    );
}

#[test]
fn left_click_owned_cell_selects_without_unclaim() {
    let mut st = ClaimsUiState::new();
    st.chunks.insert(
        (1, 1),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: false,
            sub_claim: None,
        },
    );
    let idx = st.grid_index(1, 1).unwrap();
    let (_, hits) = st.render(W, H);
    let (x, y) = center_of(cell_region(&hits, idx));
    let acts = st.handle_input(UiInputEvent::Click { x, y }, W, H);
    assert!(
        acts.is_empty(),
        "left click on own claim only focuses the inspector"
    );
    assert_eq!(st.focus_chunk(), (1, 1));
}

#[test]
fn right_click_own_chunk_unclaims() {
    let mut st = ClaimsUiState::new();
    st.chunks.insert(
        (2, -1),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: false,
            sub_claim: None,
        },
    );
    let idx = st.grid_index(2, -1).unwrap();
    let (_, hits) = st.render(W, H);
    let (x, y) = center_of(cell_region(&hits, idx));
    let acts = st.secondary_click_at(x, y, W, H);
    assert_eq!(acts, vec![ClaimsUiAction::UnclaimChunk { cx: 2, cz: -1 }]);
}

#[test]
fn right_click_foreign_or_free_chunk_does_not_unclaim() {
    let mut st = ClaimsUiState::new();
    st.chunks.insert(
        (0, 0),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedByOther("Griefer".into()),
            forceloaded: false,
            sub_claim: None,
        },
    );
    let own_idx = st.grid_index(0, 0).unwrap();
    let free_idx = st.grid_index(3, 3).unwrap();
    let (_, hits) = st.render(W, H);
    for idx in [own_idx, free_idx] {
        let (x, y) = center_of(cell_region(&hits, idx));
        assert!(
            st.secondary_click_at(x, y, W, H).is_empty(),
            "cell {idx} must not emit UnclaimChunk"
        );
    }
}

#[test]
fn inspector_unclaim_button_unclaims_focused_chunk() {
    let mut st = ClaimsUiState::new();
    st.chunks.insert(
        (0, 0),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: false,
            sub_claim: None,
        },
    );
    st.cursor = (0, 0);
    let (_, hits) = st.render(W, H);
    let r = region_for(&hits, |id| id == ClaimsWidgetId::UnclaimFocused)
        .expect("unclaim button")
        .rect;
    let (x, y) = center_of(r);
    let acts = st.handle_input(UiInputEvent::Click { x, y }, W, H);
    assert_eq!(acts, vec![ClaimsUiAction::UnclaimChunk { cx: 0, cz: 0 }]);
}

#[test]
fn inspector_claim_button_claims_focused_chunk() {
    let mut st = ClaimsUiState::new();
    st.cursor = (1, 0);
    let (_, hits) = st.render(W, H);
    let r = region_for(&hits, |id| id == ClaimsWidgetId::ClaimFocused)
        .expect("claim button")
        .rect;
    let (x, y) = center_of(r);
    let acts = st.handle_input(UiInputEvent::Click { x, y }, W, H);
    assert_eq!(acts, vec![ClaimsUiAction::ClaimChunk { cx: 1, cz: 0 }]);
}

// ---------------------------------------------------------------------------
// Inspector / header / cost preview
// ---------------------------------------------------------------------------

#[test]
fn hover_inspector_shows_coords_and_owner() {
    let mut st = ClaimsUiState::new();
    st.chunks.insert(
        (-2, 3),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedByOther("Neighbor_Ned".into()),
            forceloaded: false,
            sub_claim: None,
        },
    );
    let idx = st.grid_index(-2, 3).unwrap();
    let (_, hits) = st.render(W, H);
    let (x, y) = center_of(cell_region(&hits, idx));
    st.handle_input(UiInputEvent::MouseMove { x, y }, W, H);
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "Chunk (-2, 3)"), "coords missing");
    assert!(
        any_text_contains(&list, "Neighbor_Ned"),
        "owner name missing"
    );
}

#[test]
fn inspector_shows_free_badge_then_dynamic_cost() {
    let mut st = ClaimsUiState::new();
    st.cursor = (0, 0);
    // Starter-free: first chunk costs nothing.
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "FREE!"), "free badge missing");
    // Past the allowance, the default point price shows instead.
    st.claims_used = 5;
    st.starter_free_chunks = 1;
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "100 pt"), "points cost missing");
    // An explicit quote wins over the default, and XP pricing renders.
    st.cost_quotes.insert((0, 0), ClaimCostPreview::XpLevels(3));
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "XP 3"), "xp cost missing");
}

#[test]
fn header_shows_claims_used_max_dimension_and_free_badge() {
    let mut st = ClaimsUiState::new();
    st.claims_max = 42;
    st.dimension = "minecraft:the_nether".into();
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "Claims Used: 0 / 42"));
    assert!(any_text_contains(&list, "the_nether"));
    assert!(any_text_contains(&list, "first chunk free"));
    st.claims_used = 7;
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "Claims Used: 7 / 42"));
    assert!(
        !any_text_contains(&list, "first chunk free"),
        "free badge must hide once the freebie is spent"
    );
}

#[test]
fn inspector_shows_forceload_and_subclaim_flags() {
    let mut st = ClaimsUiState::new();
    st.chunks.insert(
        (0, 0),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: true,
            sub_claim: Some("farm".into()),
        },
    );
    st.cursor = (0, 0);
    let (list, _) = st.render(W, H);
    assert!(any_text_contains(&list, "forceload"));
    assert!(any_text_contains(&list, "sub:farm"));
}

// ---------------------------------------------------------------------------
// Trusted Players tab
// ---------------------------------------------------------------------------

#[test]
fn trusted_tab_renders_search_and_roster() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    let acts = st.activate(ClaimsWidgetId::TabTrusted);
    assert!(acts.is_empty());
    assert_eq!(st.tab, ClaimsTab::Trusted);
    let (_, hits) = st.render(W, H);
    assert!(region_for(&hits, |id| id == ClaimsWidgetId::SearchField).is_some());
    let rows: Vec<_> = hits
        .iter()
        .filter(|h| matches!(h.id, ClaimsWidgetId::PlayerRow(_)))
        .collect();
    assert_eq!(rows.len(), 3, "all roster rows render");
    // Six granular toggles for the selected player.
    let toggles: Vec<_> = hits
        .iter()
        .filter(|h| matches!(h.id, ClaimsWidgetId::PermToggle(_)))
        .collect();
    assert_eq!(toggles.len(), 6);
    assert!(region_for(&hits, |id| id == ClaimsWidgetId::RevokeSelected).is_some());
}

#[test]
fn search_filters_player_roster() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    st.handle_input(UiInputEvent::CharTyped('a'), W, H);
    st.handle_input(UiInputEvent::CharTyped('l'), W, H);
    assert_eq!(st.search, "al");
    let vis = st.visible_players();
    assert_eq!(vis.len(), 1, "only 'Alpha' matches 'al'");
    assert_eq!(st.known_players[vis[0]].name, "Alpha");
}

#[test]
fn perm_toggle_emits_set_player_permission() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    // "Alpha" sorts first; it has containers=true, build=false.
    let acts = st.activate(ClaimsWidgetId::PermToggle(0));
    assert_eq!(
        acts,
        vec![ClaimsUiAction::SetPlayerPermission {
            player_uuid: "uuid-alpha-0001".into(),
            permission: ClaimPermission::Build,
            allowed: true,
        }]
    );
    // Containers was already allowed -> toggles off.
    let acts = st.activate(ClaimsWidgetId::PermToggle(1));
    assert_eq!(
        acts,
        vec![ClaimsUiAction::SetPlayerPermission {
            player_uuid: "uuid-alpha-0001".into(),
            permission: ClaimPermission::Containers,
            allowed: false,
        }]
    );
}

#[test]
fn revoke_button_emits_revoke_player() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    let (_, hits) = st.render(W, H);
    let r = region_for(&hits, |id| id == ClaimsWidgetId::RevokeSelected)
        .expect("revoke button")
        .rect;
    let (x, y) = center_of(r);
    let acts = st.handle_input(UiInputEvent::Click { x, y }, W, H);
    assert_eq!(
        acts,
        vec![ClaimsUiAction::RevokePlayer {
            player_uuid: "uuid-alpha-0001".into()
        }]
    );
}

#[test]
fn selecting_a_player_row_targets_its_permissions() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    let (_, hits) = st.render(W, H);
    // Row 1 in visible order is "Bravo" (Alpha, Bravo, charlie sorted).
    let r = region_for(&hits, |id| id == ClaimsWidgetId::PlayerRow(1))
        .expect("row 1")
        .rect;
    let (x, y) = center_of(r);
    st.handle_input(UiInputEvent::Click { x, y }, W, H);
    let acts = st.activate(ClaimsWidgetId::PermToggle(5));
    assert_eq!(
        acts,
        vec![ClaimsUiAction::SetPlayerPermission {
            player_uuid: "uuid-bravo-0002".into(),
            permission: ClaimPermission::Explosions,
            allowed: false, // Bravo had all six on -> toggles off
        }]
    );
}

#[test]
fn empty_roster_shows_placeholder_not_crash() {
    let mut st = ClaimsUiState::new();
    st.tab = ClaimsTab::Trusted;
    let (list, hits) = st.render(W, H);
    assert!(any_text_contains(&list, "no players"));
    // Toggles/selection are inert without a selection.
    assert!(st.activate(ClaimsWidgetId::PermToggle(0)).is_empty());
    assert!(st.activate(ClaimsWidgetId::RevokeSelected).is_empty());
    let _ = hits;
}

// ---------------------------------------------------------------------------
// Keyboard / gamepad navigation
// ---------------------------------------------------------------------------

#[test]
fn keyboard_arrows_move_map_cursor_clamped_to_grid() {
    let mut st = ClaimsUiState::new();
    st.set_center(0, 0);
    for _ in 0..3 {
        st.handle_input(UiInputEvent::KeyPressed(UiKey::Right), W, H);
    }
    assert_eq!(st.cursor, (3, 0));
    for _ in 0..9 {
        st.handle_input(UiInputEvent::KeyPressed(UiKey::Right), W, H);
    }
    assert_eq!(st.cursor, (4, 0), "cursor clamps at the grid edge");
    for _ in 0..9 {
        st.handle_input(UiInputEvent::KeyPressed(UiKey::Down), W, H);
    }
    assert_eq!(st.cursor, (4, 4));
}

#[test]
fn keyboard_enter_claims_and_backspace_unclaims() {
    let mut st = ClaimsUiState::new();
    st.cursor = (1, 1);
    let acts = st.handle_input(UiInputEvent::KeyPressed(UiKey::Enter), W, H);
    assert_eq!(acts, vec![ClaimsUiAction::ClaimChunk { cx: 1, cz: 1 }]);
    // Once claimed, Enter no longer reclaims; Backspace unclaims it.
    st.chunks.insert(
        (1, 1),
        ClaimChunkCell {
            state: ChunkClaimState::OwnedBySelf,
            forceloaded: false,
            sub_claim: None,
        },
    );
    assert!(st
        .handle_input(UiInputEvent::KeyPressed(UiKey::Enter), W, H)
        .is_empty());
    let acts = st.handle_input(UiInputEvent::KeyPressed(UiKey::Backspace), W, H);
    assert_eq!(acts, vec![ClaimsUiAction::UnclaimChunk { cx: 1, cz: 1 }]);
}

#[test]
fn gamepad_dpad_south_and_east_work() {
    let mut st = ClaimsUiState::new();
    st.set_center(0, 0);
    st.handle_input(UiInputEvent::GamepadPressed(GamepadButton::DpadRight), W, H);
    assert_eq!(st.cursor, (1, 0));
    let acts = st.handle_input(UiInputEvent::GamepadPressed(GamepadButton::South), W, H);
    assert_eq!(acts, vec![ClaimsUiAction::ClaimChunk { cx: 1, cz: 0 }]);
    let acts = st.handle_input(UiInputEvent::GamepadPressed(GamepadButton::East), W, H);
    assert_eq!(acts, vec![ClaimsUiAction::Close]);
}

#[test]
fn keyboard_trusted_tab_navigation() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    st.handle_input(UiInputEvent::KeyPressed(UiKey::Down), W, H);
    assert_eq!(st.selected_player, 1);
    st.handle_input(UiInputEvent::KeyPressed(UiKey::Right), W, H);
    st.handle_input(UiInputEvent::KeyPressed(UiKey::Right), W, H);
    assert_eq!(st.perm_selected, 2);
    // Enter toggles the focused permission for the selected player.
    let acts = st.handle_input(UiInputEvent::KeyPressed(UiKey::Enter), W, H);
    assert_eq!(
        acts,
        vec![ClaimsUiAction::SetPlayerPermission {
            player_uuid: "uuid-bravo-0002".into(),
            permission: ClaimPermission::DoorsAndSwitches,
            allowed: false,
        }]
    );
}

#[test]
fn backspace_on_empty_search_revokes_selected_player() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    let acts = st.handle_input(UiInputEvent::KeyPressed(UiKey::Backspace), W, H);
    assert_eq!(
        acts,
        vec![ClaimsUiAction::RevokePlayer {
            player_uuid: "uuid-alpha-0001".into()
        }]
    );
    // With text in the box, Backspace edits text first.
    st.search = "ab".into();
    assert!(st
        .handle_input(UiInputEvent::KeyPressed(UiKey::Backspace), W, H)
        .is_empty());
    assert_eq!(st.search, "a");
}

#[test]
fn tab_key_switches_tabs() {
    let mut st = ClaimsUiState::new();
    st.handle_input(UiInputEvent::KeyPressed(UiKey::Tab), W, H);
    assert_eq!(st.tab, ClaimsTab::Trusted);
    st.handle_input(UiInputEvent::KeyPressed(UiKey::Tab), W, H);
    assert_eq!(st.tab, ClaimsTab::Map);
}

#[test]
fn escape_clears_search_then_closes() {
    let mut st = ClaimsUiState::new();
    st.tab = ClaimsTab::Trusted;
    st.search = "abc".into();
    assert!(st
        .handle_input(UiInputEvent::KeyPressed(UiKey::Escape), W, H)
        .is_empty());
    assert!(st.search.is_empty());
    let acts = st.handle_input(UiInputEvent::KeyPressed(UiKey::Escape), W, H);
    assert_eq!(acts, vec![ClaimsUiAction::Close]);
}

// ---------------------------------------------------------------------------
// Layout / accessibility guarantees
// ---------------------------------------------------------------------------

#[test]
fn hit_regions_never_overlap_on_either_tab_or_size() {
    for &(w, h) in &SIZES {
        for tab in [ClaimsTab::Map, ClaimsTab::Trusted] {
            let mut st = ClaimsUiState::new();
            st.known_players = roster();
            st.tab = tab;
            let (_, hits) = st.render(w, h);
            for (i, a) in hits.iter().enumerate() {
                for b in hits.iter().skip(i + 1) {
                    assert!(
                        !a.rect.intersects(b.rect),
                        "{tab:?} @{w}x{h}: {:?} overlaps {:?}",
                        a.id,
                        b.id
                    );
                }
            }
        }
    }
}

#[test]
fn hit_regions_stay_inside_the_frame_on_both_sizes() {
    for &(w, h) in &SIZES {
        for tab in [ClaimsTab::Map, ClaimsTab::Trusted] {
            let mut st = ClaimsUiState::new();
            st.known_players = roster();
            st.tab = tab;
            let (_, hits) = st.render(w, h);
            assert!(!hits.is_empty(), "{tab:?} @{w}x{h} emitted no regions");
            for r in &hits {
                assert!(
                    r.rect.x >= 0 && r.rect.y >= 0,
                    "{tab:?} region {r:?} escapes left/top"
                );
                assert!(
                    r.rect.right() <= w as i32,
                    "{tab:?} region {r:?} escapes right edge"
                );
                assert!(
                    r.rect.bottom() <= h as i32,
                    "{tab:?} region {r:?} escapes bottom edge"
                );
            }
        }
    }
}

#[test]
fn every_rendered_ja_glyph_has_font_coverage() {
    let mut missing = std::collections::BTreeSet::new();
    for &(w, h) in &SIZES {
        for tab in [ClaimsTab::Map, ClaimsTab::Trusted] {
            let mut st = ClaimsUiState::new();
            st.lang = UiLang::Ja;
            st.tab = tab;
            st.known_players = roster();
            st.chunks.insert(
                (0, 0),
                ClaimChunkCell {
                    state: ChunkClaimState::OwnedBySelf,
                    forceloaded: true,
                    sub_claim: Some("farm".into()),
                },
            );
            st.chunks.insert(
                (1, 1),
                ClaimChunkCell {
                    state: ChunkClaimState::OwnedByOther("ほかの人".into()),
                    forceloaded: false,
                    sub_claim: None,
                },
            );
            let (list, hits) = st.render(w, h);
            assert!(!hits.is_empty());
            let mut texts_seen = 0usize;
            for cmd in &list.commands {
                if let UiCommand::Text { text, .. } = cmd {
                    texts_seen += 1;
                    for c in text.chars() {
                        if !glyph_is_covered(c) {
                            missing.insert(c);
                        }
                    }
                }
            }
            assert!(texts_seen > 0, "{tab:?} @{w}x{h} emitted no text");
        }
    }
    assert!(
        missing.is_empty(),
        "ja screen emits uncovered (tofu) glyphs: {missing:?}"
    );
}

#[test]
fn bilingual_labels_switch_cleanly() {
    let mut st = ClaimsUiState::new();
    let (en, _) = st.render(W, H);
    st.lang = UiLang::Ja;
    let (ja, _) = st.render(W, H);
    assert!(any_text_contains(&en, "Chunk Claims"));
    assert!(any_text_contains(&ja, "クレーム管理"));
    assert!(any_text_contains(&en, "Chunk Map"));
    assert!(any_text_contains(&ja, "マップ"));
    st.tab = ClaimsTab::Trusted;
    st.known_players = roster();
    let (ja2, _) = st.render(W, H);
    assert!(any_text_contains(&ja2, "すべての許可を削除"));
}

// ---------------------------------------------------------------------------
// Determinism / ordering
// ---------------------------------------------------------------------------

#[test]
fn roster_order_is_deterministic_by_name() {
    let mut st = ClaimsUiState::new();
    let mut z = PlayerTrustEntry::new("uuid-z", "zeta");
    z.online = true;
    let mut a = PlayerTrustEntry::new("uuid-a", "apple");
    a.online = false;
    let m = PlayerTrustEntry::new("uuid-m", "Mango");
    st.known_players = vec![z, a, m];
    st.tab = ClaimsTab::Trusted;
    let names: Vec<_> = st
        .visible_players()
        .iter()
        .map(|&i| st.known_players[i].name.as_str())
        .collect();
    assert_eq!(names, ["apple", "Mango", "zeta"]);
}

#[test]
fn selection_clamps_when_filter_shrinks_the_roster() {
    let mut st = ClaimsUiState::new();
    st.known_players = roster();
    st.tab = ClaimsTab::Trusted;
    st.selected_player = 2;
    for c in "alp".chars() {
        st.handle_input(UiInputEvent::CharTyped(c), W, H);
    }
    assert_eq!(st.visible_players().len(), 1);
    assert_eq!(
        st.selected_player, 0,
        "selection clamps into the filtered list"
    );
    assert_eq!(st.selected_player_entry().unwrap().name, "Alpha");
}
