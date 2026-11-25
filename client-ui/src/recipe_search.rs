// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Fast recipe search index and nearby-chest craft planner — the pure-logic
//! core a JEI/EMI-style overlay wires into its screen.
//!
//! Three cooperating layers, all `std`-only and fully deterministic (no
//! clock reads, no I/O, no unsafe):
//!
//! * **Japanese-aware normalization** — [`normalize_kana`] folds katakana
//!   to hiragana, half-width katakana to hiragana (with `ﾞ`/`ﾟ` voicing),
//!   full-width ASCII to half-width lowercase, drops the prolonged sound
//!   mark `ー`, and collapses long-vowel kana spellings
//!   (`とうきょう → ときょ`). [`kana_to_romaji`] renders a normalized
//!   kana string as Hepburn romaji (`っ` geminates, `ん → n`, youon
//!   digraphs). [`query_variants`] explodes a raw search token into every
//!   form it can match in: the folded literal, its romaji→hiragana
//!   conversion (via `megu_core::romaji_ime`), and the romaji
//!   re-rendering — so `daia`, `だいあ`, `ダイア`, `ダイヤ` and `diamond`
//!   all reach the same `minecraft:diamond` entry indexed as
//!   `ダイヤモンド`/`daiyamondo`.
//! * **`RecipeSearchIndex`** — an inverted index over a host-supplied
//!   item catalog (`mod → items`, `tag → items`, `output item →
//!   producing recipes`, `input item → consuming recipes` with tag
//!   slots resolved against the catalog) plus a per-item normalized
//!   match document for plain-text terms. Query syntax is the JEI set:
//!   plain text AND-ed across tokens, `@mod` namespace filters,
//!   `#tag` item-tag filters — `@create 歯車`, `#planks oak` just work.
//!   Plain text is a pure substring match because every fuzzy
//!   equivalence is folded at normalize time: long-vowel kana, dropped
//!   `ー`, katakana/half-width/full-width folding, and [`glide_fold`]'s
//!   vowel-glide collapse (`daiyamondo` indexes as `daiamondo`, so
//!   `daia`, `daiya` and `だいあ` all land on `minecraft:diamond`).
//! * **`NearbyChestCraftPlanner`** — the 1-click craft: given a recipe's
//!   [`IngredientReq`]s, the player inventory slots and the surrounding
//!   chests (sorted by distance, filtered by a configurable reach —
//!   `6.0` blocks by default), it reports
//!   [`CraftAvailability::CraftableFromInventory`],
//!   [`CraftAvailability::CraftableWithChests`] or
//!   [`CraftAvailability::MissingIngredients`], and when craftable emits
//!   an atomic [`CraftTransferPlan`] — deterministic [`SlotPull`]s that
//!   drain the player first then the nearest chest, never take more than
//!   a slot holds, and support both single crafts and max-stack
//!   shift-crafts ([`NearbyChestCraftPlanner::plan_max`]).

use std::collections::{BTreeMap, HashMap};

use megu_core::romaji_ime::romaji_to_hiragana;

// ---------------------------------------------------------------------------
// Japanese text normalization
// ---------------------------------------------------------------------------

/// Folds a string into its canonical search form: katakana → hiragana,
/// half-width katakana → hiragana (voiced/semi-voiced marks applied),
/// full-width ASCII → half-width lowercase, ideographic space → plain
/// space, the prolonged sound mark `ー` (and wave/horizontal dash
/// lookalikes) dropped, and long-vowel kana spellings collapsed
/// (`おお`/`おう → お`, `えい → え`, …).
///
/// Applied identically to indexed names and query tokens, so
/// `ダイヤモンド`, `だいやもんど`, `ﾀﾞｲﾔﾓﾝﾄﾞ` and `ＤＡＩＹＡＭＯＮＤ`
/// all normalize to the same reading `だいやもんど`.
pub fn normalize_kana(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let u = c as u32;
        // Half-width katakana with optional ﾞ/ﾟ combining marks.
        if (0xFF66..=0xFF9D).contains(&u) {
            let mut kana = HALFWIDTH_KANA[(u - 0xFF66) as usize];
            if i + 1 < chars.len() {
                let mark = chars[i + 1];
                if mark == '\u{FF9E}' {
                    if let Some(v) = voiced_kana(kana, false) {
                        kana = v;
                        i += 1;
                    }
                } else if mark == '\u{FF9F}' {
                    if let Some(v) = voiced_kana(kana, true) {
                        kana = v;
                        i += 1;
                    }
                }
            }
            if kana != 'ー' {
                out.push(kana);
            }
        } else if (0xFF61..=0xFF65).contains(&u) {
            // Half-width punctuation: fold to the full-width forms so
            // they compare equal with typed full-width input.
            out.push(HALFWIDTH_PUNCT[(u - 0xFF61) as usize]);
        } else if c == '\u{FF9E}' {
            out.push('゛');
        } else if c == '\u{FF9F}' {
            out.push('゜');
        } else if (0x30A1..=0x30F6).contains(&u) {
            // Full-width katakana → hiragana (fixed -0x60 offset).
            out.push(char::from_u32(u - 0x60).unwrap_or(c));
        } else if (0xFF01..=0xFF5E).contains(&u) {
            // Full-width ASCII variants → half-width, lowercased.
            let half = char::from_u32(u - 0xFEE0).unwrap_or(c);
            out.push(if half.is_ascii_uppercase() {
                half.to_ascii_lowercase()
            } else {
                half
            });
        } else if c.is_ascii_uppercase() {
            out.push(c.to_ascii_lowercase());
        } else if c == '\u{3000}' {
            out.push(' ');
        } else if matches!(c, 'ー' | '‐' | '‑' | '–' | '—' | '―' | '〜') {
            // Prolonged sound mark and its lookalikes: drop, so
            // コーヒー and こひ compare equal.
        } else {
            out.push(c);
        }
        i += 1;
    }
    collapse_long_vowels(&out)
}

/// Renders normalized kana as Hepburn romaji: youon digraphs
/// (`きゃ → kya`, `しゃ → sha`, `ちゃ → cha`, `りょ → ryo`), `っ` doubling
/// the next consonant, `ん → n`, and the foreign-sound small-kana
/// combos (`ふぁ → fa`, `うぉ → wo`, `ゔぇ → ve`, `つぉ → tso`).
/// Non-kana characters — ASCII, kanji, punctuation — pass through
/// unchanged, so `歯車` stays `歯車`.
pub fn kana_to_romaji(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == 'っ' {
            // Sokuon: geminate the leading consonant of the next
            // syllable (っこ → kko). Nothing to double at the end.
            if next.is_some() {
                if let Some(first) = syllable_romaji(&chars, i + 1).0.chars().next() {
                    if first.is_ascii_alphabetic() {
                        out.push(first);
                    }
                }
            }
            i += 1;
            continue;
        }
        let (romaji, consumed) = syllable_romaji(&chars, i);
        if romaji.is_empty() && consumed == 1 {
            // Non-kana (kanji, ASCII, punctuation) passes through.
            out.push(c);
        } else {
            out.push_str(romaji);
        }
        i += consumed;
    }
    out
}

/// All normalized forms a raw query token can match in.
///
/// For ASCII input (`daia`) this is `{ folded literal, romaji→hiragana
/// conversion, romaji re-render of that conversion }`; for kana input
/// (`だいあ` / `ダイア`) `{ folded kana, its romaji rendering }`; for
/// kanji or mixed input just the folded literal. Duplicates and empty
/// forms are removed, order is stable.
pub fn query_variants(token: &str) -> Vec<String> {
    let folded = normalize_kana(token);
    if folded.is_empty() {
        return Vec::new();
    }
    let mut out = vec![glide_fold(&folded)];
    if folded.is_ascii() {
        let kana = normalize_kana(&romaji_to_hiragana(&folded));
        if !kana.is_empty() && kana != folded {
            let back = kana_to_romaji(&kana);
            out.push(kana);
            let back = glide_fold(&back);
            if !back.is_empty() && back != out[0] {
                out.push(back);
            }
        }
    } else {
        let romaji = glide_fold(&kana_to_romaji(&folded));
        if !romaji.is_empty() && romaji != out[0] {
            out.push(romaji);
        }
    }
    out
}

/// Collapses `y`/`w` glides sitting between two vowels in ASCII text
/// (`daiya → daia`, `touyou → touo`-style). Applied identically to
/// indexed romaji/name fields and query variants, so the loose roman
/// spellings people actually type (`daia`, `daiya`, `diya`) all land on
/// the same canonical reading — a plain substring match, not fuzzy
/// scoring. Non-ASCII input is returned untouched.
pub fn glide_fold(text: &str) -> String {
    if !text.is_ascii() {
        return text.to_owned();
    }
    let b = text.as_bytes();
    let vowel = |x: u8| matches!(x, b'a' | b'e' | b'i' | b'o' | b'u');
    let mut out = String::with_capacity(b.len());
    for (i, &c) in b.iter().enumerate() {
        if (c == b'y' || c == b'w')
            && i > 0
            && i + 1 < b.len()
            && vowel(b[i - 1])
            && vowel(b[i + 1])
        {
            continue;
        }
        out.push(c as char);
    }
    out
}

/// Every distinct normalized term this item/document should answer to:
/// the folded name itself plus its romaji rendering. Joined into the
/// per-item match document by [`RecipeSearchIndex`].
fn document_variants(text: &str) -> Vec<String> {
    let folded = normalize_kana(text);
    if folded.is_empty() {
        return Vec::new();
    }
    let mut out = vec![folded.clone()];
    let romaji = glide_fold(&kana_to_romaji(&folded));
    if romaji != folded && !romaji.is_empty() {
        out.push(romaji);
    }
    // ASCII fields get the glide fold too so `daia`/`daiya` queries
    // match `daiyamondo` readings by plain substring.
    for field in &mut out {
        if field.is_ascii() {
            *field = glide_fold(field);
        }
    }
    out
}

/// Hiragana vowel class used by the long-vowel collapse: `0=a 1=i 2=u
/// 3=e 4=o`; `None` for `ん`, `っ`, punctuation and non-kana.
fn kana_vowel(c: char) -> Option<u8> {
    Some(match c {
        'あ' | 'か' | 'さ' | 'た' | 'な' | 'は' | 'ま' | 'や' | 'ら' | 'わ' | 'が' | 'ざ'
        | 'だ' | 'ば' | 'ぱ' | 'ぁ' | 'ゃ' | 'ゕ' | 'ゎ' => 0,
        'い' | 'き' | 'し' | 'ち' | 'に' | 'ひ' | 'み' | 'り' | 'ぎ' | 'じ' | 'ぢ' | 'び'
        | 'ぴ' | 'ぃ' | 'ゐ' => 1,
        'う' | 'く' | 'す' | 'つ' | 'ぬ' | 'ふ' | 'む' | 'ゆ' | 'る' | 'ぐ' | 'ず' | 'づ'
        | 'ぶ' | 'ぷ' | 'ぅ' | 'ゅ' | 'ゔ' => 2,
        'え' | 'け' | 'せ' | 'て' | 'ね' | 'へ' | 'め' | 'れ' | 'げ' | 'ぜ' | 'で' | 'べ'
        | 'ぺ' | 'ぇ' | 'ゑ' | 'ゖ' => 3,
        'こ' | 'そ' | 'と' | 'の' | 'ほ' | 'も' | 'よ' | 'ろ' | 'を' | 'ご' | 'ぞ' | 'ど'
        | 'ぼ' | 'ぽ' | 'ぉ' | 'ょ' | 'お' => 4,
        _ => return None,
    })
}

/// Collapses Japanese long-vowel spellings after `normalize_kana`'s
/// folding: a vowel kana continuing the previous kana's vowel is
/// dropped (`とうきょう → ときょ`, `おおきい → おき`, `えい → え`). `ん`,
/// `っ`, kanji and Latin text reset the run. This makes the
/// `おお/おう/おー`, `えい/ええ/えー` spelling families compare equal on
/// both the index and query sides.
fn collapse_long_vowels(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut prev_vowel: Option<u8> = None;
    for &c in chars {
        let vowel = kana_vowel(c);
        let continues = match (vowel, prev_vowel, c) {
            // う continues u and o (おう is the standard long-お).
            (Some(2), Some(2) | Some(4), 'う' | 'ぅ') => true,
            // い continues i and e (えい is the standard long-え).
            (Some(1), Some(1) | Some(3), 'い' | 'ぃ') => true,
            // Every other vowel kana continues only itself.
            (Some(v), Some(p), 'あ' | 'ぁ' | 'え' | 'ぇ' | 'お' | 'ぉ' | 'ゃ' | 'ゅ' | 'ょ') => {
                v == p
            }
            _ => false,
        };
        if continues {
            prev_vowel = vowel;
            continue;
        }
        prev_vowel = vowel;
        out.push(c);
    }
    out
}

/// Voiced/semi-voiced hiragana for a base kana (`か ﾞ → が`, `は ﾟ → ぱ`).
fn voiced_kana(c: char, handakuten: bool) -> Option<char> {
    Some(if handakuten {
        match c {
            'は' => 'ぱ',
            'ひ' => 'ぴ',
            'ふ' => 'ぷ',
            'へ' => 'ぺ',
            'ほ' => 'ぽ',
            _ => return None,
        }
    } else {
        match c {
            'か' => 'が',
            'き' => 'ぎ',
            'く' => 'ぐ',
            'け' => 'げ',
            'こ' => 'ご',
            'さ' => 'ざ',
            'し' => 'じ',
            'す' => 'ず',
            'せ' => 'ぜ',
            'そ' => 'ぞ',
            'た' => 'だ',
            'ち' => 'ぢ',
            'つ' => 'づ',
            'て' => 'で',
            'と' => 'ど',
            'は' => 'ば',
            'ひ' => 'び',
            'ふ' => 'ぶ',
            'へ' => 'べ',
            'ほ' => 'ぼ',
            'う' => 'ゔ',
            _ => return None,
        }
    })
}

/// Romaji for one kana syllable at `chars[i]`, plus how many input
/// chars it consumed (1 normally, 2 for digraphs / small-vowel combos).
fn syllable_romaji(chars: &[char], i: usize) -> (&'static str, usize) {
    let c = chars[i];
    let next = chars.get(i + 1).copied();
    // Youon digraphs: base kana + ゃ/ゅ/ょ.
    if let Some(small) = next {
        if let Some(prefix) = youon_prefix(c) {
            if let Some(vowel) = small_y_vowel(small) {
                return (digraph_romaji(prefix, vowel), 2);
            }
        }
        // Foreign-sound small-vowel combos: ふぁ, うぉ, つぉ, ゔぇ, …
        if let Some(prefix) = combo_prefix(c, small) {
            return (prefix, 2);
        }
    }
    (base_romaji(c), 1)
}

/// Digraph consonant for `きしちにひみりぎじぢびぴ` before ゃ/ゅ/ょ.
fn youon_prefix(c: char) -> Option<&'static str> {
    Some(match c {
        'き' => "ky",
        'し' => "sh",
        'ち' => "ch",
        'に' => "ny",
        'ひ' => "hy",
        'み' => "my",
        'り' => "ry",
        'ぎ' => "gy",
        'じ' | 'ぢ' => "j",
        'び' => "by",
        'ぴ' => "py",
        _ => return None,
    })
}

fn small_y_vowel(c: char) -> Option<char> {
    match c {
        'ゃ' => Some('a'),
        'ゅ' => Some('u'),
        'ょ' => Some('o'),
        _ => None,
    }
}

fn digraph_romaji(prefix: &'static str, vowel: char) -> &'static str {
    match (prefix, vowel) {
        ("ky", 'a') => "kya",
        ("ky", 'u') => "kyu",
        ("ky", 'o') => "kyo",
        ("sh", 'a') => "sha",
        ("sh", 'u') => "shu",
        ("sh", 'o') => "sho",
        ("ch", 'a') => "cha",
        ("ch", 'u') => "chu",
        ("ch", 'o') => "cho",
        ("ny", 'a') => "nya",
        ("ny", 'u') => "nyu",
        ("ny", 'o') => "nyo",
        ("hy", 'a') => "hya",
        ("hy", 'u') => "hyu",
        ("hy", 'o') => "hyo",
        ("my", 'a') => "mya",
        ("my", 'u') => "myu",
        ("my", 'o') => "myo",
        ("ry", 'a') => "rya",
        ("ry", 'u') => "ryu",
        ("ry", 'o') => "ryo",
        ("gy", 'a') => "gya",
        ("gy", 'u') => "gyu",
        ("gy", 'o') => "gyo",
        ("j", 'a') => "ja",
        ("j", 'u') => "ju",
        ("j", 'o') => "jo",
        ("by", 'a') => "bya",
        ("by", 'u') => "byu",
        ("by", 'o') => "byo",
        ("py", 'a') => "pya",
        ("py", 'u') => "pyu",
        ("py", 'o') => "pyo",
        _ => "",
    }
}

/// Foreign-sound combos: `base + small vowel` (`ふぁ → fa`, `うぉ → wo`,
/// `つぉ → tso`, `ゔぇ → ve`, `てぃ → ti`, `しぇ → she`, `いぇ → ye`).
fn combo_prefix(c: char, small: char) -> Option<&'static str> {
    Some(match (c, small) {
        ('う', 'ぁ') => "wa",
        ('う', 'ぃ') => "wi",
        ('う', 'ぇ') => "we",
        ('う', 'ぉ') => "wo",
        ('ゔ', 'ぁ') => "va",
        ('ゔ', 'ぃ') => "vi",
        ('ゔ', 'ぅ') => "vu",
        ('ゔ', 'ぇ') => "ve",
        ('ゔ', 'ぉ') => "vo",
        ('ふ', 'ぁ') => "fa",
        ('ふ', 'ぃ') => "fi",
        ('ふ', 'ぅ') => "fu",
        ('ふ', 'ぇ') => "fe",
        ('ふ', 'ぉ') => "fo",
        ('つ', 'ぁ') => "tsa",
        ('つ', 'ぃ') => "tsi",
        ('つ', 'ぅ') => "tsu",
        ('つ', 'ぇ') => "tse",
        ('つ', 'ぉ') => "tso",
        ('て', 'ぃ') => "ti",
        ('で', 'ぃ') => "di",
        ('と', 'ぅ') => "tu",
        ('ど', 'ぅ') => "du",
        ('い', 'ぇ') => "ye",
        ('し', 'ぇ') => "she",
        ('じ', 'ぇ') => "je",
        ('ち', 'ぇ') => "che",
        _ => return None,
    })
}

/// One-kana Hepburn romaji; non-kana input is returned as itself via a
/// pass-through branch in [`kana_to_romaji`].
fn base_romaji(c: char) -> &'static str {
    match c {
        'あ' => "a",
        'い' => "i",
        'う' => "u",
        'え' => "e",
        'お' => "o",
        'か' => "ka",
        'き' => "ki",
        'く' => "ku",
        'け' => "ke",
        'こ' => "ko",
        'が' => "ga",
        'ぎ' => "gi",
        'ぐ' => "gu",
        'げ' => "ge",
        'ご' => "go",
        'さ' => "sa",
        'し' => "shi",
        'す' => "su",
        'せ' => "se",
        'そ' => "so",
        'ざ' => "za",
        'じ' => "ji",
        'ず' => "zu",
        'ぜ' => "ze",
        'ぞ' => "zo",
        'た' => "ta",
        'ち' => "chi",
        'つ' => "tsu",
        'て' => "te",
        'と' => "to",
        'だ' => "da",
        'ぢ' => "ji",
        'づ' => "zu",
        'で' => "de",
        'ど' => "do",
        'な' => "na",
        'に' => "ni",
        'ぬ' => "nu",
        'ね' => "ne",
        'の' => "no",
        'は' => "ha",
        'ひ' => "hi",
        'ふ' => "fu",
        'へ' => "he",
        'ほ' => "ho",
        'ば' => "ba",
        'び' => "bi",
        'ぶ' => "bu",
        'べ' => "be",
        'ぼ' => "bo",
        'ぱ' => "pa",
        'ぴ' => "pi",
        'ぷ' => "pu",
        'ぺ' => "pe",
        'ぽ' => "po",
        'ま' => "ma",
        'み' => "mi",
        'む' => "mu",
        'め' => "me",
        'も' => "mo",
        'や' => "ya",
        'ゆ' => "yu",
        'よ' => "yo",
        'ら' => "ra",
        'り' => "ri",
        'る' => "ru",
        'れ' => "re",
        'ろ' => "ro",
        'わ' => "wa",
        'を' => "wo",
        'ん' => "n",
        'ゔ' => "vu",
        'ゐ' => "wi",
        'ゑ' => "we",
        'ゕ' => "ka",
        'ゖ' => "ke",
        'ゎ' => "wa",
        'ゃ' => "ya",
        'ゅ' => "yu",
        'ょ' => "yo",
        'ぁ' => "a",
        'ぃ' => "i",
        'ぅ' => "u",
        'ぇ' => "e",
        'ぉ' => "o",
        _ => "",
    }
}

/// Half-width katakana `U+FF66..=U+FF9D` → unvoiced hiragana.
const HALFWIDTH_KANA: [char; 0x38] = [
    'を', 'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'ゃ', 'ゅ', 'ょ', 'っ', 'ー', 'あ', 'い', 'う', 'え', 'お',
    'か', 'き', 'く', 'け', 'こ', 'さ', 'し', 'す', 'せ', 'そ', 'た', 'ち', 'つ', 'て', 'と', 'な',
    'に', 'ぬ', 'ね', 'の', 'は', 'ひ', 'ふ', 'へ', 'ほ', 'ま', 'み', 'む', 'め', 'も', 'や', 'ゆ',
    'よ', 'ら', 'り', 'る', 'れ', 'ろ', 'わ', 'ん',
];

/// Half-width punctuation `U+FF61..=U+FF65` → full-width forms.
const HALFWIDTH_PUNCT: [char; 5] = ['。', '「', '」', '、', '・'];

// ---------------------------------------------------------------------------
// Item catalog & recipes
// ---------------------------------------------------------------------------

/// One catalog item the index searches. `name_ja` may hold any mix of
/// kanji and kana — it is matched raw, kana-normalized, and as romaji.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchItem {
    /// Namespaced id (`"minecraft:diamond"`, `"create:cogwheel"`).
    pub id: String,
    /// `en_us` display name (`"Diamond"`).
    pub name_en: String,
    /// `ja_jp` display name (`"ダイヤモンド"`); may be empty.
    pub name_ja: String,
    /// Owning mod namespace matched by `@mod` tokens.
    pub mod_id: String,
    /// Item tags matched by `#tag` tokens (`"minecraft:planks"`).
    pub tags: Vec<String>,
}

impl SearchItem {
    /// Minimal entry: `mod_id` is derived from the id namespace.
    pub fn new(id: &str, name_en: &str, name_ja: &str) -> Self {
        Self {
            id: id.to_owned(),
            name_en: name_en.to_owned(),
            name_ja: name_ja.to_owned(),
            mod_id: id.split(':').next().unwrap_or("minecraft").to_owned(),
            tags: Vec::new(),
        }
    }
}

/// One recipe input slot: a concrete item, an item tag resolved against
/// the index catalog, or an explicit any-of list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngredientReq {
    /// A specific item id plus count.
    Item { id: String, count: u32 },
    /// Any item carrying `tag`; the planner picks the best-stocked
    /// member deterministically.
    Tag { tag: String, count: u32 },
    /// Any one of the listed concrete item ids (mod-alias lists like
    /// `c:ingots/copper` expanded by the host).
    AnyOf { options: Vec<String>, count: u32 },
}

impl IngredientReq {
    pub fn item(id: &str, count: u32) -> Self {
        Self::Item {
            id: id.to_owned(),
            count: count.max(1),
        }
    }

    pub fn tag(tag: &str, count: u32) -> Self {
        Self::Tag {
            tag: tag.to_owned(),
            count: count.max(1),
        }
    }

    pub fn any_of(options: &[&str], count: u32) -> Self {
        Self::AnyOf {
            options: options.iter().map(|s| (*s).to_owned()).collect(),
            count: count.max(1),
        }
    }

    /// Units consumed per craft.
    pub fn count(&self) -> u32 {
        match self {
            Self::Item { count, .. } | Self::Tag { count, .. } | Self::AnyOf { count, .. } => {
                *count
            }
        }
    }
}

/// One recipe edge in the index: `ingredients → output_item × output_count`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchRecipe {
    pub id: String,
    pub output_item: String,
    pub output_count: u32,
    pub ingredients: Vec<IngredientReq>,
}

impl SearchRecipe {
    pub fn new(id: &str, output_item: &str, output_count: u32) -> Self {
        Self {
            id: id.to_owned(),
            output_item: output_item.to_owned(),
            output_count: output_count.max(1),
            ingredients: Vec::new(),
        }
    }
}

/// Polymorph-style conflict group: every recipe producing the same
/// `output_item`, so the UI can offer a recipe selector when two mods
/// (or a shaped/shapeless pair) collide on one result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeConflictGroup {
    pub output_item: String,
    /// Recipe ids, lexicographically sorted for determinism.
    pub recipe_ids: Vec<String>,
}

/// Match metadata for one field inside an [`ItemDoc`].
#[derive(Clone, Debug, Default)]
struct FieldMeta {
    /// Byte range of this field inside `ItemDoc::text` (end exclusive;
    /// never spans the `\u{1}` separator).
    start: u32,
    end: u32,
    /// ASCII chars present, one bit per byte value `<128`.
    ascii_mask: u128,
    /// Chars in `U+3000..=U+30FF` present (kana + CJK punctuation),
    /// four 64-bit lanes indexed by `(codepoint - 0x3000) / 64`.
    kana_mask: [u64; 4],
    /// Any char outside ASCII and the kana range (kanji, …).
    has_other: bool,
}

/// Per-item precomputed match document: every normalized term variant
/// stored once as a ranged field inside one contiguous `text`, plus
/// whole-document and per-field presence masks so non-matching
/// variants reject on a few bit tests instead of a `contains` scan.
#[derive(Clone, Debug, Default)]
struct ItemDoc {
    text: String,
    fields: Vec<FieldMeta>,
    /// Union of all field masks — the first, cheapest gate.
    ascii_mask: u128,
    kana_mask: [u64; 4],
    /// Sorted dedup chars outside ASCII and the kana range (kanji etc.).
    other: Vec<char>,
}

#[inline(always)]
fn kana_bit(u: u32) -> (usize, u64) {
    let b = u - 0x3000;
    ((b / 64) as usize, 1u64 << (b % 64))
}

impl ItemDoc {
    fn build(item: &SearchItem) -> Self {
        // Only normalized fields go into the document: queries are
        // normalized identically, so a raw `name_ja` copy could never
        // match anything its folded form misses — it would only pad
        // every `contains` scan with dead bytes.
        let mut src: Vec<String> = vec![glide_fold(&item.id.to_lowercase())];
        src.extend(document_variants(&item.name_en));
        src.extend(document_variants(&item.name_ja));
        let mut text = String::new();
        let mut fields = Vec::with_capacity(src.len());
        let mut ascii_mask = 0u128;
        let mut kana_mask = [0u64; 4];
        let mut other: Vec<char> = Vec::new();
        for f in src {
            if f.is_empty() {
                continue;
            }
            let mut fa = 0u128;
            let mut fk = [0u64; 4];
            let mut fo = false;
            for c in f.chars() {
                let u = c as u32;
                if u < 128 {
                    fa |= 1u128 << u;
                } else if (0x3000..=0x30FF).contains(&u) {
                    let (i, bit) = kana_bit(u);
                    fk[i] |= bit;
                } else {
                    fo = true;
                    if !other.contains(&c) {
                        other.push(c);
                    }
                }
            }
            let start = text.len() as u32;
            text.push_str(&f);
            let end = text.len() as u32;
            // Separator keeps ranged fields disjoint from their
            // neighbours inside `text`.
            text.push('\u{1}');
            fields.push(FieldMeta {
                start,
                end,
                ascii_mask: fa,
                kana_mask: fk,
                has_other: fo,
            });
            ascii_mask |= fa;
            for i in 0..4 {
                kana_mask[i] |= fk[i];
            }
        }
        other.sort_unstable();
        Self {
            text,
            fields,
            ascii_mask,
            kana_mask,
            other,
        }
    }
}

/// A query variant plus its precomputed character requirements — the
/// masks let per-item gates run as a handful of ANDs instead of
/// re-walking the variant's chars for every catalog entry.
#[derive(Clone, Debug)]
struct VariantProbe {
    /// The variant text (the `contains` needle on a gate pass).
    text: String,
    /// ASCII chars the needle needs, one bit per byte `<128`.
    ascii: u128,
    /// `U+3000..=U+30FF` chars the needle needs.
    kana: [u64; 4],
    /// Chars outside ASCII/kana the needle needs (kanji, …), sorted.
    other: Vec<char>,
}

impl VariantProbe {
    fn build(text: String) -> Self {
        let mut ascii = 0u128;
        let mut kana = [0u64; 4];
        let mut other: Vec<char> = Vec::new();
        for c in text.chars() {
            let u = c as u32;
            if u < 128 {
                ascii |= 1u128 << u;
            } else if (0x3000..=0x30FF).contains(&u) {
                let (i, bit) = kana_bit(u);
                kana[i] |= bit;
            } else if !other.contains(&c) {
                other.push(c);
            }
        }
        other.sort_unstable();
        Self {
            text,
            ascii,
            kana,
            other,
        }
    }

    /// `true` when every char of the needle is present somewhere in the
    /// document — the cheap whole-item reject before any field scan.
    #[inline(always)]
    fn covers_doc(&self, doc: &ItemDoc) -> bool {
        if self.ascii & !doc.ascii_mask != 0 {
            return false;
        }
        for i in 0..4 {
            if self.kana[i] & !doc.kana_mask[i] != 0 {
                return false;
            }
        }
        self.other.iter().all(|c| doc.other.contains(c))
    }

    /// `true` when every char of the needle is present inside this
    /// field's char set — a necessary condition for `contains`.
    #[inline(always)]
    fn covers_field(&self, field: &FieldMeta) -> bool {
        if self.ascii & !field.ascii_mask != 0 {
            return false;
        }
        for i in 0..4 {
            if self.kana[i] & !field.kana_mask[i] != 0 {
                return false;
            }
        }
        self.other.is_empty() || field.has_other
    }

    /// Rank against the document: `2` when the needle is a substring of
    /// any field, else `0`. Fuzzy equivalence classes are folded at
    /// normalize time ([`glide_fold`], long-vowel collapse), so a
    /// plain substring test is the whole matcher.
    ///
    /// `inline(always)` because the search loop runs at whatever
    /// opt-level the embedder picked — the gate must stay cheap even
    /// unoptimised.
    #[inline(always)]
    fn rank(&self, doc: &ItemDoc) -> u32 {
        if !self.covers_doc(doc) {
            return 0;
        }
        let text = doc.text.as_str();
        for f in &doc.fields {
            if self.covers_field(f) && text[f.start as usize..f.end as usize].contains(&self.text) {
                return 2;
            }
        }
        0
    }
}

#[derive(Clone, Debug)]
enum QueryToken {
    /// Plain text: the precomputed [`query_variants`] as probes.
    Text(Vec<VariantProbe>),
    /// `@namespace` filter (normalized).
    Mod(String),
    /// `#tag` filter (normalized).
    Tag(String),
}

fn parse_query(query: &str) -> Vec<QueryToken> {
    let mut out = Vec::new();
    for raw in query.split_whitespace() {
        if let Some(rest) = raw.strip_prefix('@') {
            let m = normalize_kana(rest);
            if !m.is_empty() {
                out.push(QueryToken::Mod(m));
            }
            continue;
        }
        if let Some(rest) = raw.strip_prefix('#') {
            let t = normalize_kana(rest);
            if !t.is_empty() {
                out.push(QueryToken::Tag(t));
            }
            continue;
        }
        let variants = query_variants(raw);
        if !variants.is_empty() {
            out.push(QueryToken::Text(
                variants.into_iter().map(VariantProbe::build).collect(),
            ));
        }
    }
    out
}

/// The inverted index a JEI/EMI-style overlay searches.
///
/// Fill it with [`add_item`](Self::add_item) for every catalog entry,
/// then [`add_recipe`](Self::add_recipe) for every recipe — registering
/// items first lets `U` (uses) lookups resolve tag ingredients. If the
/// host must interleave loads, call [`rebuild`](Self::rebuild) once at
/// the end to recompute every derived index.
#[derive(Clone, Debug, Default)]
pub struct RecipeSearchIndex {
    items: Vec<SearchItem>,
    docs: Vec<ItemDoc>,
    item_index: HashMap<String, u32>,
    /// `mod_id (normalized) → item indices`.
    mod_index: BTreeMap<String, Vec<u32>>,
    /// `tag id (normalized) → item indices`.
    tag_index: BTreeMap<String, Vec<u32>>,
    recipes: Vec<SearchRecipe>,
    /// `output item id → recipe indices` (the `R` lookup).
    produces: HashMap<String, Vec<u32>>,
    /// `concrete input item id → recipe indices` (the `U` lookup; tag
    /// slots are resolved to member ids at index time).
    uses: HashMap<String, Vec<u32>>,
}

impl RecipeSearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_item(&mut self, item: SearchItem) {
        let idx = self.items.len() as u32;
        self.item_index.insert(item.id.clone(), idx);
        self.mod_index
            .entry(normalize_kana(&item.mod_id))
            .or_default()
            .push(idx);
        for tag in &item.tags {
            self.tag_index
                .entry(normalize_kana(tag))
                .or_default()
                .push(idx);
        }
        self.docs.push(ItemDoc::build(&item));
        self.items.push(item);
    }

    pub fn add_recipe(&mut self, recipe: SearchRecipe) {
        let idx = self.recipes.len() as u32;
        self.produces
            .entry(recipe.output_item.clone())
            .or_default()
            .push(idx);
        for ing in &recipe.ingredients {
            for id in self.ingredient_item_ids(ing) {
                let entry = self.uses.entry(id).or_default();
                if !entry.contains(&idx) {
                    entry.push(idx);
                }
            }
        }
        self.recipes.push(recipe);
    }

    /// Recomputes every derived index. Call once after interleaved
    /// item/recipe loads so tag ingredients resolve against the full
    /// catalog.
    pub fn rebuild(&mut self) {
        self.item_index.clear();
        self.mod_index.clear();
        self.tag_index.clear();
        self.docs.clear();
        for (idx, item) in self.items.iter().enumerate() {
            let idx = idx as u32;
            self.item_index.insert(item.id.clone(), idx);
            self.mod_index
                .entry(normalize_kana(&item.mod_id))
                .or_default()
                .push(idx);
            for tag in &item.tags {
                self.tag_index
                    .entry(normalize_kana(tag))
                    .or_default()
                    .push(idx);
            }
            self.docs.push(ItemDoc::build(item));
        }
        self.produces.clear();
        self.uses.clear();
        for idx in 0..self.recipes.len() {
            let idx = idx as u32;
            let recipe = self.recipes[idx as usize].clone();
            self.produces
                .entry(recipe.output_item.clone())
                .or_default()
                .push(idx);
            for ing in &recipe.ingredients {
                for id in self.ingredient_item_ids(ing) {
                    let entry = self.uses.entry(id).or_default();
                    if !entry.contains(&idx) {
                        entry.push(idx);
                    }
                }
            }
        }
    }

    /// All concrete item ids an ingredient accepts — tag slots expand
    /// to every catalog member, unknown tags expand to nothing.
    fn ingredient_item_ids(&self, ing: &IngredientReq) -> Vec<String> {
        match ing {
            IngredientReq::Item { id, .. } => vec![id.clone()],
            IngredientReq::AnyOf { options, .. } => options.clone(),
            IngredientReq::Tag { tag, .. } => self
                .tag_index
                .get(&normalize_kana(tag))
                .map(|idxs| {
                    idxs.iter()
                        .map(|&i| self.items[i as usize].id.clone())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Catalog entry for `id`, if present.
    pub fn item(&self, id: &str) -> Option<&SearchItem> {
        self.item_index.get(id).map(|&i| &self.items[i as usize])
    }

    pub fn items(&self) -> &[SearchItem] {
        &self.items
    }

    pub fn recipes(&self) -> &[SearchRecipe] {
        &self.recipes
    }

    /// Catalog item ids carrying `tag` (normalized, sorted — the same
    /// deterministic order the craft planner's member pick uses).
    pub fn tag_members(&self, tag: &str) -> Vec<String> {
        self.tag_index
            .get(&normalize_kana(tag))
            .map(|idxs| {
                let mut ids: Vec<String> = idxs
                    .iter()
                    .map(|&i| self.items[i as usize].id.clone())
                    .collect();
                ids.sort();
                ids
            })
            .unwrap_or_default()
    }

    /// JEI-style search: whitespace-separated `@mod`, `#tag` and plain
    /// text tokens AND-ed together. Plain text matches across the item
    /// id, `name_en`, `name_ja`, the kana-normalized reading and its
    /// glide-folded romaji rendering. Results are catalog order.
    pub fn search(&self, query: &str) -> Vec<&SearchItem> {
        let tokens = parse_query(query);
        match tokens.as_slice() {
            [] => self.items.iter().collect(),
            [QueryToken::Mod(m)] => self.items_in_index(&self.mod_index, m),
            [QueryToken::Tag(t)] => self.items_in_index(&self.tag_index, t),
            [QueryToken::Text(variants)] => self.text_search(variants),
            _ => self.search_multi(&tokens),
        }
    }

    /// Single `@`/`#` token: union the posting lists of every index key
    /// containing the normalized token, dedup in catalog order.
    fn items_in_index(&self, index: &BTreeMap<String, Vec<u32>>, token: &str) -> Vec<&SearchItem> {
        let mut idxs: Vec<u32> = Vec::new();
        for (key, posting) in index {
            if key.contains(token) {
                idxs.extend_from_slice(posting);
            }
        }
        idxs.sort_unstable();
        idxs.dedup();
        idxs.into_iter().map(|i| &self.items[i as usize]).collect()
    }

    /// Single text token: one pass, no score array.
    fn text_search(&self, probes: &[VariantProbe]) -> Vec<&SearchItem> {
        let n = self.items.len();
        let mut hits: Vec<u32> = Vec::new();
        let docs = &self.docs;
        let mut i = 0usize;
        while i < n {
            let doc = &docs[i];
            let mut pi = 0usize;
            while pi < probes.len() {
                if probes[pi].rank(doc) != 0 {
                    hits.push(i as u32);
                    break;
                }
                pi += 1;
            }
            i += 1;
        }
        hits.into_iter().map(|i| &self.items[i as usize]).collect()
    }

    /// Multi-token AND: score array + counting-bucket ordering.
    fn search_multi(&self, tokens: &[QueryToken]) -> Vec<&SearchItem> {
        let n = self.items.len();
        // One byte per item: `DEAD` = eliminated, otherwise the
        // accumulated text-match rank. A single array keeps the hot
        // loop to one bounds check and one cache line per item.
        const DEAD: u8 = u8::MAX;
        let mut score = vec![0u8; n];
        for token in tokens {
            match token {
                QueryToken::Mod(m) => {
                    let mut allowed = vec![false; n];
                    for (key, idxs) in &self.mod_index {
                        if key.contains(m.as_str()) {
                            for &i in idxs {
                                allowed[i as usize] = true;
                            }
                        }
                    }
                    for i in 0..n {
                        if !allowed[i] {
                            score[i] = DEAD;
                        }
                    }
                }
                QueryToken::Tag(t) => {
                    let mut allowed = vec![false; n];
                    for (key, idxs) in &self.tag_index {
                        if key.contains(t.as_str()) {
                            for &i in idxs {
                                allowed[i as usize] = true;
                            }
                        }
                    }
                    for i in 0..n {
                        if !allowed[i] {
                            score[i] = DEAD;
                        }
                    }
                }
                QueryToken::Text(probes) => {
                    let docs = &self.docs;
                    let mut i = 0usize;
                    while i < n {
                        if score[i] == DEAD {
                            i += 1;
                            continue;
                        }
                        let doc = &docs[i];
                        let mut best = 0u32;
                        let mut pi = 0usize;
                        while pi < probes.len() {
                            let r = probes[pi].rank(doc);
                            if r > best {
                                // 2 is the ceiling — stop checking
                                // variants the moment it is reached.
                                best = r;
                                if best == 2 {
                                    break;
                                }
                            }
                            pi += 1;
                        }
                        if best == 0 {
                            score[i] = DEAD;
                        } else {
                            score[i] = score[i].saturating_add(best as u8);
                        }
                        i += 1;
                    }
                }
            }
        }
        // Ranks are tiny (≤2 per text token), so order by counting
        // buckets — O(n), deterministic catalog order inside a rank,
        // and no comparator calls in the hot path.
        let max_rank = score
            .iter()
            .filter(|&&s| s != DEAD)
            .max()
            .copied()
            .unwrap_or(0);
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); usize::from(max_rank) + 1];
        let mut total = 0usize;
        for i in 0..n {
            if score[i] != DEAD {
                buckets[usize::from(score[i])].push(i as u32);
                total += 1;
            }
        }
        let mut out = Vec::with_capacity(total);
        for bucket in buckets.iter().rev() {
            for &i in bucket {
                out.push(&self.items[i as usize]);
            }
        }
        out
    }

    /// `R` lookup: every recipe producing `item_id`, in load order.
    /// O(1) map hit plus the result fan-out.
    pub fn recipes_producing(&self, item_id: &str) -> Vec<&SearchRecipe> {
        self.produces
            .get(item_id)
            .map(|idxs| idxs.iter().map(|&i| &self.recipes[i as usize]).collect())
            .unwrap_or_default()
    }

    /// `U` lookup: every recipe consuming `item_id` — direct slots and
    /// tag slots whose membership included the item at index time.
    pub fn recipes_using(&self, item_id: &str) -> Vec<&SearchRecipe> {
        self.uses
            .get(item_id)
            .map(|idxs| idxs.iter().map(|&i| &self.recipes[i as usize]).collect())
            .unwrap_or_default()
    }

    /// The Polymorph-style conflict group for `output_item`: `Some`
    /// only when two or more recipes produce it.
    pub fn conflict_group(&self, output_item: &str) -> Option<RecipeConflictGroup> {
        let idxs = self.produces.get(output_item)?;
        if idxs.len() < 2 {
            return None;
        }
        let mut recipe_ids: Vec<String> = idxs
            .iter()
            .map(|&i| self.recipes[i as usize].id.clone())
            .collect();
        recipe_ids.sort();
        Some(RecipeConflictGroup {
            output_item: output_item.to_owned(),
            recipe_ids,
        })
    }

    /// Every conflict group, ordered by output item id.
    pub fn conflict_groups(&self) -> Vec<RecipeConflictGroup> {
        let mut outputs: Vec<&String> = self
            .produces
            .iter()
            .filter(|(_, idxs)| idxs.len() > 1)
            .map(|(item, _)| item)
            .collect();
        outputs.sort();
        outputs
            .into_iter()
            .filter_map(|item| self.conflict_group(item))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Nearby-chest 1-click craft planner
// ---------------------------------------------------------------------------

/// One item stack sitting in a slot (player inventory or chest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotStack {
    pub item_id: String,
    pub count: u32,
}

impl SlotStack {
    pub fn new(item_id: &str, count: u32) -> Self {
        Self {
            item_id: item_id.to_owned(),
            count,
        }
    }
}

/// A chest near the crafting site: its Manhattan/spherical distance in
/// blocks and its slot contents in slot order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NearbyChest {
    pub distance_blocks: f32,
    pub slots: Vec<Option<SlotStack>>,
}

impl NearbyChest {
    pub fn new(distance_blocks: f32, slots: Vec<Option<SlotStack>>) -> Self {
        Self {
            distance_blocks,
            slots,
        }
    }
}

/// Where a [`SlotPull`] takes items from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PullSource {
    /// The player's own inventory — always drained first.
    PlayerInventory,
    /// `nearby_chests[i]` in the caller's original array order.
    Chest(u32),
}

/// One atomic transfer inside a [`CraftTransferPlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotPull {
    pub source: PullSource,
    /// Slot index inside the source container.
    pub slot: u32,
    pub item_id: String,
    /// Units to move — never more than the slot holds.
    pub count: u32,
}

/// Whether a recipe can run right now, and where the materials are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CraftAvailability {
    /// Everything needed already sits in the player inventory.
    CraftableFromInventory,
    /// Craftable, but at least one unit must come out of a nearby chest.
    CraftableWithChests,
    /// Not craftable: `(display id, shortfall)` per missing ingredient
    /// — the resolved item id, or `"#tag"` when the tag matched nothing
    /// in the catalog.
    MissingIngredients(Vec<(String, u32)>),
}

/// The atomic, deterministic result of a successful plan: the pulls a
/// 1-click craft button performs, plus the output the craft yields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CraftTransferPlan {
    /// Pulls in deterministic order: player inventory slots first
    /// (slot order), then chests nearest-first (slot order within).
    pub pulls: Vec<SlotPull>,
    pub output_item: String,
    /// Output units per craft.
    pub output_count: u32,
    /// Number of crafts this plan performs — `1` for a normal click,
    /// the materials-and-stack-limited maximum for shift-click.
    pub craft_count: u32,
}

impl CraftTransferPlan {
    /// Total output units: `output_count × craft_count`.
    pub fn total_output(&self) -> u32 {
        self.output_count.saturating_mul(self.craft_count)
    }

    /// Total units pulled of `item_id` across all sources.
    pub fn pulled(&self, item_id: &str) -> u32 {
        self.pulls
            .iter()
            .filter(|p| p.item_id == item_id)
            .map(|p| p.count)
            .sum()
    }
}

/// The planner's verdict: the availability classification plus, when
/// craftable, the ready-to-execute [`CraftTransferPlan`].
#[derive(Clone, Debug, PartialEq)]
pub struct CraftPlanResult {
    pub availability: CraftAvailability,
    /// `Some` iff availability is not `MissingIngredients`.
    pub plan: Option<CraftTransferPlan>,
}

/// One ingredient resolved to a concrete item id for material math.
#[derive(Clone, Debug)]
struct ResolvedReq {
    /// Display id for missing-ingredient reports.
    need_key: String,
    /// `None` when the ingredient can't resolve (unknown tag / empty
    /// options) — always counts as missing.
    item_id: Option<String>,
    /// Units per craft.
    per_craft: u32,
}

/// Plans 1-click crafts that pull from the player inventory and every
/// chest within reach — the logic behind the JEI `+` button plus the
/// Polymorph-style "grab it from the chest next to you" extension.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NearbyChestCraftPlanner {
    /// Maximum chest distance in blocks; chests farther away (or with a
    /// non-finite distance) never contribute.
    pub reach_blocks: f32,
}

impl Default for NearbyChestCraftPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl NearbyChestCraftPlanner {
    /// Vanilla interaction reach.
    pub const DEFAULT_REACH: f32 = 6.0;

    pub fn new() -> Self {
        Self {
            reach_blocks: Self::DEFAULT_REACH,
        }
    }

    pub fn with_reach(reach_blocks: f32) -> Self {
        Self { reach_blocks }
    }

    /// Availability + transfer plan for exactly `crafts` crafts of a
    /// recipe described by `ingredients` → `output_item × output_count`.
    ///
    /// Deterministic: tag/any-of slots pick the member with the most
    /// units available (ties → lexicographically smallest id), pulls
    /// drain the player inventory in slot order then chests
    /// nearest-first, and no pull ever exceeds a slot's contents. The
    /// caller's slots are never mutated — the returned
    /// [`CraftTransferPlan`] is the whole transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn plan(
        &self,
        index: &RecipeSearchIndex,
        ingredients: &[IngredientReq],
        player_slots: &[Option<SlotStack>],
        nearby_chests: &[NearbyChest],
        output_item: &str,
        output_count: u32,
        crafts: u32,
    ) -> CraftPlanResult {
        let order = self.chest_order(nearby_chests);
        let inv_totals = totals(player_slots.iter().flatten());
        let chest_totals = totals(
            order
                .iter()
                .flat_map(|&i| nearby_chests[i].slots.iter().flatten()),
        );
        let reqs = resolve_ingredients(index, ingredients, &inv_totals, &chest_totals);
        let availability = classify(&reqs, crafts, &inv_totals, &chest_totals);
        match availability {
            CraftAvailability::MissingIngredients(_) => CraftPlanResult {
                availability,
                plan: None,
            },
            _ => CraftPlanResult {
                availability: availability.clone(),
                plan: Some(self.transfer_plan(
                    &reqs,
                    crafts,
                    player_slots,
                    nearby_chests,
                    &order,
                    output_item,
                    output_count,
                    availability == CraftAvailability::CraftableFromInventory,
                )),
            },
        }
    }

    /// Shift-craft variant: as many crafts as the materials and the
    /// output item's max stack size allow (vanilla shift-click crafts a
    /// full stack, never more). `craft_count` is `0`-free — when even
    /// one craft is impossible the result is
    /// [`CraftAvailability::MissingIngredients`] for a single craft.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_max(
        &self,
        index: &RecipeSearchIndex,
        ingredients: &[IngredientReq],
        player_slots: &[Option<SlotStack>],
        nearby_chests: &[NearbyChest],
        output_item: &str,
        output_count: u32,
        output_max_stack: u32,
    ) -> CraftPlanResult {
        let order = self.chest_order(nearby_chests);
        let inv_totals = totals(player_slots.iter().flatten());
        let chest_totals = totals(
            order
                .iter()
                .flat_map(|&i| nearby_chests[i].slots.iter().flatten()),
        );
        let reqs = resolve_ingredients(index, ingredients, &inv_totals, &chest_totals);
        let stack_cap = output_max_stack.checked_div(output_count).unwrap_or(0);
        let crafts = min_crafts(&reqs, &inv_totals, &chest_totals).min(stack_cap);
        if crafts == 0 {
            return CraftPlanResult {
                availability: classify(&reqs, 1, &inv_totals, &chest_totals),
                plan: None,
            };
        }
        let availability = classify(&reqs, crafts, &inv_totals, &chest_totals);
        CraftPlanResult {
            availability: availability.clone(),
            plan: Some(self.transfer_plan(
                &reqs,
                crafts,
                player_slots,
                nearby_chests,
                &order,
                output_item,
                output_count,
                availability == CraftAvailability::CraftableFromInventory,
            )),
        }
    }

    /// Availability classification for `crafts` crafts, without
    /// computing the transfer plan.
    #[allow(clippy::too_many_arguments)]
    pub fn availability(
        &self,
        index: &RecipeSearchIndex,
        ingredients: &[IngredientReq],
        player_slots: &[Option<SlotStack>],
        nearby_chests: &[NearbyChest],
        crafts: u32,
    ) -> CraftAvailability {
        let order = self.chest_order(nearby_chests);
        let inv_totals = totals(player_slots.iter().flatten());
        let chest_totals = totals(
            order
                .iter()
                .flat_map(|&i| nearby_chests[i].slots.iter().flatten()),
        );
        let reqs = resolve_ingredients(index, ingredients, &inv_totals, &chest_totals);
        classify(&reqs, crafts, &inv_totals, &chest_totals)
    }

    /// Convenience wrapper over [`plan`](Self::plan) taking the recipe
    /// value itself.
    pub fn plan_recipe(
        &self,
        index: &RecipeSearchIndex,
        recipe: &SearchRecipe,
        player_slots: &[Option<SlotStack>],
        nearby_chests: &[NearbyChest],
        crafts: u32,
    ) -> CraftPlanResult {
        self.plan(
            index,
            &recipe.ingredients,
            player_slots,
            nearby_chests,
            &recipe.output_item,
            recipe.output_count,
            crafts,
        )
    }

    /// In-reach chest indices ordered by `(distance, original index)`;
    /// out-of-reach and non-finite entries are dropped.
    fn chest_order(&self, chests: &[NearbyChest]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..chests.len())
            .filter(|&i| {
                chests[i].distance_blocks.is_finite()
                    && chests[i].distance_blocks <= self.reach_blocks
            })
            .collect();
        order.sort_by(|&a, &b| {
            chests[a]
                .distance_blocks
                .partial_cmp(&chests[b].distance_blocks)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        order
    }

    /// Builds the pull list for `crafts` crafts. Caller guarantees the
    /// materials exist; pulls therefore never exceed a slot's contents.
    #[allow(clippy::too_many_arguments)]
    fn transfer_plan(
        &self,
        reqs: &[ResolvedReq],
        crafts: u32,
        player_slots: &[Option<SlotStack>],
        chests: &[NearbyChest],
        order: &[usize],
        output_item: &str,
        output_count: u32,
        inventory_only: bool,
    ) -> CraftTransferPlan {
        let mut pulls = Vec::new();
        for req in reqs {
            let Some(item_id) = &req.item_id else {
                continue;
            };
            let mut need = req.per_craft.saturating_mul(crafts);
            // Player inventory first, slot order.
            for (slot, stack) in player_slots.iter().enumerate() {
                if need == 0 {
                    break;
                }
                let Some(stack) = stack else { continue };
                if &stack.item_id != item_id {
                    continue;
                }
                let take = stack.count.min(need);
                if take > 0 {
                    pulls.push(SlotPull {
                        source: PullSource::PlayerInventory,
                        slot: slot as u32,
                        item_id: item_id.clone(),
                        count: take,
                    });
                    need -= take;
                }
            }
            if inventory_only {
                continue;
            }
            // Then in-reach chests, nearest first, slot order within.
            for &ci in order {
                if need == 0 {
                    break;
                }
                for (slot, stack) in chests[ci].slots.iter().enumerate() {
                    if need == 0 {
                        break;
                    }
                    let Some(stack) = stack else { continue };
                    if &stack.item_id != item_id {
                        continue;
                    }
                    let take = stack.count.min(need);
                    if take > 0 {
                        pulls.push(SlotPull {
                            source: PullSource::Chest(ci as u32),
                            slot: slot as u32,
                            item_id: item_id.clone(),
                            count: take,
                        });
                        need -= take;
                    }
                }
            }
        }
        CraftTransferPlan {
            pulls,
            output_item: output_item.to_owned(),
            output_count,
            craft_count: crafts,
        }
    }
}

/// Total units per item id across a slot iterator.
fn totals<'a>(stacks: impl Iterator<Item = &'a SlotStack>) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for s in stacks {
        *out.entry(s.item_id.clone()).or_default() += u64::from(s.count);
    }
    out
}

/// Resolves tag/any-of slots to a concrete item id: the member with
/// the most units across inventory + in-reach chests, ties broken by
/// lexicographically smallest id. Requirements aggregate by resolved
/// id in first-seen order.
fn resolve_ingredients(
    index: &RecipeSearchIndex,
    ingredients: &[IngredientReq],
    inv_totals: &HashMap<String, u64>,
    chest_totals: &HashMap<String, u64>,
) -> Vec<ResolvedReq> {
    let mut resolved: Vec<ResolvedReq> = Vec::new();
    let mut slots: HashMap<String, usize> = HashMap::new();
    let available = |id: &str| -> u64 {
        inv_totals.get(id).copied().unwrap_or(0) + chest_totals.get(id).copied().unwrap_or(0)
    };
    for ing in ingredients {
        let (need_key, item_id, per_craft) = match ing {
            IngredientReq::Item { id, count } => (id.clone(), Some(id.clone()), *count),
            IngredientReq::Tag { tag, count } => {
                let members = index.tag_members(tag);
                if members.is_empty() {
                    (format!("#{tag}"), None, *count)
                } else {
                    let pick = members
                        .iter()
                        .max_by(|a, b| available(a).cmp(&available(b)).then_with(|| b.cmp(a)))
                        .cloned();
                    (pick.clone().unwrap_or_default(), pick, *count)
                }
            }
            IngredientReq::AnyOf { options, count } => {
                if options.is_empty() {
                    ("<any_of>".to_owned(), None, *count)
                } else {
                    let mut sorted = options.clone();
                    sorted.sort();
                    let pick = sorted
                        .iter()
                        .max_by(|a, b| available(a).cmp(&available(b)).then_with(|| b.cmp(a)))
                        .cloned();
                    (pick.clone().unwrap_or_default(), pick, *count)
                }
            }
        };
        if let Some(id) = &item_id {
            if let Some(&slot) = slots.get(id) {
                resolved[slot].per_craft += per_craft;
                continue;
            }
            slots.insert(id.clone(), resolved.len());
        }
        resolved.push(ResolvedReq {
            need_key,
            item_id,
            per_craft,
        });
    }
    resolved
}

/// Total units available for a resolved requirement; `u64::MAX` for
/// unresolvable slots is never used — they report as `0`.
fn req_available(req: &ResolvedReq, totals: &HashMap<String, u64>) -> u64 {
    req.item_id
        .as_ref()
        .and_then(|id| totals.get(id).copied())
        .unwrap_or(0)
}

/// How many crafts the requirements allow with inventory + chests.
fn min_crafts(
    reqs: &[ResolvedReq],
    inv_totals: &HashMap<String, u64>,
    chest_totals: &HashMap<String, u64>,
) -> u32 {
    let mut best = u32::MAX;
    for req in reqs {
        if req.per_craft == 0 {
            continue;
        }
        let total = req_available(req, inv_totals) + req_available(req, chest_totals);
        let crafts = total / u64::from(req.per_craft);
        best = best.min(crafts.min(u64::from(u32::MAX)) as u32);
    }
    best
}

/// The three-way availability classification for `crafts` crafts.
fn classify(
    reqs: &[ResolvedReq],
    crafts: u32,
    inv_totals: &HashMap<String, u64>,
    chest_totals: &HashMap<String, u64>,
) -> CraftAvailability {
    let crafts = u64::from(crafts.max(1));
    let mut missing: Vec<(String, u32)> = Vec::new();
    let mut needs_chest = false;
    for req in reqs {
        let wanted = u64::from(req.per_craft).saturating_mul(crafts);
        let inv = req_available(req, inv_totals);
        let all = inv + req_available(req, chest_totals);
        if all < wanted {
            missing.push((
                req.need_key.clone(),
                (wanted - all).min(u64::from(u32::MAX)) as u32,
            ));
        } else if inv < wanted {
            needs_chest = true;
        }
    }
    if !missing.is_empty() {
        CraftAvailability::MissingIngredients(missing)
    } else if needs_chest {
        CraftAvailability::CraftableWithChests
    } else {
        CraftAvailability::CraftableFromInventory
    }
}
