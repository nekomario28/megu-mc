// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for `megu_core::romaji_ime`.

use megu_core::romaji_ime::{
    format_romaji_chat, romaji_to_hiragana, RomajiChatConfig, RomajiChatMode,
};

fn config(mode: RomajiChatMode) -> RomajiChatConfig {
    RomajiChatConfig {
        mode,
        ..RomajiChatConfig::default()
    }
}

// ---------------------------------------------------------------------------
// romaji_to_hiragana
// ---------------------------------------------------------------------------

#[test]
fn converts_common_greetings() {
    assert_eq!(romaji_to_hiragana("konnichiha"), "こんにちは");
    assert_eq!(romaji_to_hiragana("arigatou"), "ありがとう");
    assert_eq!(romaji_to_hiragana("yoroshiku"), "よろしく");
    assert_eq!(romaji_to_hiragana("konbanwa"), "こんばんわ");
    assert_eq!(romaji_to_hiragana("ohayou"), "おはよう");
    assert_eq!(romaji_to_hiragana("sumimasen"), "すみません");
}

#[test]
fn converts_all_basic_rows() {
    assert_eq!(romaji_to_hiragana("a i u e o"), "あ い う え お");
    assert_eq!(romaji_to_hiragana("kakikukeko"), "かきくけこ");
    assert_eq!(romaji_to_hiragana("sasisuseso"), "さしすせそ");
    assert_eq!(romaji_to_hiragana("tatituteto"), "たちつてと");
    assert_eq!(romaji_to_hiragana("naninuneno"), "なにぬねの");
    assert_eq!(romaji_to_hiragana("hahiHUheho"), "はひふへほ");
    assert_eq!(romaji_to_hiragana("mamimumemo"), "まみむめも");
    assert_eq!(romaji_to_hiragana("yayuyo"), "やゆよ");
    assert_eq!(romaji_to_hiragana("rarirurero"), "らりるれろ");
    assert_eq!(romaji_to_hiragana("wawo"), "わを");
}

#[test]
fn converts_voiced_and_semi_voiced() {
    assert_eq!(romaji_to_hiragana("gakusei"), "がくせい");
    assert_eq!(romaji_to_hiragana("zasshi"), "ざっし");
    assert_eq!(romaji_to_hiragana("jidousha"), "じどうしゃ");
    assert_eq!(romaji_to_hiragana("bangou"), "ばんごう");
    assert_eq!(romaji_to_hiragana("panda"), "ぱんだ");
}

#[test]
fn converts_youon_digraphs() {
    assert_eq!(romaji_to_hiragana("kyoushitsu"), "きょうしつ");
    assert_eq!(romaji_to_hiragana("shashin"), "しゃしん");
    assert_eq!(romaji_to_hiragana("sya"), "しゃ");
    assert_eq!(romaji_to_hiragana("chotto"), "ちょっと");
    assert_eq!(romaji_to_hiragana("kitsune"), "きつね");
    assert_eq!(romaji_to_hiragana("nyanko"), "にゃんこ");
    assert_eq!(romaji_to_hiragana("byouin"), "びょういん");
    assert_eq!(romaji_to_hiragana("ryouri"), "りょうり");
    assert_eq!(romaji_to_hiragana("gya"), "ぎゃ");
    assert_eq!(romaji_to_hiragana("ja jya zya"), "じゃ じゃ じゃ");
    assert_eq!(romaji_to_hiragana("pya pyu pyo"), "ぴゃ ぴゅ ぴょ");
}

#[test]
fn converts_extended_romaji() {
    assert_eq!(romaji_to_hiragana("fa fi fe fo"), "ふぁ ふぃ ふぇ ふぉ");
    assert_eq!(romaji_to_hiragana("tsa tsi tse tso"), "つぁ つぃ つぇ つぉ");
    assert_eq!(romaji_to_hiragana("thi dhi"), "てぃ でぃ");
    assert_eq!(romaji_to_hiragana("wi we"), "ゐ ゑ");
    assert_eq!(romaji_to_hiragana("va vi vu ve vo"), "ゔぁ ゔぃ ゔ ゔぇ ゔぉ");
}

#[test]
fn converts_small_kana() {
    assert_eq!(romaji_to_hiragana("xtu"), "っ");
    assert_eq!(romaji_to_hiragana("xtsu"), "っ");
    assert_eq!(romaji_to_hiragana("ltu"), "っ");
    assert_eq!(romaji_to_hiragana("ltsu"), "っ");
    assert_eq!(romaji_to_hiragana("xa xi xu xe xo"), "ぁ ぃ ぅ ぇ ぉ");
    assert_eq!(romaji_to_hiragana("la li lu le lo"), "ぁ ぃ ぅ ぇ ぉ");
    assert_eq!(romaji_to_hiragana("xya xyu xyo"), "ゃ ゅ ょ");
    assert_eq!(romaji_to_hiragana("lya lyu lyo"), "ゃ ゅ ょ");
}

#[test]
fn converts_sokuon_geminates() {
    assert_eq!(romaji_to_hiragana("gakkou"), "がっこう");
    assert_eq!(romaji_to_hiragana("katte"), "かって");
    assert_eq!(romaji_to_hiragana("suppai"), "すっぱい");
    assert_eq!(romaji_to_hiragana("zasshi"), "ざっし");
    assert_eq!(romaji_to_hiragana("gakkou"), "がっこう"); // never 学校
}

#[test]
fn converts_n_rules() {
    assert_eq!(romaji_to_hiragana("kantan"), "かんたん");
    assert_eq!(romaji_to_hiragana("shinkansen"), "しんかんせん");
    assert_eq!(romaji_to_hiragana("kan'i"), "かんい");
    assert_eq!(romaji_to_hiragana("konnnitiha"), "こんんにちは");
    assert_eq!(romaji_to_hiragana("denwa"), "でんわ");
}

#[test]
fn converts_punctuation() {
    assert_eq!(romaji_to_hiragana("ra-men"), "らーめん");
    assert_eq!(romaji_to_hiragana("sore, kore."), "それ、 これ。");
    // Punctuation between digits stays literal.
    assert_eq!(romaji_to_hiragana("3.14"), "3.14");
    assert_eq!(romaji_to_hiragana("1,000"), "1,000");
    assert_eq!(romaji_to_hiragana("-5"), "-5");
}

#[test]
fn passes_through_unknown_and_non_ascii() {
    assert_eq!(romaji_to_hiragana("abc123"), "あbc123");
    assert_eq!(romaji_to_hiragana("カタカナ"), "カタカナ");
    assert_eq!(romaji_to_hiragana(""), "");
}

// ---------------------------------------------------------------------------
// format_romaji_chat
// ---------------------------------------------------------------------------

#[test]
fn off_mode_is_vanilla_passthrough() {
    let cfg = config(RomajiChatMode::Off);
    assert_eq!(format_romaji_chat("konnichiha", &cfg), "konnichiha");
    assert_eq!(format_romaji_chat("/tp 1 2 3", &cfg), "/tp 1 2 3");
    let default_cfg = RomajiChatConfig::default();
    assert_eq!(default_cfg.mode, RomajiChatMode::Off);
    assert_eq!(format_romaji_chat("konnichiha", &default_cfg), "konnichiha");
}

#[test]
fn kana_only_mode() {
    let cfg = config(RomajiChatMode::KanaOnly);
    assert_eq!(format_romaji_chat("konnichiha", &cfg), "こんにちは");
    assert_eq!(
        format_romaji_chat("chotto matte", &cfg),
        "ちょっと まって"
    );
}

#[test]
fn kana_with_raw_mode() {
    let cfg = config(RomajiChatMode::KanaWithRaw);
    assert_eq!(
        format_romaji_chat("konnichiha", &cfg),
        "こんにちは (konnichiha)"
    );
}

#[test]
fn bypasses_commands_and_escape_prefix() {
    for mode in [RomajiChatMode::KanaOnly, RomajiChatMode::KanaWithRaw] {
        let cfg = config(mode);
        assert_eq!(format_romaji_chat("/tp 1 2 3", &cfg), "/tp 1 2 3");
        assert_eq!(format_romaji_chat("/give @a diamond", &cfg), "/give @a diamond");
        assert_eq!(format_romaji_chat(".gg nice", &cfg), "gg nice");
        assert_eq!(format_romaji_chat(".hello world", &cfg), "hello world");
    }
}

#[test]
fn custom_escape_prefix() {
    let cfg = RomajiChatConfig {
        mode: RomajiChatMode::KanaOnly,
        escape_prefix: '#',
        max_chars: 256,
    };
    assert_eq!(format_romaji_chat("#konnichiha", &cfg), "konnichiha");
    assert_eq!(format_romaji_chat("konnichiha", &cfg), "こんにちは");
}

#[test]
fn bypasses_existing_japanese() {
    for mode in [RomajiChatMode::KanaOnly, RomajiChatMode::KanaWithRaw] {
        let cfg = config(mode);
        assert_eq!(format_romaji_chat("すでに日本語", &cfg), "すでに日本語");
        assert_eq!(format_romaji_chat("hello 世界", &cfg), "hello 世界");
        assert_eq!(format_romaji_chat("カタカナ only", &cfg), "カタカナ only");
    }
}

#[test]
fn preserves_urls_mentions_and_numerics() {
    let cfg = config(RomajiChatMode::KanaOnly);
    let msg = "konnichiha https://megu.megumin.tech @someone 3.14 GG";
    let out = format_romaji_chat(msg, &cfg);
    assert!(out.contains("https://megu.megumin.tech"));
    assert!(out.contains("@someone"));
    assert!(out.contains("3.14"));
    assert!(out.contains("GG"));
    assert!(out.contains("こんにちは"));

    // A bare URL message is entirely protected.
    assert_eq!(
        format_romaji_chat("https://megu.megumin.tech", &cfg),
        "https://megu.megumin.tech"
    );
}

#[test]
fn preserves_all_caps_acronyms() {
    let cfg = config(RomajiChatMode::KanaOnly);
    assert_eq!(format_romaji_chat("GG AFK TNT", &cfg), "GG AFK TNT");
    assert_eq!(format_romaji_chat("FPS OP", &cfg), "FPS OP");
    // Mixed with romaji: acronym stays, romaji converts.
    assert_eq!(
        format_romaji_chat("GG konnichiha", &cfg),
        "GG こんにちは"
    );
}

#[test]
fn does_not_wrap_when_nothing_converts() {
    let cfg = config(RomajiChatMode::KanaWithRaw);
    assert_eq!(format_romaji_chat("GG AFK", &cfg), "GG AFK");
    assert_eq!(format_romaji_chat("12345", &cfg), "12345");
}

#[test]
fn output_is_bounded_at_max_chars() {
    let long = "konnichiha ".repeat(60); // ~600 chars of romaji
    for mode in [RomajiChatMode::KanaOnly, RomajiChatMode::KanaWithRaw] {
        let cfg = config(mode);
        let out = format_romaji_chat(&long, &cfg);
        assert!(out.chars().count() <= 256, "mode {mode:?} exceeded limit");
        // Char-boundary safe: re-collecting must reproduce identical bytes.
        assert_eq!(out, out.chars().collect::<String>());
    }
}

#[test]
fn custom_max_chars_respected() {
    let cfg = RomajiChatConfig {
        mode: RomajiChatMode::KanaOnly,
        escape_prefix: '.',
        max_chars: 10,
    };
    let out = format_romaji_chat("konnichiha arigatou", &cfg);
    assert_eq!(out.chars().count(), 10);
    assert_eq!(out, "こんにちは ありがと");
}

#[test]
fn empty_and_whitespace_inputs() {
    for mode in [RomajiChatMode::Off, RomajiChatMode::KanaOnly, RomajiChatMode::KanaWithRaw] {
        let cfg = config(mode);
        assert_eq!(format_romaji_chat("", &cfg), "");
        assert_eq!(format_romaji_chat("   ", &cfg), "   ");
    }
}
