// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::HashMap;

use megu_client::protocol::{NbtValue, encode_slot_stack};
use megu_client_gpu::window::{
    book_hotbar_slot, decode_merchant_result_slot, inventory_key_matches, should_send_sign_update,
    sign_face_lines, title_panorama_is_visible,
};
use megu_ui::state::{KeyBindings, Screen};
use winit::keyboard::KeyCode;

#[test]
fn book_menu_slots_map_to_protocol_hotbar_indices() {
    for (menu, hotbar) in (36..=44).zip(0..=8) {
        assert_eq!(book_hotbar_slot(menu), Some(hotbar));
    }
    assert_eq!(book_hotbar_slot(35), None);
    assert_eq!(book_hotbar_slot(45), None);
}

#[test]
fn sign_editor_initial_lines_read_front_and_back_text() {
    let face = |messages: [&str; 4]| {
        let mut face = HashMap::new();
        face.insert(
            "messages".to_owned(),
            NbtValue::List(
                messages
                    .into_iter()
                    .map(|text| NbtValue::String(format!(r#"{{"text":"{text}"}}"#)))
                    .collect(),
            ),
        );
        NbtValue::Compound(face)
    };
    let mut root = HashMap::new();
    root.insert(
        "front_text".to_owned(),
        face(["front 1", "front 2", "", "front 4"]),
    );
    root.insert(
        "back_text".to_owned(),
        face(["back 1", "", "back 3", "back 4"]),
    );
    let root = NbtValue::Compound(root);
    assert_eq!(
        sign_face_lines(&root, true).unwrap(),
        ["front 1", "front 2", "", "front 4"].map(str::to_owned)
    );
    assert_eq!(
        sign_face_lines(&root, false).unwrap(),
        ["back 1", "", "back 3", "back 4"].map(str::to_owned)
    );
}

#[test]
fn sign_escape_discards_without_sending_unchanged_lines() {
    assert!(!should_send_sign_update(false));
    assert!(should_send_sign_update(true));
}

#[test]
fn protocol_767_merchant_result_decodes_item_and_exact_count() {
    let wire = encode_slot_stack(123, 7);
    assert_eq!(decode_merchant_result_slot(&wire), Some((123, 7)));
    assert_eq!(decode_merchant_result_slot(&[]), None);
}

#[test]
fn rebound_inventory_key_disables_the_old_e_key() {
    let bindings = KeyBindings {
        inventory: "KeyI".to_owned(),
        ..KeyBindings::default()
    };
    assert!(!inventory_key_matches(KeyCode::KeyE, &bindings));
    assert!(inventory_key_matches(KeyCode::KeyI, &bindings));
}

#[test]
fn title_panorama_visibility_tracks_screen_and_upload_state() {
    assert!(title_panorama_is_visible(Screen::Title, true));
    assert!(!title_panorama_is_visible(Screen::Title, false));
    assert!(!title_panorama_is_visible(Screen::Pause, true));
}
