// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Behavior tests for the zsh-style command bar: readline editing, ghost text,
//! reverse search, completion cycling, syntax highlighting, and badges.

use mc_spine_client::zsh_command_bar::{
    BadgeKind, BadgeSide, CommandBarAction, CommandKey, CompletionCandidate, HighlightColor,
    HighlightKind, ZshCommandBar, GLYPH_PIXELS,
};

fn candidate_for(bar: &ZshCommandBar, text: &str) -> CompletionCandidate {
    bar.completions()
        .into_iter()
        .find(|candidate| candidate.text == text)
        .unwrap_or_else(|| panic!("missing completion '{text}' for '{}'", bar.text()))
}

fn assert_badge(candidate: &CompletionCandidate, kind: BadgeKind, sprite_key: &str) {
    assert_eq!(candidate.badge_side, BadgeSide::Left);
    let badge = candidate
        .badge
        .clone()
        .unwrap_or_else(|| panic!("candidate '{}' has no badge", candidate.text));
    assert_eq!(badge.kind, kind);
    assert_eq!(badge.sprite_key, sprite_key);
    assert_eq!(badge.glyph_8x8.len(), GLYPH_PIXELS);
    assert!(
        badge.glyph_8x8.iter().any(|&pixel| pixel != 0),
        "badge glyph for '{sprite_key}' is empty"
    );
}

#[test]
fn readline_editing_controls() {
    let mut bar = ZshCommandBar::new();
    bar.set_text("/give @p diamond_sword");

    assert_eq!(bar.handle_key(CommandKey::CtrlW), CommandBarAction::Changed);
    assert_eq!(bar.text(), "/give @p ");
    assert_eq!(bar.cursor(), bar.text().len());

    bar.handle_key(CommandKey::CtrlA);
    assert_eq!(bar.cursor(), 0);
    bar.handle_key(CommandKey::CtrlE);
    assert_eq!(bar.cursor(), bar.text().len());

    bar.set_cursor(3);
    bar.handle_key(CommandKey::CtrlU);
    assert_eq!(bar.text(), "ve @p ");
    assert_eq!(bar.cursor(), 0);
}

#[test]
fn ghost_text_accepts_history_and_completion_suggestions() {
    let mut bar = ZshCommandBar::new();
    bar.push_history_entry("/give @p diamond_sword");
    bar.set_text("/giv");
    assert_eq!(bar.ghost_text(), "e @p diamond_sword");
    assert!(bar.accept_ghost_text());
    assert_eq!(bar.text(), "/give @p diamond_sword");

    let mut bar = ZshCommandBar::new();
    bar.set_text("/gi");
    assert_eq!(bar.ghost_text(), "ve");
    assert_eq!(
        bar.handle_key(CommandKey::End),
        CommandBarAction::GhostAccepted
    );
    assert_eq!(bar.text(), "/give");
}

#[test]
fn reverse_incremental_search_finds_history() {
    let mut bar = ZshCommandBar::new();
    bar.push_history_entry("/give @p diamond_sword");
    bar.push_history_entry("/summon zombie ~ ~ ~");
    bar.push_history_entry("/tp @a 0 0 0");
    bar.set_text("draft");

    assert_eq!(
        bar.handle_key(CommandKey::CtrlR),
        CommandBarAction::ReverseSearchStarted
    );
    assert!(bar.is_reverse_search_active());
    assert_eq!(bar.handle_char('s'), CommandBarAction::ReverseSearchChanged);
    assert_eq!(
        bar.reverse_search_match().as_deref(),
        Some("/summon zombie ~ ~ ~")
    );
    assert_eq!(
        bar.handle_key(CommandKey::Enter),
        CommandBarAction::ReverseSearchFinished
    );
    assert_eq!(bar.text(), "/summon zombie ~ ~ ~");
    assert!(!bar.is_reverse_search_active());
}

#[test]
fn tab_and_shift_tab_cycle_completion_candidates() {
    let mut bar = ZshCommandBar::new();
    bar.set_text("/summon ");

    let first = bar.handle_key(CommandKey::Tab);
    assert!(
        matches!(first, CommandBarAction::Completed { index: 0, .. }),
        "unexpected first completion action: {first:?}"
    );
    assert_eq!(bar.text(), "/summon allay");

    let second = bar.handle_key(CommandKey::Tab);
    assert!(
        matches!(second, CommandBarAction::Completed { index: 1, .. }),
        "unexpected second completion action: {second:?}"
    );
    assert_eq!(bar.text(), "/summon armadillo");

    let back = bar.handle_key(CommandKey::ShiftTab);
    assert!(
        matches!(back, CommandBarAction::Completed { index: 0, .. }),
        "unexpected reverse completion action: {back:?}"
    );
    assert_eq!(bar.text(), "/summon allay");
}

#[test]
fn syntax_highlighting_marks_valid_and_invalid_commands() {
    let mut bar = ZshCommandBar::new();
    bar.set_text("/give @p diamond_sword");
    let spans = bar.syntax_highlight();
    assert_eq!(spans[0].kind, HighlightKind::Command);
    assert_eq!(spans[0].color, HighlightColor::Green);
    assert_eq!(spans[1].kind, HighlightKind::TargetSelector);
    assert_eq!(spans[1].color, HighlightColor::Yellow);
    assert_eq!(spans[2].kind, HighlightKind::StringOrNbt);
    assert_eq!(spans[2].color, HighlightColor::White);

    bar.set_text("/tp @p ~ ~ ~");
    let spans = bar.syntax_highlight();
    assert_eq!(spans[2].kind, HighlightKind::Coordinate);
    assert_eq!(spans[2].color, HighlightColor::Blue);

    bar.set_text("/bogus command");
    let spans = bar.syntax_highlight();
    assert!(spans.iter().all(|span| span.kind == HighlightKind::Invalid));
    assert!(spans.iter().all(|span| span.color == HighlightColor::Red));
    assert!(bar
        .usage_hint()
        .unwrap()
        .contains("Unknown command '/bogus'"));
}

#[test]
fn give_candidates_emit_item_badges_for_vanilla_and_modded_ids() {
    let mut bar = ZshCommandBar::new();
    bar.register_item("create:zinc_ore");
    bar.register_item("create:brass_ingot");
    bar.register_item("farmersdelight:tomato");

    bar.set_text("/give @p diamond_s");
    let sword = candidate_for(&bar, "diamond_sword");
    assert_badge(&sword, BadgeKind::ItemSprite, "item/diamond_sword");
    assert_eq!(sword.right_tag, None);

    bar.set_text("/give @p create:zinc");
    let zinc = candidate_for(&bar, "create:zinc_ore");
    assert_badge(&zinc, BadgeKind::ItemSprite, "item/create:zinc_ore");

    bar.set_text("/give @p create:brass");
    let brass = candidate_for(&bar, "create:brass_ingot");
    assert_badge(&brass, BadgeKind::ItemSprite, "item/create:brass_ingot");

    bar.set_text("/give @p farmersdelight:tom");
    let tomato = candidate_for(&bar, "farmersdelight:tomato");
    assert_badge(&tomato, BadgeKind::ItemSprite, "item/farmersdelight:tomato");
}

#[test]
fn summon_and_selector_entities_emit_mob_face_badges() {
    let mut bar = ZshCommandBar::new();
    bar.set_text("/summon zom");
    let zombie = candidate_for(&bar, "zombie");
    assert_badge(&zombie, BadgeKind::MobFace, "entity/zombie");
    assert_eq!(zombie.right_tag.as_deref(), Some("Hostile"));

    bar.set_text("@e[type=zom");
    let selector = candidate_for(&bar, "zombie]");
    assert_badge(&selector, BadgeKind::MobFace, "entity/zombie");
    assert_eq!(selector.right_tag.as_deref(), Some("Hostile"));
}

#[test]
fn effect_and_enchant_candidates_emit_contextual_badges() {
    let mut bar = ZshCommandBar::new();
    bar.set_text("/effect give @p spe");
    let speed = candidate_for(&bar, "speed");
    assert_badge(&speed, BadgeKind::StatusEffect, "mob_effect/speed");
    assert_eq!(speed.right_tag.as_deref(), Some("Buff"));

    bar.set_text("/effect give @p poison");
    let poison = candidate_for(&bar, "poison");
    assert_badge(&poison, BadgeKind::StatusEffect, "mob_effect/poison");
    assert_eq!(poison.right_tag.as_deref(), Some("Debuff"));

    bar.set_text("/enchant @p smi");
    let smite = candidate_for(&bar, "smite");
    assert_badge(&smite, BadgeKind::EnchantBook, "item/enchanted_book");
    assert_eq!(smite.right_tag.as_deref(), Some("Max V"));

    bar.set_text("/enchant @p vanishing");
    let curse = candidate_for(&bar, "vanishing_curse");
    assert_badge(&curse, BadgeKind::EnchantBook, "item/enchanted_book");
    assert_eq!(curse.right_tag.as_deref(), Some("Curse"));
}

#[test]
fn setblock_and_fill_emit_block_swatch_badges() {
    let mut bar = ZshCommandBar::new();
    bar.register_block("create:zinc_ore");
    bar.register_block("farmersdelight:tomato");

    bar.set_text("/setblock 0 0 0 sto");
    let stone = candidate_for(&bar, "stone");
    assert_badge(&stone, BadgeKind::BlockSwatch, "block/stone");
    assert_eq!(stone.right_tag, None);

    bar.set_text("/setblock 0 0 0 create:zinc");
    let zinc = candidate_for(&bar, "create:zinc_ore");
    assert_badge(&zinc, BadgeKind::BlockSwatch, "block/create:zinc_ore");

    bar.set_text("/fill 0 0 0 1 1 1 farmersdelight:tom");
    let tomato = candidate_for(&bar, "farmersdelight:tomato");
    assert_badge(
        &tomato,
        BadgeKind::BlockSwatch,
        "block/farmersdelight:tomato",
    );
}
