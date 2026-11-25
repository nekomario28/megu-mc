// SPDX-License-Identifier: LGPL-3.0-or-later
//! Wire-level contracts used by the live-session workstation bridge.

use megu_client::protocol::{
    encode_container_button_click, encode_container_click, encode_container_slot_state_changed,
    encode_rename_item, encode_select_trade, encode_set_beacon,
};

#[test]
fn workstation_packets_encode_the_protocol_767_actions() {
    assert_eq!(encode_rename_item("Sharp axe"), ("rename_item", b"\x09Sharp axe".to_vec()));
    assert_eq!(encode_select_trade(2), ("select_trade", vec![2]));
    assert_eq!(encode_set_beacon(Some(5), Some(12)), ("set_beacon", vec![1, 5, 1, 12]));
    assert_eq!(
        encode_container_slot_state_changed(8, 4, true),
        ("container_slot_state_changed", vec![8, 4, 1]),
    );
    assert_eq!(
        encode_container_button_click(7, 2),
        ("container_button_click", vec![7, 2]),
    );
}

#[test]
fn container_click_modes_cover_mouse_and_keyboard_actions() {
    for (button, mode) in [
        (0, 0), // left click
        (1, 0), // right click
        (0, 1), // shift click
        (0, 2), // number-key swap
        (0, 4), // drop
        (0, 5), // drag
        (0, 6), // double-click collect
    ] {
        let (name, payload) = encode_container_click(3, 11, 5, button, mode, &[], &[0]);
        assert_eq!(name, "container_click");
        assert!(!payload.is_empty(), "mode {mode} must produce a packet body");
    }
}
