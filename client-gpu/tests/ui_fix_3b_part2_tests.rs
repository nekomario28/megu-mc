// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client::protocol::{ItemComponentPatch, ItemStackComponents};
use megu_client_gpu::window::{stack_shows_enchant_glint, vanilla_creative_catalog};
use megu_ui::gameplay_screens::BookEditorState;

#[test]
fn creative_catalog_comes_from_registry_and_is_not_inventory_dependent() {
    let catalog = vanilla_creative_catalog();
    assert!(catalog.len() > 500);
    assert!(
        catalog
            .iter()
            .any(|item| item.name.eq_ignore_ascii_case("stone"))
    );
    assert!(
        catalog
            .iter()
            .all(|item| item.count == 1 && item.item_id >= 0)
    );
}

#[test]
fn explicit_false_glint_override_suppresses_enchantments() {
    let stack = ItemStackComponents {
        count: 1,
        item_id: 1,
        added: vec![
            ItemComponentPatch {
                type_id: 9,
                data: vec![1],
            },
            ItemComponentPatch {
                type_id: 18,
                data: vec![0],
            },
        ],
        removed: Vec::new(),
    };
    assert!(!stack_shows_enchant_glint(&stack));
}

#[test]
fn book_editor_accepts_body_s_and_newline_as_text() {
    let mut book = BookEditorState::new(36);
    book.type_char('s');
    book.newline();
    book.type_char('x');
    assert_eq!(book.pages[0], "s\nx");
}
