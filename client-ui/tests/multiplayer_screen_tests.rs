// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! The Multiplayer server-select/direct-connect screen and the
//! Disconnected kick-reason screen: Title `Multiplayer` opens the
//! browser, typed `host[:port]` input wins over the selected row,
//! Join/Add/Delete emit the matching `UiAction`s for the host's
//! `ServerListStore`, and a dropped session lands on
//! `Screen::Disconnected` with the verbatim reason plus a title path.

use megu_client_ui::draw::UiCommand;
use megu_client_ui::state::{
    parse_server_address, DisconnectedScreenState, MultiplayerServerEntry, Screen, UiAction,
    UiInputEvent, UiKey, UiState, WidgetId,
};

/// Render then click the center of `id`'s hit region.
fn click(ui: &mut UiState, id: WidgetId, w: u32, h: u32) -> Vec<UiAction> {
    ui.render(w, h);
    let r = ui
        .hit_regions
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no hit region for {id:?} on {:?}", ui.screen))
        .rect;
    ui.handle_input(UiInputEvent::Click {
        x: r.x + r.w as i32 / 2,
        y: r.y + r.h as i32 / 2,
    })
}

fn type_str(ui: &mut UiState, s: &str) {
    for c in s.chars() {
        ui.handle_input(UiInputEvent::CharTyped(c));
    }
}

fn server(name: &str, address: &str, port: u16) -> MultiplayerServerEntry {
    MultiplayerServerEntry {
        name: name.to_string(),
        address: address.to_string(),
        port,
    }
}

#[test]
fn parse_server_address_accepts_all_forms() {
    assert_eq!(
        parse_server_address("localhost"),
        Some(("localhost".to_string(), 25565))
    );
    assert_eq!(
        parse_server_address("play.example.org:25566"),
        Some(("play.example.org".to_string(), 25566))
    );
    assert_eq!(
        parse_server_address("  mc.net  "),
        Some(("mc.net".to_string(), 25565))
    );
    assert_eq!(
        parse_server_address("[::1]"),
        Some(("::1".to_string(), 25565))
    );
    assert_eq!(
        parse_server_address("[2001:db8::1]:25570"),
        Some(("2001:db8::1".to_string(), 25570))
    );
    // Unbracketed multi-colon text reads as a bare IPv6 literal.
    assert_eq!(
        parse_server_address("fe80::1"),
        Some(("fe80::1".to_string(), 25565))
    );
}

#[test]
fn parse_server_address_rejects_bad_input() {
    assert_eq!(parse_server_address(""), None);
    assert_eq!(parse_server_address("   "), None);
    assert_eq!(parse_server_address(":25565"), None);
    assert_eq!(parse_server_address("host:0"), None);
    assert_eq!(parse_server_address("host:abc"), None);
    assert_eq!(parse_server_address("host:"), None);
    assert_eq!(parse_server_address("[]:25565"), None);
    assert_eq!(parse_server_address("[::1]x"), None);
    assert_eq!(parse_server_address("[::1]:99999"), None);
    assert_eq!(parse_server_address("[unclosed"), None);
}

#[test]
fn title_multiplayer_opens_server_browser() {
    let mut ui = UiState::new();
    let acts = click(&mut ui, WidgetId::TitleMultiplayer, 1280, 720);
    assert_eq!(ui.screen, Screen::Multiplayer);
    assert_eq!(acts, vec![UiAction::OpenScreen(Screen::Multiplayer)]);
}

#[test]
fn selected_row_join_emits_join_server() {
    let mut ui = UiState::new();
    ui.multiplayer.servers = vec![
        server("Alpha", "alpha.example.org", 25565),
        server("Beta", "10.0.0.2", 30000),
    ];
    ui.navigate(Screen::Multiplayer);

    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerServer(1), 1280, 720),
        Vec::<UiAction>::new()
    );
    assert_eq!(ui.multiplayer.selected, Some(1));
    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerJoin, 1280, 720),
        vec![UiAction::JoinServer {
            address: "10.0.0.2".to_string(),
            port: 30000
        }]
    );
}

#[test]
fn typed_direct_connect_wins_over_selection() {
    let mut ui = UiState::new();
    ui.multiplayer.servers = vec![server("Saved", "saved.example.org", 25565)];
    ui.navigate(Screen::Multiplayer);
    click(&mut ui, WidgetId::MultiplayerServer(0), 1280, 720);
    click(&mut ui, WidgetId::MultiplayerAddressField, 1280, 720);
    type_str(&mut ui, "direct.example.org:25570");

    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerJoin, 1280, 720),
        vec![UiAction::JoinServer {
            address: "direct.example.org".to_string(),
            port: 25570
        }]
    );
    assert_eq!(ui.multiplayer.status, None);
}

#[test]
fn join_without_target_shows_status() {
    let mut ui = UiState::new();
    ui.navigate(Screen::Multiplayer);
    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerJoin, 1280, 720),
        Vec::<UiAction>::new()
    );
    assert!(ui.multiplayer.status.is_some());
}

#[test]
fn invalid_typed_address_shows_status_and_no_action() {
    let mut ui = UiState::new();
    ui.navigate(Screen::Multiplayer);
    click(&mut ui, WidgetId::MultiplayerAddressField, 1280, 720);
    type_str(&mut ui, "host:0");

    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerJoin, 1280, 720),
        Vec::<UiAction>::new()
    );
    assert!(ui.multiplayer.status.is_some());
}

#[test]
fn add_button_emits_save_server() {
    let mut ui = UiState::new();
    ui.navigate(Screen::Multiplayer);
    click(&mut ui, WidgetId::MultiplayerAddressField, 1280, 720);
    type_str(&mut ui, "new.example.org");

    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerAdd, 1280, 720),
        vec![UiAction::SaveServer {
            name: "new.example.org".to_string(),
            address: "new.example.org".to_string(),
            port: 25565
        }]
    );
}

#[test]
fn delete_requires_selection_then_emits_index() {
    let mut ui = UiState::new();
    ui.multiplayer.servers = vec![server("Only", "only.example.org", 25565)];
    ui.navigate(Screen::Multiplayer);

    // No selection: nothing emitted, status explains why.
    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerDelete, 1280, 720),
        Vec::<UiAction>::new()
    );
    assert!(ui.multiplayer.status.is_some());

    click(&mut ui, WidgetId::MultiplayerServer(0), 1280, 720);
    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerDelete, 1280, 720),
        vec![UiAction::DeleteServer { index: 0 }]
    );
}

#[test]
fn multiplayer_back_emits_back() {
    let mut ui = UiState::new();
    ui.navigate(Screen::Multiplayer);
    assert_eq!(
        click(&mut ui, WidgetId::MultiplayerBack, 1280, 720),
        vec![UiAction::Back]
    );
    assert_eq!(ui.screen, Screen::Title);
}

#[test]
fn multiplayer_screen_renders_rows_and_controls() {
    let mut ui = UiState::new();
    ui.multiplayer.servers = vec![server("Shown", "shown.example.org", 25565)];
    ui.navigate(Screen::Multiplayer);
    let list = ui.render(1280, 720);
    for id in [
        WidgetId::MultiplayerServer(0),
        WidgetId::MultiplayerAddressField,
        WidgetId::MultiplayerJoin,
        WidgetId::MultiplayerAdd,
        WidgetId::MultiplayerDelete,
        WidgetId::MultiplayerBack,
    ] {
        assert!(
            ui.hit_regions.iter().any(|r| r.id == id),
            "missing hit region for {id:?}"
        );
    }
    // The row label renders its name + endpoint into the draw list.
    let mut texts = Vec::new();
    for cmd in &list.commands {
        if let UiCommand::Text { text, .. } = cmd {
            texts.push(text.clone());
        }
    }
    assert!(texts.iter().any(|t| t.contains("Shown")));
}

#[test]
fn disconnected_screen_shows_reason_and_back_returns_to_title() {
    let mut ui = UiState::new();
    ui.screen = Screen::InGameHud;
    ui.show_disconnected(DisconnectedScreenState {
        title: "Disconnected".to_string(),
        reason: "Kicked by an operator.".to_string(),
    });
    assert_eq!(ui.screen, Screen::Disconnected);
    assert_eq!(ui.disconnected.reason, "Kicked by an operator.");

    let list = ui.render(1280, 720);
    let mut texts = Vec::new();
    for cmd in &list.commands {
        if let UiCommand::Text { text, .. } = cmd {
            texts.push(text.clone());
        }
    }
    assert!(texts.iter().any(|t| t.contains("Kicked by an operator.")));

    assert_eq!(
        click(&mut ui, WidgetId::DisconnectedBack, 1280, 720),
        vec![UiAction::OpenScreen(Screen::Title)]
    );
    assert_eq!(ui.screen, Screen::Title);
}

#[test]
fn disconnected_escape_returns_to_title() {
    let mut ui = UiState::new();
    ui.show_disconnected(DisconnectedScreenState {
        title: "Disconnected".to_string(),
        reason: "Timed out".to_string(),
    });
    // The nav stack is dropped on disconnect, so Escape lands on Title.
    ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
    assert_eq!(ui.screen, Screen::Title);
}
