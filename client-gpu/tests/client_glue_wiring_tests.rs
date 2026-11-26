// SPDX-License-Identifier: LGPL-3.0-or-later
//! Cross-lane glue regressions: credits acknowledgements, book draft
//! retention/routing, and clip-aware UI fingerprinting.

use megu_client::protocol::{ClientPlayEvent, ItemComponentPatch, ItemStackComponents, PlayEvent};
use megu_client_gpu::live_session::{LiveSession, SessionStatus};
use megu_client_gpu::window::GpuApp;
use megu_client_gpu::{fingerprint_draw_list, GpuCliConfig};
use megu_proto::item_components::{
    plain_text_component, VerifiedComponent, WrittenBook, WrittenPage,
};
use megu_ui::draw::{UiColor, UiCommand, UiDrawList, UiRect};
use megu_ui::gameplay_screens::{BookEditorAction, BookEditorMode};

fn playing_session() -> LiveSession {
    let mut live = LiveSession::offline_with_null_audio();
    live.status = SessionStatus::Playing;
    live
}

fn credits_app() -> GpuApp {
    let mut live = playing_session();
    live.apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
        event: 4,
        value: 1.0,
    }))
    .expect("win-game event applies");
    let mut app = GpuApp::new(GpuCliConfig::default());
    app.attach_live(live);
    assert!(app.activate_pending_credits());
    assert!(app.credits_screen_open());
    app
}

#[test]
fn end_credits_escape_and_natural_completion_enqueue_ack() {
    let mut escaped = credits_app();
    assert!(escaped.escape_end_credits_screen());
    assert!(!escaped.credits_screen_open());
    assert!(
        !escaped.end_credits_ack_pending(),
        "playing session accepted the end-credits client command"
    );

    let mut completed = credits_app();
    assert!(!completed.tick_end_credits_screen(120.0, 600, false));
    assert!(!completed.credits_screen_open());
    assert!(
        !completed.end_credits_ack_pending(),
        "natural completion also enqueued the client command"
    );

    let mut retry_live = LiveSession::offline_with_null_audio();
    retry_live
        .apply_event(ClientPlayEvent::Play(PlayEvent::GameEvent {
            event: 4,
            value: 1.0,
        }))
        .expect("win-game event applies");
    let mut retry = GpuApp::new(GpuCliConfig::default());
    retry.attach_live(retry_live);
    assert!(retry.activate_pending_credits());
    assert!(retry.escape_end_credits_screen());
    assert!(
        retry.end_credits_ack_pending(),
        "a pre-play/full queue close remains pending for a later retry"
    );
}

#[test]
fn book_save_failure_retains_draft_and_success_closes_editor() {
    let mut full_queue_live = playing_session();
    for _ in 0..8 {
        assert!(full_queue_live.send_end_credits_complete());
    }
    assert!(
        !full_queue_live.send_end_credits_complete(),
        "fixture filled the bounded worker queue"
    );
    let mut failed = GpuApp::new(GpuCliConfig::default());
    failed.attach_live(full_queue_live);
    failed.open_book_editor(36, vec!["edited draft".into()]);
    failed.dispatch_book_action(BookEditorAction::Save {
        slot: 36,
        pages: vec!["edited draft".into()],
        title: Some("draft title".into()),
    });
    let editor = failed
        .book_editor_state_for_test()
        .expect("failed enqueue must retain the editor draft");
    assert_eq!(editor.pages, vec!["edited draft"]);
    assert_eq!(editor.mode, BookEditorMode::Edit);

    let mut succeeded = GpuApp::new(GpuCliConfig::default());
    succeeded.attach_live(playing_session());
    succeeded.open_book_editor(36, vec!["draft page".into()]);
    succeeded.dispatch_book_action(BookEditorAction::Save {
        slot: 36,
        pages: vec!["accepted page".into()],
        title: None,
    });
    assert!(
        succeeded.book_editor_state_for_test().is_none(),
        "accepted enqueue closes the editor and consumes the draft"
    );
}

#[test]
fn fingerprint_draw_list_distinguishes_clip_only_changes() {
    let rect = UiRect::new(4, 5, 30, 10);
    let command = UiCommand::FillRect {
        rect,
        color: UiColor::rgb(20, 40, 60),
    };
    let mut unclipped = UiDrawList::new();
    unclipped.push(command.clone());
    let mut clipped = UiDrawList::new();
    clipped.push_with_clip(command.clone(), Some(UiRect::new(0, 0, 12, 12)));
    let mut different_clip = UiDrawList::new();
    different_clip.push_with_clip(command, Some(UiRect::new(2, 0, 12, 12)));

    let unclipped_fp = fingerprint_draw_list(64, 64, &unclipped);
    let clipped_fp = fingerprint_draw_list(64, 64, &clipped);
    let different_clip_fp = fingerprint_draw_list(64, 64, &different_clip);
    assert_ne!(unclipped_fp, clipped_fp);
    assert_ne!(clipped_fp, different_clip_fp);
}

#[test]
fn server_open_book_routes_written_book_to_read_only_viewer() {
    let table = megu_proto::ProtocolTable::v767_builtin();
    let written_item = table
        .registry_id("minecraft:item", "minecraft:written_book")
        .expect("written book item id") as i32;
    let component = VerifiedComponent::WrittenBookContent(WrittenBook {
        title: "Signed title".into(),
        filtered_title: None,
        author: "Author".into(),
        generation: 0,
        pages: vec![WrittenPage {
            text: plain_text_component("Written page").expect("plain text component"),
            filtered: None,
        }],
        resolved: true,
    })
    .to_raw()
    .expect("written book component encodes");

    let mut live = playing_session();
    live.player_inventory.insert(
        36,
        ItemStackComponents {
            count: 1,
            item_id: written_item,
            added: vec![ItemComponentPatch {
                type_id: component.type_id as i32,
                data: component.data,
            }],
            removed: Vec::new(),
        },
    );
    live.apply_event(ClientPlayEvent::Play(PlayEvent::OpenBook { hand: 0 }))
        .expect("open-book event applies");

    let mut app = GpuApp::new(GpuCliConfig::default());
    app.attach_live(live);
    assert!(app.sync_open_book_events());
    let viewer = app
        .book_editor_state_for_test()
        .expect("written book viewer open");
    assert_eq!(viewer.mode, BookEditorMode::Written);
    assert_eq!(viewer.title, "Signed title");
    assert_eq!(viewer.author, "Author");
    assert_eq!(viewer.pages, vec!["Written page"]);
}

#[test]
fn mixed_create_and_generic_mrs1_frame_installs_both_sparse_bands_live() {
    let registry = megu_proto::mod_wire::Mrs1Registry {
        base_block_state_id: 26_684,
        base_item_id: 1_000_000,
        block_states: vec![
            megu_proto::mod_wire::Mrs1BlockStateEntry {
                state_id: 26_684,
                block_id: "create:shaft".into(),
                properties: Vec::new(),
            },
            megu_proto::mod_wire::Mrs1BlockStateEntry {
                state_id: 1_000_000,
                block_id: "examplemod:machine".into(),
                properties: Vec::new(),
            },
        ],
        items: Default::default(),
    };
    let frame = megu_proto::mod_wire::encode_mrs1_frame_v2(&registry).expect("encode MRS1 v2");

    let mut live = playing_session();
    live.ingest_custom_channel_payload_for_test("megu:mod_registry_sync_v1", &frame);

    assert_eq!(live.mod_registry_rejected, 0);
    assert_eq!(
        live.world.blocks.state_name(26_684),
        Some("create:shaft"),
        "the Create band must remain installed"
    );
    assert_eq!(
        live.world.blocks.state_name(1_000_000),
        Some("examplemod:machine"),
        "the generic mod band beyond the old dense probe window must install"
    );
    let synced = live.mod_registry.as_ref().expect("MRS1 registry retained");
    assert_eq!(synced.block_state_name(26_684), Some("create:shaft"));
    assert_eq!(
        synced.block_state_name(1_000_000),
        Some("examplemod:machine")
    );
}
