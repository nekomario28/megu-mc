// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_ui::draw::{UiCommand, UiRect};
use megu_ui::gameplay_screens::{BookEditorClick, BookEditorMode, BookEditorState};
use megu_ui::gui_bridge::{lower_bridge_batch_to_draw_list, GuiBridgeBatch, GuiBridgeDrawOp};

#[test]
fn bridge_scissor_metadata_survives_native_draw_list_lowering() {
    let batch = GuiBridgeBatch {
        ops: vec![
            GuiBridgeDrawOp::PushScissor {
                x: 8,
                y: 9,
                w: 20,
                h: 21,
            },
            GuiBridgeDrawOp::FillRect {
                x: 0,
                y: 0,
                w: 40,
                h: 40,
                rgba: [255, 0, 0, 255],
            },
            GuiBridgeDrawOp::PopScissor,
        ],
    };
    let lowered = lower_bridge_batch_to_draw_list(&batch, 100, 100);
    assert_eq!(lowered.commands.len(), 1);
    assert_eq!(lowered.command_clip(0), Some(UiRect::new(8, 9, 20, 21)));

    let mut composed = megu_ui::draw::UiDrawList::new();
    let clips = lowered.command_clips().to_vec();
    for (index, command) in lowered.commands.into_iter().enumerate() {
        composed.push_with_clip(command, clips.get(index).copied().flatten());
    }
    assert_eq!(composed.command_clip(0), Some(UiRect::new(8, 9, 20, 21)));
    assert!(matches!(composed.commands[0], UiCommand::FillRect { .. }));
}

#[test]
fn written_book_mode_navigates_without_edit_or_sign_actions() {
    let mut book = BookEditorState::new_written(
        36,
        vec!["first".into(), "second".into()],
        "A title",
        "An author",
    );
    assert_eq!(book.mode, BookEditorMode::Written);
    book.type_char('x');
    book.backspace();
    assert_eq!(book.pages, ["first", "second"]);
    assert!(book.click(BookEditorClick::Next).is_none());
    assert_eq!(book.page, 1);
    assert!(book.click(BookEditorClick::Previous).is_none());
    assert_eq!(book.page, 0);
    assert!(book.click(BookEditorClick::SignAndClose).is_none());
    assert!(book.click(BookEditorClick::Done).is_some());
}
