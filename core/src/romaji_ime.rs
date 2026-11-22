// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc_spine_core::romaji_ime` — a clean-room, zero-dependency, opt-in
//! Japanese romaji → kana chat converter in the LunaChat style.
//!
//! Everything here is pure `std`, fully deterministic, and never reads a
//! clock. Two layers are provided:
//!
//! * [`romaji_to_hiragana`] — a complete Hepburn + Kunrei-shiki romaji →
//!   hiragana converter covering vowels, all consonant rows, voiced and
//!   semi-voiced kana, youon digraphs, small vowels/tsu via `x`/`l`
//!   prefixes, sokuon geminates, `nn`/`n'` → `ん`, and the `-`/`,`/`.`
//!   punctuation maps. Unknown sequences pass through untouched, so the
//!   output is always bounded by the input length.
//! * [`format_romaji_chat`] + [`RomajiChatConfig`] — the smart chat
//!   pipeline. [`RomajiChatMode::Off`] (the default) is a strict vanilla
//!   passthrough; [`RomajiChatMode::KanaWithRaw`] renders
//!   `こんにちは (konnichiha)`; [`RomajiChatMode::KanaOnly`] renders
//!   `こんにちは`. Commands (`/...`), the configurable escape prefix
//!   (`.`), messages already containing Japanese, embedded URLs,
//!   `@mentions`, numeric tokens, and all-caps acronyms are never
//!   converted. Output is truncated on a UTF-8 char boundary at
//!   `max_chars` (default 256, the vanilla chat limit).

/// Conversion mode for [`format_romaji_chat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RomajiChatMode {
    /// Vanilla passthrough: input returned byte-for-byte unchanged.
    Off,
    /// Convert romaji to hiragana and append the raw input in parentheses:
    /// `こんにちは (konnichiha)`.
    KanaWithRaw,
    /// Convert romaji to hiragana only: `こんにちは`.
    KanaOnly,
}

/// Configuration for [`format_romaji_chat`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomajiChatConfig {
    /// Conversion mode; defaults to [`RomajiChatMode::Off`].
    pub mode: RomajiChatMode,
    /// Leading character that escapes conversion for a single message
    /// (default `.`): `.hello world` → `hello world`.
    pub escape_prefix: char,
    /// Hard cap on output length in Unicode scalar values (default 256,
    /// the vanilla chat limit). Truncation is always char-boundary safe.
    pub max_chars: usize,
}

impl Default for RomajiChatConfig {
    fn default() -> Self {
        Self {
            mode: RomajiChatMode::Off,
            escape_prefix: '.',
            max_chars: 256,
        }
    }
}

/// Applies the LunaChat-style romaji pipeline to a single chat message.
///
/// Vanilla-faithful when `config.mode` is [`RomajiChatMode::Off`]: the
/// input is returned unchanged. Otherwise commands, escaped messages,
/// messages already containing Japanese, and fully-protected messages
/// pass through; remaining words are converted and the result is bounded
/// to `config.max_chars` on a char boundary.
pub fn format_romaji_chat(input: &str, config: &RomajiChatConfig) -> String {
    if config.mode == RomajiChatMode::Off || input.is_empty() {
        return input.to_string();
    }
    if input.starts_with('/') {
        return input.to_string();
    }
    if let Some(rest) = input.strip_prefix(config.escape_prefix) {
        return rest.to_string();
    }
    if contains_japanese(input) {
        return input.to_string();
    }
    let kana = convert_romaji_segments(input);
    if kana == input {
        return input.to_string();
    }
    let result = match config.mode {
        RomajiChatMode::Off => kana,
        RomajiChatMode::KanaOnly => kana,
        RomajiChatMode::KanaWithRaw => {
            let with_raw = format!("{kana} ({input})");
            if with_raw.chars().count() <= config.max_chars {
                with_raw
            } else {
                kana
            }
        }
    };
    truncate_to_chars(&result, config.max_chars)
}

/// Converts a romaji string to hiragana.
///
/// Matching is greedy longest-match over a built-in table covering
/// Hepburn and Kunrei-shiki inputs. `n`/`nn`/`n'` collapse to `ん` when
/// not starting a na-row or youon syllable; doubled consonants produce
/// `っ`; `-`/`,`/`.` map to `ー`/`、`/`。` unless they sit between two
/// ASCII digits. Anything unrecognized — digits, symbols, non-ASCII —
/// passes through unchanged, so output length never exceeds input length
/// by more than the kana expansion of matched syllables.
pub fn romaji_to_hiragana(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphabetic() {
            if lc == 'n'
                && i + 1 < chars.len()
                && chars[i + 1] == '\''
            {
                out.push('ん');
                i += 2;
                continue;
            }
            if let Some((kana, len)) = lookup_romaji(&chars, i) {
                out.push_str(kana);
                i += len;
                continue;
            }
            if lc == 'n' {
                out.push('ん');
                i += 1;
                continue;
            }
            if i + 1 < chars.len()
                && chars[i + 1].to_ascii_lowercase() == lc
                && is_sokuon_consonant(lc)
            {
                out.push('っ');
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '-' | ',' | '.' if is_between_digits(&chars, i) => out.push(c),
            // A `-` introducing a number (e.g. `-5`, `score -3`) is a sign,
            // not a long-vowel mark.
            '-' if i + 1 < chars.len()
                && chars[i + 1].is_ascii_digit()
                && (i == 0 || !chars[i - 1].is_ascii_alphabetic()) =>
            {
                out.push(c)
            }
            '-' => out.push('ー'),
            ',' => out.push('、'),
            '.' => out.push('。'),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Hiragana / katakana / CJK unified ideographs that mark a message as
/// already Japanese (bypass signal for [`format_romaji_chat`]).
fn contains_japanese(text: &str) -> bool {
    text.chars().any(|c| {
        let u = c as u32;
        (0x3040..=0x309F).contains(&u)
            || (0x30A0..=0x30FF).contains(&u)
            || (0x4E00..=0x9FFF).contains(&u)
    })
}

/// Splits `input` into whitespace-delimited words, converts each
/// non-protected word, and copies whitespace verbatim.
fn convert_romaji_segments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut word = String::new();
    for c in input.chars() {
        if c.is_whitespace() {
            flush_word(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush_word(&mut word, &mut out);
    out
}

fn flush_word(word: &mut String, out: &mut String) {
    if word.is_empty() {
        return;
    }
    if is_protected_token(word) {
        out.push_str(word);
    } else {
        out.push_str(&romaji_to_hiragana(word));
    }
    word.clear();
}

/// Words that must survive conversion untouched: `@mentions`, URLs,
/// numeric tokens, and all-caps acronyms.
fn is_protected_token(word: &str) -> bool {
    if word.starts_with('@') || word.contains("://") || word.starts_with("www.") {
        return true;
    }
    let mut digits = 0usize;
    let mut letters = 0usize;
    let mut uppercase = 0usize;
    for c in word.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c.is_ascii_alphabetic() {
            letters += 1;
            if c.is_ascii_uppercase() {
                uppercase += 1;
            }
        }
    }
    if digits > 0
        && word
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '+' | '-' | '/' | '%' | '(' | ')' | '=' | ':' | ';'))
    {
        return true;
    }
    letters > 0 && uppercase == letters
}

fn truncate_to_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

fn is_between_digits(chars: &[char], i: usize) -> bool {
    i > 0
        && i + 1 < chars.len()
        && chars[i - 1].is_ascii_digit()
        && chars[i + 1].is_ascii_digit()
}

/// Consonants that geminate to `っ` when doubled. `n` is excluded because
/// `nn` follows the `ん` rule instead.
fn is_sokuon_consonant(c: char) -> bool {
    matches!(
        c,
        'b' | 'c'
            | 'd'
            | 'f'
            | 'g'
            | 'h'
            | 'j'
            | 'k'
            | 'l'
            | 'm'
            | 'p'
            | 'r'
            | 's'
            | 't'
            | 'v'
            | 'w'
            | 'x'
            | 'y'
            | 'z'
    )
}

/// Greedy longest-match lookup at `chars[i]`, trying lengths 4 → 1.
/// Returns the hiragana and the number of input chars consumed.
fn lookup_romaji(chars: &[char], i: usize) -> Option<(&'static str, usize)> {
    for len in (1..=4).rev() {
        if i + len > chars.len() {
            continue;
        }
        let mut buf = [0u8; 4];
        let mut ascii = true;
        for (j, &ch) in chars[i..i + len].iter().enumerate() {
            let l = ch.to_ascii_lowercase();
            if !l.is_ascii() {
                ascii = false;
                break;
            }
            buf[j] = (l as u32) as u8;
        }
        if !ascii {
            continue;
        }
        if let Some(kana) = romaji_lookup(&buf[..len]) {
            return Some((kana, len));
        }
    }
    None
}

fn romaji_lookup(key: &[u8]) -> Option<&'static str> {
    Some(match key {
        // Vowels
        b"a" => "あ",
        b"i" => "い",
        b"u" => "う",
        b"e" => "え",
        b"o" => "お",
        // ka-row
        b"ka" => "か",
        b"ki" => "き",
        b"ku" => "く",
        b"ke" => "け",
        b"ko" => "こ",
        b"kya" => "きゃ",
        b"kyu" => "きゅ",
        b"kyo" => "きょ",
        // sa-row
        b"sa" => "さ",
        b"shi" => "し",
        b"si" => "し",
        b"su" => "す",
        b"se" => "せ",
        b"so" => "そ",
        b"sha" => "しゃ",
        b"shu" => "しゅ",
        b"sho" => "しょ",
        b"sya" => "しゃ",
        b"syu" => "しゅ",
        b"syo" => "しょ",
        b"she" => "しぇ",
        // ta-row
        b"ta" => "た",
        b"chi" => "ち",
        b"ti" => "ち",
        b"tsu" => "つ",
        b"tu" => "つ",
        b"te" => "て",
        b"to" => "と",
        b"cha" => "ちゃ",
        b"chu" => "ちゅ",
        b"cho" => "ちょ",
        b"tya" => "ちゃ",
        b"tyu" => "ちゅ",
        b"tyo" => "ちょ",
        b"che" => "ちぇ",
        b"tsa" => "つぁ",
        b"tsi" => "つぃ",
        b"tse" => "つぇ",
        b"tso" => "つぉ",
        b"thi" => "てぃ",
        // na-row
        b"na" => "な",
        b"ni" => "に",
        b"nu" => "ぬ",
        b"ne" => "ね",
        b"no" => "の",
        b"nya" => "にゃ",
        b"nyu" => "にゅ",
        b"nyo" => "にょ",
        // ha-row
        b"ha" => "は",
        b"hi" => "ひ",
        b"fu" => "ふ",
        b"hu" => "ふ",
        b"he" => "へ",
        b"ho" => "ほ",
        b"hya" => "ひゃ",
        b"hyu" => "ひゅ",
        b"hyo" => "ひょ",
        b"fa" => "ふぁ",
        b"fi" => "ふぃ",
        b"fe" => "ふぇ",
        b"fo" => "ふぉ",
        // ma-row
        b"ma" => "ま",
        b"mi" => "み",
        b"mu" => "む",
        b"me" => "め",
        b"mo" => "も",
        b"mya" => "みゃ",
        b"myu" => "みゅ",
        b"myo" => "みょ",
        // ya-row
        b"ya" => "や",
        b"yu" => "ゆ",
        b"yo" => "よ",
        // ra-row
        b"ra" => "ら",
        b"ri" => "り",
        b"ru" => "る",
        b"re" => "れ",
        b"ro" => "ろ",
        b"rya" => "りゃ",
        b"ryu" => "りゅ",
        b"ryo" => "りょ",
        // wa-row
        b"wa" => "わ",
        b"wo" => "を",
        b"wi" => "ゐ",
        b"we" => "ゑ",
        b"ye" => "いぇ",
        // ga-row (voiced)
        b"ga" => "が",
        b"gi" => "ぎ",
        b"gu" => "ぐ",
        b"ge" => "げ",
        b"go" => "ご",
        b"gya" => "ぎゃ",
        b"gyu" => "ぎゅ",
        b"gyo" => "ぎょ",
        // za-row (voiced)
        b"za" => "ざ",
        b"ji" => "じ",
        b"zi" => "じ",
        b"zu" => "ず",
        b"ze" => "ぜ",
        b"zo" => "ぞ",
        b"ja" => "じゃ",
        b"ju" => "じゅ",
        b"jo" => "じょ",
        b"jya" => "じゃ",
        b"jyu" => "じゅ",
        b"jyo" => "じょ",
        b"je" => "じぇ",
        b"zya" => "じゃ",
        b"zyu" => "じゅ",
        b"zyo" => "じょ",
        // da-row (voiced)
        b"da" => "だ",
        b"di" => "ぢ",
        b"du" => "づ",
        b"de" => "で",
        b"do" => "ど",
        b"dya" => "ぢゃ",
        b"dyu" => "ぢゅ",
        b"dyo" => "ぢょ",
        b"dhi" => "でぃ",
        // ba-row (voiced)
        b"ba" => "ば",
        b"bi" => "び",
        b"bu" => "ぶ",
        b"be" => "べ",
        b"bo" => "ぼ",
        b"bya" => "びゃ",
        b"byu" => "びゅ",
        b"byo" => "びょ",
        // pa-row (semi-voiced)
        b"pa" => "ぱ",
        b"pi" => "ぴ",
        b"pu" => "ぷ",
        b"pe" => "ぺ",
        b"po" => "ぽ",
        b"pya" => "ぴゃ",
        b"pyu" => "ぴゅ",
        b"pyo" => "ぴょ",
        // v-row (katakana-origin loan sounds, hiragana forms)
        b"va" => "ゔぁ",
        b"vi" => "ゔぃ",
        b"vu" => "ゔ",
        b"ve" => "ゔぇ",
        b"vo" => "ゔぉ",
        // Small vowels / tsu via x- and l- prefixes
        b"xa" => "ぁ",
        b"xi" => "ぃ",
        b"xu" => "ぅ",
        b"xe" => "ぇ",
        b"xo" => "ぉ",
        b"xya" => "ゃ",
        b"xyu" => "ゅ",
        b"xyo" => "ょ",
        b"xtu" => "っ",
        b"xtsu" => "っ",
        b"la" => "ぁ",
        b"li" => "ぃ",
        b"lu" => "ぅ",
        b"le" => "ぇ",
        b"lo" => "ぉ",
        b"lya" => "ゃ",
        b"lyu" => "ゅ",
        b"lyo" => "ょ",
        b"ltu" => "っ",
        b"ltsu" => "っ",
        _ => return None,
    })
}
