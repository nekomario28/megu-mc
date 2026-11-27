// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! The native server console mode engine: the same smart, high-signal
//! dashboard `tools/megu_tui.py` wraps the server in, reimplemented on pure
//! `std` so the shipped `megu` binary needs no Python at all.
//!
//! The engine is split into deterministic pieces that never touch the clock,
//! the network or `termios`:
//!
//! * [`ConsoleModeOptions`] + [`resolve_console_mode`] — the `--cli` /
//!   `--plain` / `--no-tui` / `--nogui` / `--tui` / `--no-banner` /
//!   `--log-filter` flag resolver and the fail-closed TTY/`TERM`/`NO_COLOR`
//!   capability check. Interactive TTY picks [`ServerConsoleMode::Tui`];
//!   pipes, CI, Docker, systemd and Pterodactyl consoles always get
//!   [`ServerConsoleMode::PlainCli`], even when `--tui` was passed.
//! * [`ServerTuiState`] — folds raw server log lines into rolling TPS / MSPT /
//!   RAM / CPU sparkline history, the player list, adapted-mod bookkeeping,
//!   sleep/hotspot/quarantine guard state, and a categorized, noise-filtered
//!   scrolling log with the classic MEGU startup banner.
//! * [`ServerTuiState::render_frame`] — a pure function of the state plus
//!   `(cols, rows, color, ascii)`: a single-line status bar (MEGU pill, state,
//!   TPS, RAM in GB, CPU %, players, chunks, mods), a clean section divider,
//!   the scrolling log / players / mods body, an always-ready `>` command
//!   prompt, and a compact key-hint footer. Exactly `rows` lines, every one
//!   at most `cols` visible columns.
//! * [`TuiKey`] / [`ServerTuiState::handle_key`] /
//!   [`ServerTuiState::feed_input`] — always-ready console input (type any
//!   command and press Enter, with Up/Down history, Left/Right editing, and
//!   Tab completion) plus non-interfering shortcuts: `F2`/`Ctrl+F` filter
//!   cycle, `F3`/`Ctrl+P` players, `F4`/`Ctrl+G` mods, `PgUp`/`PgDn` log
//!   scroll, and `Ctrl+C` graceful stop.

use std::collections::{BTreeMap, VecDeque};

/// Rolling metric history kept for sparklines.
const SAMPLE_HISTORY: usize = 120;
/// Scrolling log ring capacity.
const LOG_CAPACITY: usize = 400;
/// Hotspot/quarantine alerts remembered for the guard card.
const HOTSPOT_CAPACITY: usize = 20;
/// Command-line history kept for Up/Down recall.
const HISTORY_CAPACITY: usize = 64;

/// Common server commands offered by Tab completion on the console prompt.
const COMMAND_COMPLETIONS: &[&str] = &[
    "help",
    "list",
    "say ",
    "stop",
    "status",
    "save-all",
    "op ",
    "deop ",
    "kick ",
    "ban ",
    "pardon ",
    "whitelist ",
    "gamemode ",
    "tp ",
    "time set ",
    "weather ",
    "megu status",
    "megu audit",
    "megu backup status",
];

const SPARK_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const SPARK_ASCII: [char; 8] = ['.', '_', '-', '=', '+', '*', '#', '@'];

/// The classic six-line block-letter MEGU startup banner, written once into
/// the scrolling log at boot so it scrolls away like Mohist / Paper / Purpur.
pub const BANNER_UNICODE: [&str; 6] = [
    "███╗   ███╗███████╗ ██████╗ ██╗   ██╗",
    "████╗ ████║██╔════╝██╔════╝ ██║   ██║",
    "██╔████╔██║█████╗  ██║  ███╗██║   ██║",
    "██║╚██╔╝██║██╔══╝  ██║   ██║██║   ██║",
    "██║ ╚═╝ ██║███████╗╚██████╔╝╚██████╔╝",
    "╚═╝     ╚═╝╚══════╝ ╚═════╝  ╚═════╝ ",
];

/// The pure-ASCII six-line MEGU startup banner.
pub const BANNER_ASCII: [&str; 6] = [
    "##   ## ######  ####  ##   ##",
    "### ### ##     ##  ## ##   ##",
    "## # ## ####   ##     ##   ##",
    "##   ## ##     ## ### ##   ##",
    "##   ## ##     ##  ##  ## ## ",
    "##   ## ######  ####    ###  ",
];

/// The six startup banner rows for the requested glyph set.
pub fn startup_banner(ascii: bool) -> &'static [&'static str; 6] {
    if ascii {
        &BANNER_ASCII
    } else {
        &BANNER_UNICODE
    }
}

/// Whether a captured log line is one of the startup banner rows.
pub fn is_banner_line(line: &str) -> bool {
    let trimmed = line.trim_end();
    BANNER_UNICODE
        .iter()
        .chain(BANNER_ASCII.iter())
        .any(|banner| banner.trim_end() == trimmed)
}

// ---------------------------------------------------------------- ANSI text

const RESET: &str = "\u{1b}[0m";
const BG_BAR: &str = "\u{1b}[48;5;235m";
const BG_INPUT: &str = "\u{1b}[48;5;236m";

fn color_code(name: &str) -> &'static str {
    match name {
        "bold" => "\u{1b}[1m",
        "dim" => "\u{1b}[38;5;240m",
        "border" => "\u{1b}[38;5;239m",
        "lavender" | "purple" => "\u{1b}[38;5;183m",
        "lavender_bold" => "\u{1b}[1;38;5;183m",
        "rose" | "pink" => "\u{1b}[38;5;217m",
        "sky" | "cyan" => "\u{1b}[38;5;153m",
        "mint" | "green" => "\u{1b}[38;5;157m",
        "mint_bold" => "\u{1b}[1;38;5;157m",
        "amber" | "yellow" => "\u{1b}[38;5;222m",
        "red" => "\u{1b}[1;38;5;210m",
        "grey" | "muted" => "\u{1b}[38;5;246m",
        "white" => "\u{1b}[1;38;5;255m",
        "text" => "\u{1b}[38;5;254m",
        "brand" => "\u{1b}[1;38;5;255;48;5;54m",
        "pill_ok" => "\u{1b}[1;38;5;157;48;5;22m",
        "pill_sleep" => "\u{1b}[1;38;5;153;48;5;24m",
        "pill_busy" => "\u{1b}[1;38;5;222;48;5;58m",
        "pill_alert" => "\u{1b}[1;38;5;217;48;5;52m",
        "key_chip" => "\u{1b}[1;38;5;183;48;5;238m",
        "key_active" => "\u{1b}[1;38;5;255;48;5;54m",
        "cursor" => "\u{1b}[38;5;233;48;5;183m",
        _ => RESET,
    }
}

/// `text` wrapped in a 256-color ANSI paint; a plain copy when `color` is off.
fn paint(text: &str, name: &str, color: bool) -> String {
    if !color {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 18);
    out.push_str(color_code(name));
    out.push_str(text);
    out.push_str(RESET);
    out
}

/// Fill a line to `cols` with a subtle tonal surface background band when
/// `color` is enabled, restoring `bg` after every inner reset so pills and
/// colored text sit seamlessly on the bar.
fn paint_bar(content: &str, cols: usize, bg: &str, color: bool) -> String {
    let clipped = clip(content, cols);
    if !color {
        return clipped;
    }
    let body = clipped.strip_suffix(RESET).unwrap_or(&clipped);
    let patched = body.replace(RESET, &format!("{RESET}{bg}"));
    format!("{bg}{patched}{RESET}")
}

/// Format MiB as human-friendly GB (e.g. `524.0` → `"0.51 GB"`, `None` → `"-- GB"`).
fn format_ram_gb(mib: Option<f64>) -> String {
    match mib {
        Some(v) if v.is_finite() && v >= 0.0 => format!("{:.2} GB", v / 1024.0),
        _ => "-- GB".to_string(),
    }
}

/// Remove CSI escape sequences (`ESC [` params intermediates final).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for inner in chars.by_ref() {
                if ('@'..='~').contains(&inner) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Visible column count of a string that may carry ANSI sequences.
pub fn vlen(text: &str) -> usize {
    strip_ansi(text).chars().count()
}

/// Fit `text` to exactly `width` visible columns, preserving ANSI codes.
///
/// Truncation can drop a trailing reset; one is re-appended so colors cannot
/// bleed into whatever the terminal draws next.
pub fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut visible = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            out.push(c);
            out.push('[');
            for inner in chars.by_ref() {
                out.push(inner);
                if ('@'..='~').contains(&inner) {
                    break;
                }
            }
            continue;
        }
        if visible >= width {
            break;
        }
        out.push(c);
        visible += 1;
    }
    while visible < width {
        out.push(' ');
        visible += 1;
    }
    if out.contains('\u{1b}') && !out.ends_with(RESET) {
        out.push_str(RESET);
    }
    out
}

/// Truncate or pad a plain (no-ANSI) string to `width` columns.
fn fit_plain(text: &str, width: usize, fill: char) -> String {
    let visible = text.chars().count();
    if visible >= width {
        return text.chars().take(width).collect();
    }
    let mut out = String::with_capacity(width);
    out.push_str(text);
    for _ in visible..width {
        out.push(fill);
    }
    out
}

/// Map every non-ASCII char to `?` — the `--ascii` glyph policy.
fn asciify(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii() { c } else { '?' })
        .collect()
}

/// `1,284`-style grouping for chunk counts.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Render the last `width` values as a sparkline of exactly `width` cells.
///
/// `low`/`high` fix the scale; `None` derives that bound from the finite data.
/// Values clamp to the ends, NaN renders as a gap, infinities pin the top and
/// bottom cells, and a flat series sits on the middle rung so "no variation"
/// does not look like "zero".
pub fn sparkline(
    values: &VecDeque<f64>,
    width: usize,
    low: Option<f64>,
    high: Option<f64>,
    ascii: bool,
) -> String {
    let chars: &[char; 8] = if ascii { &SPARK_ASCII } else { &SPARK_BLOCKS };
    if width == 0 {
        return String::new();
    }
    let skip = values.len().saturating_sub(width);
    let vals: Vec<f64> = values.iter().copied().skip(skip).collect();
    let numbers: Vec<f64> = vals.iter().copied().filter(|v| v.is_finite()).collect();
    let low = low.unwrap_or_else(|| numbers.iter().copied().fold(f64::INFINITY, f64::min));
    let low = if low.is_finite() { low } else { 0.0 };
    let high = high.unwrap_or_else(|| numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max));
    let high = if high.is_finite() { high } else { low };
    let span = high - low;
    let mid = chars.len() / 2;
    let mut out = String::with_capacity(width * 3);
    for _ in vals.len()..width {
        out.push(' ');
    }
    for v in vals {
        if v.is_nan() {
            out.push(' ');
        } else if v == f64::INFINITY {
            out.push(chars[chars.len() - 1]);
        } else if v == f64::NEG_INFINITY {
            out.push(chars[0]);
        } else if span <= 0.0 {
            out.push(chars[mid]);
        } else {
            let index = ((v - low) / span * (chars.len() - 1) as f64) as isize;
            let index = index.clamp(0, chars.len() as isize - 1) as usize;
            out.push(chars[index]);
        }
    }
    out
}

// ------------------------------------------------------------- small parser

/// Word characters in the Python `\b` sense: ASCII alnum plus underscore.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `word` appears in `haystack` on word boundaries.
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack.match_indices(word).any(|(index, _)| {
        let before = haystack[..index].chars().next_back();
        let after = haystack[index + word.len()..].chars().next();
        before.is_none_or(|c| !is_word_char(c)) && after.is_none_or(|c| !is_word_char(c))
    })
}

/// A walking cursor over a line for hand-rolled log-shape matching.
struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Cursor { text, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    /// Skip whitespace; `false` when none was present (regex `\s*`).
    fn skip_ws(&mut self) -> bool {
        let start = self.pos;
        while self.rest().starts_with(|c: char| c.is_whitespace()) {
            self.pos += 1;
        }
        self.pos > start
    }

    /// Consume `lit` at the cursor; false (no movement) when absent.
    fn take(&mut self, lit: &str) -> bool {
        if self.rest().starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    /// Consume `lit` after required whitespace (regex `\s+`).
    fn take_ws1(&mut self, lit: &str) -> bool {
        self.skip_ws() && self.take(lit)
    }

    /// The next non-whitespace run (regex `\S+`).
    fn next_word(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        while self.rest().starts_with(|c: char| !c.is_whitespace()) {
            self.pos += 1;
        }
        (self.pos > start).then(|| &self.text[start..self.pos])
    }

    /// A run of `0-9.` bytes parsed as a float; `None` when absent or junk.
    fn take_float(&mut self) -> Option<f64> {
        let start = self.pos;
        while self
            .rest()
            .starts_with(|c: char| c.is_ascii_digit() || c == '.')
        {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        match self.text[start..self.pos].parse::<f64>() {
            Ok(value) => Some(value),
            Err(_) => {
                self.pos = start;
                None
            }
        }
    }

    /// A signed decimal integer (`-?\d+`).
    fn take_int(&mut self) -> Option<i64> {
        let start = self.pos;
        let _ = self.take("-");
        let digits_start = self.pos;
        while self.rest().starts_with(|c: char| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            self.pos = start;
            return None;
        }
        self.text[start..self.pos].parse::<i64>().ok()
    }

    /// Required whitespace then a float — `\s+(\d+(\.\d+)?)`.
    fn take_ws1_float(&mut self) -> Option<f64> {
        if !self.skip_ws() {
            return None;
        }
        self.take_float()
    }

    /// Required whitespace then a signed integer — `\s+(-?\d+)`.
    fn take_ws1_int(&mut self) -> Option<i64> {
        if !self.skip_ws() {
            return None;
        }
        self.take_int()
    }
}

/// `marker<float>` where a `=`/`:` separator may sit between them.
fn float_after_marker(line: &str, marker: &str, separators: &str) -> Option<f64> {
    let mut search = 0;
    while let Some(offset) = line[search..].find(marker) {
        let mut cursor = Cursor::new(line);
        cursor.pos = search + offset + marker.len();
        cursor.skip_ws();
        let sep_ok = separators.is_empty()
            || cursor
                .rest()
                .chars()
                .next()
                .is_some_and(|c| separators.contains(c));
        if sep_ok {
            if !separators.is_empty() {
                cursor.pos += 1;
            }
            cursor.skip_ws();
            if let Some(value) = cursor.take_float() {
                return Some(value);
            }
        }
        search += offset + 1;
    }
    None
}

/// `<marker>` on a word boundary, then a float — `\bclients=(\d+)`.
fn bounded_float_after_marker(line: &str, marker: &str) -> Option<f64> {
    let mut search = 0;
    while let Some(offset) = line[search..].find(marker) {
        let at = search + offset;
        let bounded = line[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        if bounded {
            let mut cursor = Cursor::new(line);
            cursor.pos = at + marker.len();
            if let Some(value) = cursor.take_float() {
                return Some(value);
            }
        }
        search += offset + 1;
    }
    None
}

/// `VmRSS: <n> kB` — the `/proc` RSS figure inside `[sz]` lines.
fn vmrss_kib(line: &str) -> Option<f64> {
    let mut search = 0;
    while let Some(offset) = line[search..].find("VmRSS:") {
        let mut cursor = Cursor::new(line);
        cursor.pos = search + offset + 6;
        cursor.skip_ws();
        if let Some(value) = cursor.take_float() {
            cursor.skip_ws();
            if cursor.take("kB") {
                return Some(value);
            }
        }
        search += offset + 1;
    }
    None
}

/// `ms/chunk=4.2` / `(4.2 ms/chunk)` / `CHUNKPROF … map=15.8ms` figures.
fn ms_per_chunk_from(line: &str) -> Option<f64> {
    if let Some(value) = float_after_marker(line, "ms/chunk", "=:") {
        return Some(value);
    }
    // `(<float> ms/chunk)` — scan every '('.
    let mut search = 0;
    while let Some(offset) = line[search..].find('(') {
        let mut cursor = Cursor::new(line);
        cursor.pos = search + offset + 1;
        if let Some(value) = cursor.take_float() {
            cursor.skip_ws();
            if cursor.take("ms/chunk)") {
                return Some(value);
            }
        }
        search += offset + 1;
    }
    // `CHUNKPROF … map=<float>ms` (case-insensitive markers).
    let lower = line.to_lowercase();
    if lower.contains("chunkprof") {
        let mut search = 0;
        while let Some(offset) = lower[search..].find("map=") {
            let at = search + offset;
            let bounded = lower[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !is_word_char(c));
            let mut cursor = Cursor::new(&lower);
            cursor.pos = at + 4;
            if bounded {
                if let Some(value) = cursor.take_float() {
                    cursor.skip_ws();
                    if cursor.take("ms")
                        && cursor
                            .rest()
                            .chars()
                            .next()
                            .is_none_or(|c| !is_word_char(c))
                    {
                        return Some(value);
                    }
                }
            }
            search += offset + 1;
        }
    }
    None
}

/// `(x, z)` integer pair at the cursor, e.g. `(12, -4)`.
fn take_coord_pair(cursor: &mut Cursor) -> Option<(i64, i64)> {
    if !cursor.take("(") {
        return None;
    }
    cursor.skip_ws();
    let x = cursor.take_int()?;
    cursor.skip_ws();
    if !cursor.take(",") {
        return None;
    }
    cursor.skip_ws();
    let z = cursor.take_int()?;
    cursor.skip_ws();
    if !cursor.take(")") {
        return None;
    }
    Some((x, z))
}

/// First `(x, z)` pair after `from` — `[^()]*` keeps parens from crossing.
fn coords_after_keyword(line: &str, keyword_end: usize) -> Option<(i64, i64)> {
    let tail = &line[keyword_end..];
    let open = tail.find('(')?;
    if tail[..open].contains(')') {
        return None;
    }
    let mut cursor = Cursor::new(tail);
    cursor.pos = open;
    take_coord_pair(&mut cursor)
}

/// `(cx, cz)` from a hotspot/slow-chunk/quarantine line, else `None`.
fn hotspot_coords(line: &str) -> Option<(i64, i64)> {
    let lower = line.to_lowercase();
    // `hotspot` / `slow chunk` then the first `(x, z)` parens pair.
    for keyword in ["hotspot", "slow chunk"] {
        let mut search = 0;
        while let Some(offset) = lower[search..].find(keyword) {
            let end = search + offset + keyword.len();
            if let Some(coords) = coords_after_keyword(&lower, end) {
                return Some(coords);
            }
            search += offset + 1;
        }
    }
    // `chunk (x, z)` followed by hotspot/slow/throttle/quarantine/frozen/froze.
    let mut search = 0;
    while let Some(offset) = lower[search..].find("chunk") {
        let end = search + offset + "chunk".len();
        if let Some((x, z)) = coords_after_keyword(&lower, end) {
            if let Some(close) = lower[end..].find(')') {
                let tail = &lower[end + close + 1..];
                if ["hotspot", "slow", "throttl", "quarantin", "frozen", "froze"]
                    .iter()
                    .any(|kw| tail.contains(kw))
                {
                    return Some((x, z));
                }
            }
        }
        search += offset + 1;
    }
    // `cx=N cz=M` / `cx=N, cz=M`.
    let mut search = 0;
    while let Some(offset) = lower[search..].find("cx") {
        let at = search + offset;
        let bounded = lower[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let mut cursor = Cursor::new(&lower);
        cursor.pos = at + 2;
        if bounded && cursor.take("=") {
            cursor.skip_ws();
            if let Some(x) = cursor.take_int() {
                cursor.skip_ws();
                if cursor.take(",") || cursor.take(" ") {
                    cursor.skip_ws();
                    if cursor.take("cz") {
                        cursor.skip_ws();
                        if cursor.take("=") {
                            cursor.skip_ws();
                            if let Some(z) = cursor.take_int() {
                                return Some((x, z));
                            }
                        }
                    }
                }
            }
        }
        search += offset + 1;
    }
    // `chunk N, M` / `hotspot N, M` (bare word order).
    for keyword in ["chunk", "hotspot"] {
        let mut search = 0;
        while let Some(offset) = lower[search..].find(keyword) {
            let at = search + offset;
            let bounded = lower[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !is_word_char(c));
            let mut cursor = Cursor::new(&lower);
            cursor.pos = at + keyword.len();
            if bounded && cursor.skip_ws() {
                if let Some(x) = cursor.take_int() {
                    cursor.skip_ws();
                    if cursor.take(",") {
                        cursor.skip_ws();
                        if let Some(z) = cursor.take_int() {
                            let bounded_after = cursor
                                .rest()
                                .chars()
                                .next()
                                .is_none_or(|c| !is_word_char(c));
                            if bounded_after {
                                return Some((x, z));
                            }
                        }
                    }
                }
            }
            search += offset + 1;
        }
    }
    None
}

/// The `\bSLEEPING\b`/`server (is )?(sleeping|asleep|idle)`/`idle sleep`
/// guard predicate (case-insensitive, like the Python wrapper).
fn matches_sleep(line: &str) -> bool {
    let lower = line.to_lowercase();
    if contains_word(&lower, "sleeping") {
        return true;
    }
    // `idle\s+sleep`
    let mut search = 0;
    while let Some(offset) = lower[search..].find("idle") {
        let mut cursor = Cursor::new(&lower);
        cursor.pos = search + offset + 4;
        if cursor.skip_ws() && cursor.take("sleep") {
            return true;
        }
        search += offset + 1;
    }
    // `\bserver\s+(?:is\s+)?(sleeping|asleep|idle)\b`
    let mut search = 0;
    while let Some(offset) = lower[search..].find("server") {
        let at = search + offset;
        let bounded = lower[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let mut cursor = Cursor::new(&lower);
        cursor.pos = at + 6;
        if bounded && cursor.skip_ws() {
            if cursor.rest().starts_with("is")
                && cursor.rest()[2..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                cursor.pos += 2;
                cursor.skip_ws();
            }
            for word in ["sleeping", "asleep", "idle"] {
                if cursor.rest().starts_with(word)
                    && cursor.rest()[word.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !is_word_char(c))
                {
                    return true;
                }
            }
        }
        search += offset + 1;
    }
    false
}

/// `woke` / `wake-on-join` / `resumed from sleep` / `waking`.
fn matches_wake(line: &str) -> bool {
    let lower = line.to_lowercase();
    if contains_word(&lower, "woke") || contains_word(&lower, "waking") {
        return true;
    }
    // `\bwake[- ]on[- ]join\b`
    let mut search = 0;
    while let Some(offset) = lower[search..].find("wake") {
        let at = search + offset;
        let bounded = lower[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let mut cursor = Cursor::new(&lower);
        cursor.pos = at + 4;
        if bounded
            && (cursor.take("-") || cursor.take(" "))
            && cursor.take("on")
            && (cursor.take("-") || cursor.take(" "))
            && cursor.take("join")
            && cursor
                .rest()
                .chars()
                .next()
                .is_none_or(|c| !is_word_char(c))
        {
            return true;
        }
        search += offset + 1;
    }
    // `\bresumed\s+from\s+sleep\b`
    let mut search = 0;
    while let Some(offset) = lower[search..].find("resumed") {
        let at = search + offset;
        let bounded = lower[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let mut cursor = Cursor::new(&lower);
        cursor.pos = at + 7;
        if bounded
            && cursor.take_ws1("from")
            && cursor.take_ws1("sleep")
            && cursor
                .rest()
                .chars()
                .next()
                .is_none_or(|c| !is_word_char(c))
        {
            return true;
        }
        search += offset + 1;
    }
    false
}

fn is_fatal(line: &str) -> bool {
    line.contains("[FATAL]") || line.contains("panicked")
}

fn has_error(line: &str) -> bool {
    contains_word(line, "error")
        || contains_word(line, "failed")
        || line.contains("FAILED")
        || line.contains("refusing")
}

fn has_warn(line: &str) -> bool {
    line.contains("[security]")
        || line.contains("refused")
        || line.contains("rejected")
        || contains_word(line, "warn")
        || line.contains("skipped")
        || line.contains("missing")
}

/// `[tick]`/`[sz]`/`[chunkprof]`-family lines fold into metrics, not events.
fn looks_telemetry(line: &str) -> bool {
    let stripped = line.trim_start();
    let lower = stripped.to_lowercase();
    stripped.starts_with("[tick]")
        || stripped.starts_with("[sz]")
        || stripped.starts_with("[chunkprof]")
        || lower.contains("chunkprof")
        || lower.contains("observed_tps=")
        || lower.contains("vmrss:")
}

// ----------------------------------------------------------- mode selection

/// Which console front-end the `megu` binary should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerConsoleMode {
    /// The interactive dashboard: pinned HUD, filtered log, command line.
    Tui,
    /// Plain stdio passthrough — the fail-closed default for pipes, CI,
    /// Docker, systemd and dumb terminals.
    PlainCli,
}

/// Which subset of the scrolling log the dashboard shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFilterMode {
    /// Human events plus warnings/errors/alerts; telemetry stays folded into
    /// the metrics strip and is hidden (default).
    #[default]
    Events,
    /// Only warnings, errors, security and chunk-hotspot/quarantine alerts.
    Alerts,
    /// The raw server log, telemetry included.
    All,
}

impl LogFilterMode {
    /// `events`/`alerts`/`all`; `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "events" => Some(Self::Events),
            "alerts" => Some(Self::Alerts),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// [`Self::parse`] with the `events` default for unknown names.
    pub fn normalize(name: &str) -> Self {
        Self::parse(name).unwrap_or_default()
    }

    /// The next mode under the `[f]` key: events → alerts → all → events.
    pub fn cycle(self) -> Self {
        match self {
            Self::Events => Self::Alerts,
            Self::Alerts => Self::All,
            Self::All => Self::Events,
        }
    }

    /// The CLI/log spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Alerts => "alerts",
            Self::All => "all",
        }
    }
}

/// The console flags the `megu` binary understands before mode resolution.
#[derive(Clone, Debug, Default)]
pub struct ConsoleModeOptions {
    /// `--cli` / `--plain` / `--no-tui` / `--nogui`: never a TUI.
    pub force_cli: bool,
    /// `--tui`: explicitly request the dashboard. Capability checks still
    /// apply — a pipe or dumb terminal falls back to passthrough.
    pub force_tui: bool,
    /// `--no-banner` (or `MEGU_NO_BANNER=1` / `NO_BANNER=1`): skip the startup
    /// MEGU banner in the scrolling log.
    pub no_banner: bool,
    /// `--ascii`: pure-ASCII glyphs and no color.
    pub ascii_only: bool,
    /// `--log-filter events|alerts|all` (default `events`).
    pub log_filter: LogFilterMode,
}

impl ConsoleModeOptions {
    /// Fold command-line tokens into console options.
    ///
    /// Recognized flags are consumed; every other token is returned untouched
    /// so the server's own argument parser still sees it. A `--log-filter`
    /// flag with a missing or invalid value is handed back verbatim so the
    /// outer parser can diagnose it instead of silently swallowing it.
    pub fn from_args(args: &[String]) -> (Self, Vec<String>) {
        let mut options = ConsoleModeOptions::default();
        let mut remaining = Vec::with_capacity(args.len());
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            match arg.as_str() {
                "--" => {
                    remaining.extend(args[index + 1..].iter().cloned());
                    break;
                }
                "--cli" | "--plain" | "--no-tui" | "--nogui" => options.force_cli = true,
                "--tui" => options.force_tui = true,
                "--no-banner" => options.no_banner = true,
                "--ascii" => options.ascii_only = true,
                "--log-filter" => {
                    match args.get(index + 1).and_then(|v| LogFilterMode::parse(v)) {
                        Some(mode) => {
                            options.log_filter = mode;
                            index += 1;
                        }
                        None => remaining.push(arg.clone()),
                    }
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--log-filter=") {
                        match LogFilterMode::parse(value) {
                            Some(mode) => options.log_filter = mode,
                            None => remaining.push(arg.clone()),
                        }
                    } else {
                        remaining.push(arg.clone());
                    }
                }
            }
            index += 1;
        }
        (options, remaining)
    }
}

/// Resolve which console front-end to run.
///
/// Fail-closed order: an explicit `--cli`/`--plain`/`--no-tui`/`--nogui`
/// always wins, then non-TTY stdio, `NO_COLOR`, and unset/`dumb`/`unknown`
/// `TERM` each force [`ServerConsoleMode::PlainCli`]. `--tui` requests the
/// dashboard but never overrides those safety checks — only a genuinely
/// interactive terminal gets [`ServerConsoleMode::Tui`].
pub fn resolve_console_mode(
    opts: &ConsoleModeOptions,
    stdin_tty: bool,
    stdout_tty: bool,
    term: Option<&str>,
    no_color: bool,
) -> ServerConsoleMode {
    if opts.force_cli || !tui_capable(stdin_tty, stdout_tty, term, no_color) {
        return ServerConsoleMode::PlainCli;
    }
    // --tui is an explicit request; capability already decided the outcome.
    let _ = opts.force_tui;
    ServerConsoleMode::Tui
}

/// Whether the interactive dashboard may run at all on this terminal.
fn tui_capable(stdin_tty: bool, stdout_tty: bool, term: Option<&str>, no_color: bool) -> bool {
    if !(stdin_tty && stdout_tty) {
        return false;
    }
    if no_color {
        return false;
    }
    if matches!(term, None | Some("") | Some("dumb") | Some("unknown")) {
        return false;
    }
    // The interactive loop needs POSIX terminal control; on Windows the
    // console stays on the plain passthrough, matching the Python wrapper.
    if cfg!(windows) {
        return false;
    }
    true
}

/// Whether ANSI color may appear in frame output.
pub fn ansi_color_ok(term: Option<&str>, no_color: bool) -> bool {
    !no_color && !matches!(term, None | Some("") | Some("dumb") | Some("unknown"))
}

/// Resolve the mode against this process's real stdio and environment —
/// `NO_COLOR` counts by presence (even empty), like the wrapper.
pub fn resolve_console_mode_env(opts: &ConsoleModeOptions) -> ServerConsoleMode {
    use std::io::IsTerminal;
    resolve_console_mode(
        opts,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

// ------------------------------------------------------------- log decoding

/// What a fed log line decoded as — the classifier's match kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLineKind {
    /// `megu vX listening addr:port` — the server is up.
    Ready,
    /// `[tick] observed_tps=…` telemetry.
    Tick,
    /// `[sz] t=… VmRSS:…` telemetry.
    Size,
    /// The server announced it is sleeping until a player joins.
    Sleep,
    /// The server woke from sleep.
    Wake,
    /// A chunk hotspot / throttling diagnostic.
    Hotspot,
    /// A chunk quarantine diagnostic.
    Quarantine,
    /// `[spine] <name> joined from <addr> (uuid …)`.
    Join,
    /// `[spine] <name> left` / `disconnected:`.
    Leave,
    /// A `[live-mods]` summary/scan/dual-loader line or `Mods:` status.
    Mods,
    /// A per-jar `[live-mods] file (id) tier…` detail line.
    Mod,
    /// `Status: <Word> (… TPS, … ms/tick)`.
    Status,
    /// `Players: n/max (names)`.
    Players,
    /// `Loaded chunks: n` / `WorldSummary(chunks=n`.
    Chunks,
    /// `Memory: n MB`.
    Memory,
    /// `[lan] advertising "motd" on port n`.
    Lan,
    /// A startup banner row.
    Banner,
    /// A line carrying an `ms/chunk` figure but no other shape.
    MsPerChunk,
    /// Anything else.
    Log,
}

/// The noise-filter bucket a line lands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogCategory {
    /// `[tick]`/`[sz]`/`[chunkprof]` spam — hidden under `events`.
    Telemetry,
    /// A startup banner row — decorative, never counted as warn/err.
    Banner,
    /// A human-relevant line (joins, leaves, chat, status, lifecycle).
    Event,
    /// A warning-level alert (hotspot, quarantine, warn/refused/skipped…).
    Warn,
    /// A `[FATAL]`/panic/error/failed line.
    Error,
    /// A `[security]` line without error keywords.
    Security,
}

impl LogCategory {
    /// Alerts-filter membership: warnings, errors and security lines.
    fn is_alert(self) -> bool {
        matches!(self, Self::Warn | Self::Error | Self::Security)
    }
}

/// A captured log line with its filter bucket.
#[derive(Clone, Debug)]
pub struct LogEntry {
    /// The noise-filter bucket.
    pub category: LogCategory,
    /// The line text, ANSI-stripped and right-trimmed.
    pub line: String,
}

/// Per-jar adapted-mod detail from `[live-mods] file (id) tier: note`.
#[derive(Clone, Debug)]
pub struct ModDetail {
    /// Jar file name.
    pub file: String,
    /// Declared mod id.
    pub mod_id: String,
    /// Raw tier label (`"tier2 shim"`, `"tier3"`, …).
    pub tier: String,
    /// Free-form adapter note.
    pub detail: String,
}

/// Counts from the `[live-mods] N jar(s): …` summary line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModCounts {
    /// Jars skipped outright (client-only, wrong target, …).
    pub auto_skipped: u64,
    /// Jars with a native bundle.
    pub native: u64,
    /// Jars running under the tier-2 compatibility shim.
    pub shim: u64,
    /// Jars running under the tier-3 runtime fallback.
    pub fallback: u64,
}

impl ModCounts {
    /// Native + shim: the count the cards describe as `active`.
    fn active(self) -> u64 {
        self.native + self.shim
    }
}

/// The `[live-mods] dual-loader:` reconciliation line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DualLoader {
    /// Jars handled by both loaders consistently.
    pub unified: u64,
    /// Jars that only make sense client-side.
    pub client_only: u64,
    /// Jars superseded by a newer duplicate.
    pub superseded: u64,
    /// Jars rejected outright.
    pub rejected: u64,
    /// Duplicate mod ids seen.
    pub duplicate_ids: u64,
    /// Jars missing declared dependencies.
    pub missing_deps: u64,
    /// Damage hooks verified under the shim.
    pub damage_hooks: u64,
}

/// A chunk hotspot / quarantine diagnostic for the guard card.
#[derive(Clone, Debug)]
pub struct Hotspot {
    /// Chunk X coordinate, when the line carried it.
    pub cx: Option<i64>,
    /// Chunk Z coordinate, when the line carried it.
    pub cz: Option<i64>,
    /// `true` for quarantine, `false` for throttle/slow-chunk alerts.
    pub quarantined: bool,
    /// The raw line that raised the alert.
    pub line: String,
}

/// Structured live snapshot pushed directly by the embedded server loop so
/// metrics stay fresh every second without polluting the scrolling event log.
#[derive(Clone, Debug, Default)]
pub struct LiveTuiMetrics {
    /// Current observed TPS.
    pub tps: Option<f64>,
    /// Mean or p50 tick work in milliseconds.
    pub mspt_ms: Option<f64>,
    /// Resident memory in MiB.
    pub mem_mib: Option<f64>,
    /// Process CPU usage in percent.
    pub cpu_pct: Option<f64>,
    /// Active thread count.
    pub threads: Option<u64>,
    /// Online player count.
    pub players_online: u64,
    /// Configured maximum players.
    pub players_max: u64,
    /// Sorted or unsorted online player names.
    pub player_names: Vec<String>,
    /// Currently loaded chunk count.
    pub loaded_chunks: u64,
    /// Native + compatibility-shim mod count.
    pub active_mods: u64,
    /// Whether the server is in idle sleep.
    pub sleeping: bool,
    /// Optional latest chunk hotspot `(cx, cz, ms_per_tick)`.
    pub top_hotspot: Option<(i64, i64, f64)>,
}

/// Which body the dashboard shows under the top status bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TuiView {
    /// The filtered scrolling log (default).
    #[default]
    Log,
    /// The `F3` / `Ctrl+P` players view.
    Players,
    /// The `F4` / `Ctrl+G` adapted-mods view.
    Mods,
}

/// One decoded key for the interactive state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TuiKey {
    /// A printable character appended at the prompt cursor.
    Char(char),
    /// Enter — submits the command buffer.
    Enter,
    /// Backspace / Ctrl+H — delete char before cursor.
    Backspace,
    /// Delete — delete char at cursor.
    Delete,
    /// Tab — complete common server command prefixes.
    Tab,
    /// Escape — clear command input, or return from Players/Mods to Log.
    Escape,
    /// Up arrow — recall older command history.
    ArrowUp,
    /// Down arrow — recall newer command history.
    ArrowDown,
    /// Left arrow — move prompt cursor left.
    ArrowLeft,
    /// Right arrow — move prompt cursor right.
    ArrowRight,
    /// Home — move prompt cursor to start of line.
    Home,
    /// End — move prompt cursor to end of line.
    End,
    /// PageUp — scroll log up (older lines).
    PageUp,
    /// PageDown — scroll log down (newer lines).
    PageDown,
    /// F2 / Ctrl+F — cycle log filter (events → alerts → all).
    F2,
    /// F3 / Ctrl+P — toggle Players view.
    F3,
    /// F4 / Ctrl+G — toggle Mods view.
    F4,
    /// Ctrl+U — clear the current command input line.
    CtrlU,
    /// Ctrl+C — request a graceful stop.
    CtrlC,
}

/// What a key press asks the hosting loop to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiAction {
    /// Nothing — state updated, redraw if changed.
    None,
    /// Forward this line to the server's stdin console.
    SubmitCommand(String),
    /// Send the graceful `stop` line to the server's console.
    RequestStop,
}

/// Push onto a bounded ring, evicting the oldest entry at capacity.
fn push_ring<T>(ring: &mut VecDeque<T>, capacity: usize, value: T) {
    if ring.len() == capacity {
        ring.pop_front();
    }
    ring.push_back(value);
}

/// The smart console: parser state, metrics history and interactive state.
pub struct ServerTuiState {
    tps: VecDeque<f64>,
    mspt: VecDeque<f64>,
    mem_mib: VecDeque<f64>,
    cpu_pct: VecDeque<f64>,
    ms_per_chunk: VecDeque<f64>,
    threads: Option<u64>,
    players: BTreeMap<String, Option<String>>,
    players_online: Option<u64>,
    players_max: Option<u64>,
    loaded_chunks: Option<u64>,
    mods: ModCounts,
    jars_seen: u64,
    mod_details: Vec<ModDetail>,
    dual_loader: Option<DualLoader>,
    errors: u64,
    warnings: u64,
    panics: u64,
    over_budget: u64,
    ticks_sampled: u64,
    uptime_ticks: Option<u64>,
    clients: Option<u64>,
    ready: bool,
    version: Option<String>,
    bind: Option<String>,
    port: Option<u64>,
    motd: Option<String>,
    status_label: String,
    sleeping: bool,
    hotspots: VecDeque<Hotspot>,
    quarantine_alerts: u64,
    log: VecDeque<LogEntry>,
    lines_seen: u64,
    banner_added: bool,
    revision: u64,
    // Interactive state (always-ready console prompt).
    view: TuiView,
    log_filter: LogFilterMode,
    cmd_mode: bool,
    cmd_buf: String,
    cmd_cursor: usize,
    log_scroll: usize,
    history: VecDeque<String>,
    history_index: usize,
    quit_requested: bool,
    stopping: bool,
    exited: Option<i32>,
}

impl Default for ServerTuiState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerTuiState {
    /// A fresh console: `Booting`, log view, events filter, always-ready prompt.
    pub fn new() -> Self {
        ServerTuiState {
            tps: VecDeque::new(),
            mspt: VecDeque::new(),
            mem_mib: VecDeque::new(),
            cpu_pct: VecDeque::new(),
            ms_per_chunk: VecDeque::new(),
            threads: None,
            players: BTreeMap::new(),
            players_online: None,
            players_max: None,
            loaded_chunks: None,
            mods: ModCounts::default(),
            jars_seen: 0,
            mod_details: Vec::new(),
            dual_loader: None,
            errors: 0,
            warnings: 0,
            panics: 0,
            over_budget: 0,
            ticks_sampled: 0,
            uptime_ticks: None,
            clients: None,
            ready: false,
            version: None,
            bind: None,
            port: None,
            motd: None,
            status_label: "Booting".to_string(),
            sleeping: false,
            hotspots: VecDeque::new(),
            quarantine_alerts: 0,
            log: VecDeque::new(),
            lines_seen: 0,
            banner_added: false,
            revision: 0,
            view: TuiView::Log,
            log_filter: LogFilterMode::Events,
            cmd_mode: true,
            cmd_buf: String::new(),
            cmd_cursor: 0,
            log_scroll: 0,
            history: VecDeque::new(),
            history_index: 0,
            quit_requested: false,
            stopping: false,
            exited: None,
        }
    }

    /// A console pre-configured from resolved flags: log filter applied and
    /// the startup banner injected unless `opts.no_banner`.
    pub fn with_options(opts: &ConsoleModeOptions) -> Self {
        let mut state = Self::new();
        state.log_filter = opts.log_filter;
        if !opts.no_banner {
            state.inject_banner(opts.ascii_only);
        }
        state
    }

    /// Update live server telemetry silently without appending lines to the
    /// scrolling event log.
    pub fn update_live_metrics(&mut self, metrics: &LiveTuiMetrics) {
        if let Some(tps) = metrics.tps {
            if tps.is_finite() && tps >= 0.0 {
                push_ring(&mut self.tps, SAMPLE_HISTORY, tps);
            }
        }
        if let Some(mspt) = metrics.mspt_ms {
            if mspt.is_finite() && mspt >= 0.0 {
                push_ring(&mut self.mspt, SAMPLE_HISTORY, mspt);
            }
        }
        if let Some(mib) = metrics.mem_mib {
            if mib.is_finite() && mib >= 0.0 {
                push_ring(&mut self.mem_mib, SAMPLE_HISTORY, mib);
            }
        }
        if let Some(cpu) = metrics.cpu_pct {
            if cpu.is_finite() && cpu >= 0.0 {
                push_ring(&mut self.cpu_pct, SAMPLE_HISTORY, cpu);
            }
        }
        if let Some(threads) = metrics.threads {
            self.threads = Some(threads);
        }
        self.players_online = Some(metrics.players_online);
        self.players_max = Some(metrics.players_max);
        let mut next_players = BTreeMap::new();
        for name in &metrics.player_names {
            let addr = self.players.get(name).cloned().flatten();
            next_players.insert(name.clone(), addr);
        }
        self.players = next_players;
        self.loaded_chunks = Some(metrics.loaded_chunks);
        if metrics.active_mods > 0 && self.mods.active() == 0 {
            self.mods.native = metrics.active_mods;
        }
        self.sleeping = metrics.sleeping;
        if self.sleeping {
            self.status_label = "Sleeping".to_string();
        } else if self.ready {
            self.status_label = "Running".to_string();
        }
        if let Some((cx, cz, ms)) = metrics.top_hotspot {
            let same_last = self
                .hotspots
                .back()
                .is_some_and(|h| h.cx == Some(cx) && h.cz == Some(cz));
            if !same_last && ms >= 20.0 {
                push_ring(
                    &mut self.hotspots,
                    HOTSPOT_CAPACITY,
                    Hotspot {
                        cx: Some(cx),
                        cz: Some(cz),
                        quarantined: false,
                        line: format!("[guard] chunk hotspot ({cx},{cz}) {ms:.1} ms/tick"),
                    },
                );
            }
        }
        self.revision += 1;
    }

    /// Echo an operator-entered command into the scrolling log.
    pub fn push_command_echo(&mut self, command: &str) {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return;
        }
        push_ring(
            &mut self.log,
            LOG_CAPACITY,
            LogEntry {
                category: LogCategory::Event,
                line: format!("[console] > {trimmed}"),
            },
        );
        self.log_scroll = 0;
        self.revision += 1;
    }

    /// Write the startup banner once at the top of the scrolling log.
    ///
    /// Banner lines are not fed through the decoder: they are decorative,
    /// never counted as warnings/errors, and carry the dedicated
    /// [`LogCategory::Banner`] bucket so the alerts filter stays alerts-only.
    /// Returns `false` when the banner was already injected.
    pub fn inject_banner(&mut self, ascii: bool) -> bool {
        if self.banner_added {
            return false;
        }
        for line in startup_banner(ascii) {
            push_ring(
                &mut self.log,
                LOG_CAPACITY,
                LogEntry {
                    category: LogCategory::Banner,
                    line: (*line).to_string(),
                },
            );
        }
        self.banner_added = true;
        self.revision += 1;
        true
    }

    /// Fold one raw server log line into state; returns its decoded kind.
    ///
    /// ANSI is stripped and the line is right-trimmed; every line lands in the
    /// categorized scrolling log while known shapes additionally decode into
    /// structured metrics, the player list and guard state.
    pub fn ingest_log_line(&mut self, line: &str) -> LogLineKind {
        let line = strip_ansi(line).trim_end().to_string();
        self.lines_seen += 1;
        self.revision += 1;
        let kind = self.decode(&line);
        let category = classify_kind(&line, kind);
        push_ring(
            &mut self.log,
            LOG_CAPACITY,
            LogEntry { category, line },
        );
        kind
    }

    /// Record one external `/proc` sample: RSS MiB, threads, delta CPU %.
    /// Non-finite values are dropped; any accepted metric bumps the revision.
    pub fn note_proc_metrics(
        &mut self,
        rss_mib: Option<f64>,
        threads: Option<u64>,
        cpu_pct: Option<f64>,
    ) {
        let mut changed = false;
        if let Some(mib) = rss_mib {
            if mib.is_finite() && mib >= 0.0 {
                push_ring(&mut self.mem_mib, SAMPLE_HISTORY, mib);
                changed = true;
            }
        }
        if let Some(count) = threads {
            self.threads = Some(count);
            changed = true;
        }
        if let Some(pct) = cpu_pct {
            if pct.is_finite() && pct >= 0.0 {
                push_ring(&mut self.cpu_pct, SAMPLE_HISTORY, pct);
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
        }
    }

    /// Record an external RSS sample (e.g. from `/proc/<pid>/status`).
    pub fn note_memory(&mut self, mib: f64) {
        self.note_proc_metrics(Some(mib), None, None);
    }

    // -- decoding --------------------------------------------------------

    fn decode(&mut self, line: &str) -> LogLineKind {
        if is_banner_line(line) {
            return LogLineKind::Banner;
        }

        let mspc = ms_per_chunk_from(line);
        if let Some(value) = mspc {
            push_ring(&mut self.ms_per_chunk, SAMPLE_HISTORY, value);
        }

        // `megu vX.Y listening addr:port`
        if let Some((version, bind, port)) = parse_ready(line) {
            self.ready = true;
            self.status_label = "Running".to_string();
            self.version = Some(version);
            self.bind = Some(bind);
            self.port = Some(port);
            return LogLineKind::Ready;
        }

        // `[tick] observed_tps=F avg_work_ms=F max_work_ms=F over_budget=a/b`
        if let Some((tps, work, _max_work, over, sampled)) = parse_tick(line) {
            push_ring(&mut self.tps, SAMPLE_HISTORY, tps);
            push_ring(&mut self.mspt, SAMPLE_HISTORY, work);
            self.over_budget += over;
            self.ticks_sampled += sampled;
            return LogLineKind::Tick;
        }

        if line.starts_with("[sz]") {
            let mut cursor = Cursor::new(line);
            if cursor.take("[sz]") && cursor.take_ws1("t=") {
                if let Some(t) = cursor.take_int() {
                    self.uptime_ticks = Some(t.max(0) as u64);
                }
            }
            if let Some(kib) = vmrss_kib(line) {
                push_ring(&mut self.mem_mib, SAMPLE_HISTORY, kib / 1024.0);
            }
            if let Some(clients) = bounded_float_after_marker(line, "clients=") {
                self.clients = Some(clients as u64);
            }
            return LogLineKind::Size;
        }

        if matches_sleep(line) && !line.trim_start().starts_with("[live-mods]") {
            self.sleeping = true;
            self.status_label = "Sleeping".to_string();
            return LogLineKind::Sleep;
        }
        if matches_wake(line) {
            self.sleeping = false;
            if self.status_label == "Sleeping" {
                self.status_label = "Running".to_string();
            }
            return LogLineKind::Wake;
        }

        let lower = line.to_lowercase();
        if lower.contains("hotspot")
            || lower.contains("slow chunk")
            || (lower.contains("throttl") && lower.contains("chunk"))
        {
            let (cx, cz) = match hotspot_coords(line) {
                Some((x, z)) => (Some(x), Some(z)),
                None => (None, None),
            };
            push_ring(
                &mut self.hotspots,
                HOTSPOT_CAPACITY,
                Hotspot {
                    cx,
                    cz,
                    quarantined: false,
                    line: line.to_string(),
                },
            );
            self.warnings += 1;
            return LogLineKind::Hotspot;
        }
        if lower.contains("quarantin")
            && (lower.contains("chunk") || lower.trim_start().starts_with("[quarantine"))
        {
            let (cx, cz) = match hotspot_coords(line) {
                Some((x, z)) => (Some(x), Some(z)),
                None => (None, None),
            };
            push_ring(
                &mut self.hotspots,
                HOTSPOT_CAPACITY,
                Hotspot {
                    cx,
                    cz,
                    quarantined: true,
                    line: line.to_string(),
                },
            );
            self.quarantine_alerts += 1;
            self.warnings += 1;
            return LogLineKind::Quarantine;
        }

        // `[spine] <name> joined from <addr> (uuid <hex>)`
        if let Some((name, addr)) = parse_join(line) {
            self.players.insert(name, Some(addr));
            self.players_online = Some(self.players.len() as u64);
            self.sleeping = false;
            if self.status_label == "Sleeping" {
                self.status_label = "Running".to_string();
            }
            return LogLineKind::Join;
        }

        // `[spine] <name> left (from <addr>)` / `disconnected:`
        if let Some(name) = parse_leave(line) {
            self.players.remove(&name);
            self.players_online = Some(self.players.len() as u64);
            return LogLineKind::Leave;
        }

        if let Some((jars, counts)) = parse_mods_summary(line) {
            self.jars_seen = jars;
            self.mods = counts;
            return LogLineKind::Mods;
        }

        if let Some(dual) = parse_dual_loader(line) {
            if self.jars_seen == 0 {
                self.jars_seen = dual.unified;
            }
            self.dual_loader = Some(dual);
            return LogLineKind::Mods;
        }

        if let Some(detail) = parse_mod_detail(line) {
            self.mod_details.push(detail);
            return LogLineKind::Mod;
        }

        if let Some(jars) = parse_mods_scanned(line) {
            self.jars_seen = jars;
            return LogLineKind::Mods;
        }

        // `Status: <Word> (F TPS, F ms/tick)`
        if let Some((word, tps, work)) = parse_status(line) {
            self.status_label = word;
            push_ring(&mut self.tps, SAMPLE_HISTORY, tps);
            push_ring(&mut self.mspt, SAMPLE_HISTORY, work);
            return LogLineKind::Status;
        }

        // `Players: n/max (names…)`
        if let Some((online, max, names)) = parse_players(line) {
            self.players_online = Some(online);
            self.players_max = Some(max);
            if let Some(names) = names {
                // Keep addresses learned from join lines; the status
                // snapshot only reports names.
                let mut listed = BTreeMap::new();
                for name in names {
                    let addr = self.players.get(&name).cloned().flatten();
                    listed.insert(name, addr);
                }
                self.players = listed;
            }
            return LogLineKind::Players;
        }

        if let Some(chunks) = parse_chunks(line) {
            self.loaded_chunks = Some(chunks);
            return LogLineKind::Chunks;
        }

        // `Memory: n MB`
        if let Some(rest) = line.strip_prefix("Memory:") {
            let mut cursor = Cursor::new(rest);
            if cursor.skip_ws() {
                if let Some(mb) = cursor.take_int() {
                    if cursor.take_ws1("MB") {
                        push_ring(&mut self.mem_mib, SAMPLE_HISTORY, mb as f64);
                        return LogLineKind::Memory;
                    }
                }
            }
        }

        // `Mods: n native[, m compatibility mode]`
        if let Some(rest) = line.strip_prefix("Mods:") {
            let mut cursor = Cursor::new(rest);
            if cursor.skip_ws() {
                if let Some(native) = cursor.take_int() {
                    if cursor.take_ws1("native") {
                        self.mods.native = native.max(0) as u64;
                        if cursor.skip_ws() && cursor.take(",") {
                            cursor.skip_ws();
                            if let Some(shim) = cursor.take_int() {
                                if cursor.take_ws1("compatibility") && cursor.take_ws1("mode") {
                                    self.mods.shim = shim.max(0) as u64;
                                }
                            }
                        }
                        return LogLineKind::Mods;
                    }
                }
            }
        }

        // `[lan] advertising "motd" on port n`
        if let Some((motd, port)) = parse_lan(line) {
            self.motd = Some(motd);
            if self.port.is_none() {
                self.port = Some(port);
            }
            return LogLineKind::Lan;
        }

        if is_fatal(line) {
            self.panics += 1;
            self.errors += 1;
            self.status_label = "Crash guard".to_string();
        } else if has_error(line) {
            self.errors += 1;
        } else if has_warn(line) {
            self.warnings += 1;
        }
        if mspc.is_some() {
            LogLineKind::MsPerChunk
        } else {
            LogLineKind::Log
        }
    }

    // -- snapshot accessors ----------------------------------------------

    /// Monotonic change counter — redraw when it (or UI state) changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Total lines fed so far, before ring eviction.
    pub fn lines_seen(&self) -> u64 {
        self.lines_seen
    }

    /// Whether `megu vX listening …` has been seen.
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// Latest observed TPS.
    pub fn tps_current(&self) -> Option<f64> {
        self.tps.back().copied()
    }

    /// Latest average tick work in ms.
    pub fn mspt_current(&self) -> Option<f64> {
        self.mspt.back().copied()
    }

    /// Latest RSS in MiB.
    pub fn mem_current(&self) -> Option<f64> {
        self.mem_mib.back().copied()
    }

    /// Latest delta CPU percent.
    pub fn cpu_current(&self) -> Option<f64> {
        self.cpu_pct.back().copied()
    }

    /// Online player count (status line wins over join/leave tracking).
    pub fn players_online(&self) -> u64 {
        self.players_online.unwrap_or(self.players.len() as u64)
    }

    /// Configured player cap, when a `Players:` line reported it.
    pub fn players_max(&self) -> Option<u64> {
        self.players_max
    }

    /// Sorted online player names.
    pub fn player_names(&self) -> Vec<&str> {
        self.players.keys().map(String::as_str).collect()
    }

    /// Loaded chunk count.
    pub fn loaded_chunks(&self) -> Option<u64> {
        self.loaded_chunks
    }

    /// Warning-level lines seen.
    pub fn warnings(&self) -> u64 {
        self.warnings
    }

    /// Error/fatal lines seen.
    pub fn errors(&self) -> u64 {
        self.errors
    }

    /// Panic/guard-trip lines seen.
    pub fn panics(&self) -> u64 {
        self.panics
    }

    /// Whether a chunk hotspot or quarantine alert is on record.
    pub fn hotspot_alert(&self) -> bool {
        !self.hotspots.is_empty()
    }

    /// Whether the server is in a wake-on-join sleep.
    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    /// The active body view.
    pub fn view(&self) -> TuiView {
        self.view
    }

    /// The active log filter.
    pub fn log_filter(&self) -> LogFilterMode {
        self.log_filter
    }

    /// Whether the `/` command prompt is open.
    pub fn cmd_mode(&self) -> bool {
        self.cmd_mode
    }

    /// The in-progress command buffer.
    pub fn cmd_buffer(&self) -> &str {
        &self.cmd_buf
    }

    /// The bounded command history, oldest first.
    pub fn command_history(&self) -> Vec<&str> {
        self.history.iter().map(String::as_str).collect()
    }

    /// Whether `q`/Ctrl+C requested a graceful stop.
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Mark that a graceful `stop` has been dispatched.
    pub fn mark_stopping(&mut self) {
        self.stopping = true;
        self.revision += 1;
    }

    /// Mark the server process as exited; the footer shows its code.
    pub fn mark_exited(&mut self, code: i32) {
        self.exited = Some(code);
        self.revision += 1;
    }

    /// Adapted-mod counts from the last `[live-mods]` summary.
    pub fn mods(&self) -> ModCounts {
        self.mods
    }

    /// Per-jar adapter details in log order.
    pub fn mod_details(&self) -> &[ModDetail] {
        &self.mod_details
    }

    /// Warning-level mod problems: rejected + missing-dep jars plus detail
    /// lines mentioning `missing`/`damaged`, in plain language.
    fn mod_warnings(&self) -> u64 {
        let mut count = self.dual_loader.map_or(0, |d| d.rejected + d.missing_deps);
        for detail in &self.mod_details {
            let low = detail.detail.to_lowercase();
            if low.contains("missing") || low.contains("damaged") {
                count += 1;
            }
        }
        count
    }

    /// `ALERT`/`SLEEPING`/`BUSY`/`ONLINE`/`BOOTING`, most urgent first.
    pub fn status_word(&self) -> &'static str {
        let tps = self.tps_current();
        let mspt = self.mspt_current();
        if self.panics > 0 || self.hotspot_alert() || self.errors > 0 {
            return "ALERT";
        }
        if self.sleeping && self.players_online() == 0 {
            return "SLEEPING";
        }
        if tps.is_some_and(|v| v < 18.0) || mspt.is_some_and(|v| v >= 45.0) {
            return "ALERT";
        }
        let label = self.status_label.to_lowercase();
        if label.contains("busy")
            || tps.is_some_and(|v| v < 19.5)
            || mspt.is_some_and(|v| v >= 30.0)
        {
            return "BUSY";
        }
        if self.ready {
            return "ONLINE";
        }
        "BOOTING"
    }

    /// Log entries selected by a filter, oldest first.
    fn selected_entries(&self, mode: LogFilterMode) -> Vec<&LogEntry> {
        self.log
            .iter()
            .filter(|entry| match mode {
                LogFilterMode::All => true,
                LogFilterMode::Alerts => entry.category.is_alert(),
                LogFilterMode::Events => entry.category != LogCategory::Telemetry,
            })
            .collect()
    }

    /// The line texts a filter would show — the Python `filtered_log_lines`.
    pub fn filtered_log_lines(&self, mode: LogFilterMode) -> Vec<&str> {
        self.selected_entries(mode)
            .iter()
            .map(|entry| entry.line.as_str())
            .collect()
    }

    // -- interactive state ------------------------------------------------

    /// Prompt cursor column (character index in `cmd_buf`).
    pub fn cmd_cursor(&self) -> usize {
        self.cmd_cursor
    }

    /// Number of log lines scrolled back from the live bottom (`0` = live tail).
    pub fn log_scroll(&self) -> usize {
        self.log_scroll
    }

    /// Apply one decoded key; returns what the hosting loop should do.
    ///
    /// The command prompt is always focused so operators can type `list`,
    /// `say hello`, `op Steve`, or `stop` at any time without a mode toggle.
    pub fn handle_key(&mut self, key: TuiKey) -> TuiAction {
        match key {
            TuiKey::CtrlC => {
                self.quit_requested = true;
                self.revision += 1;
                TuiAction::RequestStop
            }
            TuiKey::F2 => {
                self.log_filter = self.log_filter.cycle();
                self.view = TuiView::Log;
                self.log_scroll = 0;
                self.revision += 1;
                TuiAction::None
            }
            TuiKey::F3 => {
                self.view = if self.view == TuiView::Players {
                    TuiView::Log
                } else {
                    TuiView::Players
                };
                self.revision += 1;
                TuiAction::None
            }
            TuiKey::F4 => {
                self.view = if self.view == TuiView::Mods {
                    TuiView::Log
                } else {
                    TuiView::Mods
                };
                self.revision += 1;
                TuiAction::None
            }
            TuiKey::PageUp => {
                if self.view == TuiView::Log {
                    let max_scroll = self.selected_entries(self.log_filter).len();
                    self.log_scroll = (self.log_scroll + 5).min(max_scroll);
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::PageDown => {
                if self.view == TuiView::Log && self.log_scroll > 0 {
                    self.log_scroll = self.log_scroll.saturating_sub(5);
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::Enter => {
                let raw = self.cmd_buf.trim();
                let normalized = raw.strip_prefix('/').unwrap_or(raw).trim();
                let action = if normalized.is_empty() {
                    TuiAction::None
                } else {
                    let command = normalized.to_string();
                    if self.history.back() != Some(&command) {
                        push_ring(&mut self.history, HISTORY_CAPACITY, command.clone());
                    }
                    self.log_scroll = 0;
                    TuiAction::SubmitCommand(command)
                };
                self.cmd_buf.clear();
                self.cmd_cursor = 0;
                self.history_index = self.history.len();
                self.revision += 1;
                action
            }
            TuiKey::Backspace => {
                if self.cmd_cursor > 0 {
                    if let Some((byte_idx, _)) = self.cmd_buf.char_indices().nth(self.cmd_cursor - 1)
                    {
                        self.cmd_buf.remove(byte_idx);
                        self.cmd_cursor -= 1;
                        self.history_index = self.history.len();
                        self.revision += 1;
                    }
                }
                TuiAction::None
            }
            TuiKey::Delete => {
                if let Some((byte_idx, _)) = self.cmd_buf.char_indices().nth(self.cmd_cursor) {
                    self.cmd_buf.remove(byte_idx);
                    self.history_index = self.history.len();
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::CtrlU => {
                if !self.cmd_buf.is_empty() {
                    self.cmd_buf.clear();
                    self.cmd_cursor = 0;
                    self.history_index = self.history.len();
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::Escape => {
                if !self.cmd_buf.is_empty() {
                    self.cmd_buf.clear();
                    self.cmd_cursor = 0;
                    self.history_index = self.history.len();
                    self.revision += 1;
                } else if self.view != TuiView::Log {
                    self.view = TuiView::Log;
                    self.revision += 1;
                } else if self.log_scroll > 0 {
                    self.log_scroll = 0;
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::Tab => {
                self.complete_command();
                TuiAction::None
            }
            TuiKey::ArrowUp => {
                if !self.history.is_empty() {
                    self.history_index = self.history_index.saturating_sub(1);
                    if let Some(entry) = self.history.get(self.history_index) {
                        self.cmd_buf = entry.clone();
                        self.cmd_cursor = self.cmd_buf.chars().count();
                    }
                }
                self.revision += 1;
                TuiAction::None
            }
            TuiKey::ArrowDown => {
                self.history_index = (self.history_index + 1).min(self.history.len());
                self.cmd_buf = self
                    .history
                    .get(self.history_index)
                    .cloned()
                    .unwrap_or_default();
                self.cmd_cursor = self.cmd_buf.chars().count();
                self.revision += 1;
                TuiAction::None
            }
            TuiKey::ArrowLeft => {
                if self.cmd_cursor > 0 {
                    self.cmd_cursor -= 1;
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::ArrowRight => {
                let len = self.cmd_buf.chars().count();
                if self.cmd_cursor < len {
                    self.cmd_cursor += 1;
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::Home => {
                if self.cmd_cursor != 0 {
                    self.cmd_cursor = 0;
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::End => {
                let len = self.cmd_buf.chars().count();
                if self.cmd_cursor != len {
                    self.cmd_cursor = len;
                    self.revision += 1;
                }
                TuiAction::None
            }
            TuiKey::Char(c) if !c.is_control() => {
                let byte_idx = self
                    .cmd_buf
                    .char_indices()
                    .nth(self.cmd_cursor)
                    .map_or(self.cmd_buf.len(), |(idx, _)| idx);
                self.cmd_buf.insert(byte_idx, c);
                self.cmd_cursor += 1;
                self.history_index = self.history.len();
                self.revision += 1;
                TuiAction::None
            }
            TuiKey::Char(_) => TuiAction::None,
        }
    }

    /// Prefix-complete common server commands on `Tab`.
    fn complete_command(&mut self) {
        let trimmed = self.cmd_buf.trim_start();
        let has_slash = trimmed.starts_with('/');
        let needle = trimmed.strip_prefix('/').unwrap_or(trimmed);
        if needle.is_empty() {
            return;
        }
        let lower = needle.to_lowercase();
        let matches: Vec<&str> = COMMAND_COMPLETIONS
            .iter()
            .copied()
            .filter(|cmd| cmd.starts_with(&lower))
            .collect();
        if matches.is_empty() {
            return;
        }
        let replacement = if matches.len() == 1 {
            matches[0].to_string()
        } else {
            let mut prefix = matches[0].to_string();
            for other in &matches[1..] {
                let shared = prefix
                    .chars()
                    .zip(other.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect::<String>();
                prefix = shared;
            }
            prefix
        };
        if replacement.len() > needle.len() {
            self.cmd_buf = if has_slash {
                format!("/{replacement}")
            } else {
                replacement
            };
            self.cmd_cursor = self.cmd_buf.chars().count();
            self.revision += 1;
        }
    }

    /// Feed raw terminal bytes, decoding arrows, F2/F3/F4, Home/End,
    /// PgUp/PgDn, Delete, Tab, Ctrl shortcuts, and UTF-8 characters.
    pub fn feed_input(&mut self, data: &[u8]) -> Vec<TuiAction> {
        let mut actions = Vec::new();
        let mut index = 0;
        while index < data.len() {
            let byte = data[index];
            if byte == 0x1B {
                let rest = &data[index + 1..];
                let (key, consumed) = if rest.starts_with(b"[A") {
                    (Some(TuiKey::ArrowUp), 3)
                } else if rest.starts_with(b"[B") {
                    (Some(TuiKey::ArrowDown), 3)
                } else if rest.starts_with(b"[C") {
                    (Some(TuiKey::ArrowRight), 3)
                } else if rest.starts_with(b"[D") {
                    (Some(TuiKey::ArrowLeft), 3)
                } else if rest.starts_with(b"[H") || rest.starts_with(b"OH") {
                    (Some(TuiKey::Home), 3)
                } else if rest.starts_with(b"[F") || rest.starts_with(b"OF") {
                    (Some(TuiKey::End), 3)
                } else if rest.starts_with(b"[1~") {
                    (Some(TuiKey::Home), 4)
                } else if rest.starts_with(b"[4~") {
                    (Some(TuiKey::End), 4)
                } else if rest.starts_with(b"[3~") {
                    (Some(TuiKey::Delete), 4)
                } else if rest.starts_with(b"[5~") {
                    (Some(TuiKey::PageUp), 4)
                } else if rest.starts_with(b"[6~") {
                    (Some(TuiKey::PageDown), 4)
                } else if rest.starts_with(b"OQ") {
                    (Some(TuiKey::F2), 3)
                } else if rest.starts_with(b"OR") {
                    (Some(TuiKey::F3), 3)
                } else if rest.starts_with(b"OS") {
                    (Some(TuiKey::F4), 3)
                } else if rest.starts_with(b"[12~") {
                    (Some(TuiKey::F2), 5)
                } else if rest.starts_with(b"[13~") {
                    (Some(TuiKey::F3), 5)
                } else if rest.starts_with(b"[14~") {
                    (Some(TuiKey::F4), 5)
                } else {
                    (Some(TuiKey::Escape), 1)
                };
                if let Some(key) = key {
                    let action = self.handle_key(key);
                    if action != TuiAction::None {
                        actions.push(action);
                    }
                }
                index += consumed;
                continue;
            }
            if byte >= 0x80 {
                if let Ok(text) = std::str::from_utf8(&data[index..]) {
                    if let Some(ch) = text.chars().next() {
                        let action = self.handle_key(TuiKey::Char(ch));
                        if action != TuiAction::None {
                            actions.push(action);
                        }
                        index += ch.len_utf8();
                        continue;
                    }
                }
                index += 1;
                continue;
            }
            let key = match byte {
                0x03 => Some(TuiKey::CtrlC),
                0x06 => Some(TuiKey::F2),    // Ctrl+F -> Filter
                0x07 => Some(TuiKey::F4),    // Ctrl+G -> Mods
                0x09 => Some(TuiKey::Tab),
                0x0D | 0x0A => Some(TuiKey::Enter),
                0x10 => Some(TuiKey::F3),    // Ctrl+P -> Players
                0x15 => Some(TuiKey::CtrlU), // Ctrl+U -> clear prompt
                0x7F | 0x08 => Some(TuiKey::Backspace),
                0x20..=0x7E => Some(TuiKey::Char(byte as char)),
                _ => None,
            };
            if let Some(key) = key {
                let action = self.handle_key(key);
                if action != TuiAction::None {
                    actions.push(action);
                }
            }
            index += 1;
        }
        actions
    }

    // -- rendering --------------------------------------------------------

    /// One dashboard frame: exactly `rows` strings, each exactly `cols`
    /// visible columns (ANSI sequences do not count).
    ///
    /// Minimal single-column layout (deterministic — no clock, no randomness):
    /// 1. Single-line unified top status bar (MEGU pill, state, TPS, RAM GB,
    ///    CPU %, players, and wide-mode chunks/mods/address)
    /// 2. One-line section divider + scrolling log / players / mods body
    /// 3. Always-ready `>` command prompt
    /// 4. Compact key-hint footer with warning/error counts
    pub fn render_frame(&self, cols: usize, rows: usize, color: bool, ascii: bool) -> Vec<String> {
        let cols = cols.max(8);
        let rows = rows.max(4);

        let mut lines = Vec::with_capacity(rows);
        lines.push(self.topbar_line(cols, color, ascii));

        // Reserve 1 topbar + 1 always-ready prompt + 1 footer = 3 chrome rows.
        let body_rows = rows.saturating_sub(3);
        if body_rows > 0 {
            let body = self.body(body_rows, cols, color, ascii);
            lines.extend(body.into_iter().take(body_rows));
        }
        while lines.len() < rows - 2 {
            lines.push(String::new());
        }
        lines.push(self.prompt_line(cols, color, ascii));
        lines.push(self.footer_line(cols, color, ascii));

        lines
            .iter()
            .map(|line| {
                if ascii {
                    clip(&asciify(&strip_ansi(line)), cols)
                } else {
                    clip(line, cols)
                }
            })
            .collect()
    }

    fn topbar_line(&self, cols: usize, color: bool, ascii: bool) -> String {
        let status = self.status_word();
        let bullet = if ascii { "*" } else { "●" };
        let brand = if color {
            paint(" MEGU ", "brand", true)
        } else {
            "MEGU".to_string()
        };
        let status_pill = if color {
            paint(&format!(" {bullet} {status} "), status_pill_style(status), true)
        } else {
            format!("{bullet} {status}")
        };

        let tps = self.tps_current();
        let mspt = self.mspt_current();
        let tps_short = format!(
            "{} {}",
            paint("TPS", "muted", color),
            paint(&tps.map_or_else(|| "--".to_string(), |v| format!("{v:.1}")), tps_band(tps), color),
        );
        let tps_full = match (tps, mspt) {
            (Some(t), Some(m)) => format!(
                "{} {} {}",
                paint("TPS", "muted", color),
                paint(&format!("{t:.1}"), tps_band(Some(t)), color),
                paint(&format!("({m:.1}ms)"), mspt_band(Some(m)), color),
            ),
            _ => tps_short.clone(),
        };

        let ram = format!(
            "{} {}",
            paint("RAM", "muted", color),
            paint(&format_ram_gb(self.mem_current()), mem_band(self.mem_current()), color),
        );
        let cpu_val = self
            .cpu_current()
            .map_or_else(|| "--%".to_string(), |v| format!("{v:.0}%"));
        let cpu = format!(
            "{} {}",
            paint("CPU", "muted", color),
            paint(&cpu_val, cpu_band(self.cpu_current()), color),
        );

        let online = self.players_online();
        let max_text = self
            .players_max
            .map_or_else(|| "?".to_string(), |m| m.to_string());
        let players = format!(
            "{} {}",
            paint("Players", "muted", color),
            paint(
                &format!("{online}/{max_text}"),
                if online > 0 { "white" } else { "muted" },
                color,
            ),
        );

        // Optional wide-terminal extras appended in priority order when space permits.
        let mut extras: Vec<String> = Vec::new();
        if let Some(hotspot) = self.hotspots.back() {
            let warn_mark = if ascii { "!" } else { "⚠" };
            let badge = match (hotspot.cx, hotspot.cz, hotspot.quarantined) {
                (Some(cx), Some(cz), true) => format!("{warn_mark} Quarantine ({cx},{cz})"),
                (Some(cx), Some(cz), false) => format!("{warn_mark} Hotspot ({cx},{cz})"),
                (_, _, true) => format!("{warn_mark} Quarantined"),
                _ => format!("{warn_mark} Hotspot"),
            };
            extras.push(paint(&badge, "red", color));
        }
        if let Some(chunks) = self.loaded_chunks {
            extras.push(format!(
                "{} {}",
                paint("Chunks", "muted", color),
                paint(&thousands(chunks), "white", color),
            ));
        }
        let active_mods = self.mods.active();
        let mod_warns = self.mod_warnings();
        if active_mods > 0 || self.mods.fallback > 0 || mod_warns > 0 {
            let dot = if ascii { "-" } else { "·" };
            let mut mod_seg = format!(
                "{} {}",
                paint("Mods", "muted", color),
                paint(&active_mods.to_string(), "sky", color),
            );
            if mod_warns > 0 {
                mod_seg.push_str(&format!(
                    " {} {}",
                    paint(dot, "dim", color),
                    paint(&format!("{mod_warns}w"), "amber", color),
                ));
            }
            extras.push(mod_seg);
        }
        if let Some(port) = self.port {
            let addr = format!("{}:{port}", self.bind.as_deref().unwrap_or("server"));
            extras.push(paint(&addr, "muted", color));
        }

        // Build progressively: prefer full TPS(ms) + core metrics + as many extras as fit.
        let inner_width = cols.saturating_sub(2);
        let core_full = [
            brand.clone(),
            status_pill.clone(),
            tps_full,
            ram.clone(),
            cpu.clone(),
            players.clone(),
        ];
        let core_short = [brand, status_pill, tps_short, ram, cpu, players];

        for (base, sep) in [
            (core_full.as_slice(), "  "),
            (core_short.as_slice(), "  "),
            (core_short.as_slice(), " "),
        ] {
            let mut line = base.join(sep);
            if vlen(&line) <= inner_width {
                for extra in &extras {
                    let candidate = format!("{line}{sep}{extra}");
                    if vlen(&candidate) <= inner_width {
                        line = candidate;
                    }
                }
                return paint_bar(&format!(" {line}"), cols, BG_BAR, color);
            }
        }
        for count in [5usize, 4, 3, 2] {
            let line = core_short[..count].join(" ");
            if vlen(&line) <= inner_width {
                return paint_bar(&format!(" {line}"), cols, BG_BAR, color);
            }
        }
        paint_bar(&format!(" {}", core_short[..2].join(" ")), cols, BG_BAR, color)
    }

    fn body(&self, body_rows: usize, cols: usize, color: bool, ascii: bool) -> Vec<String> {
        match self.view {
            TuiView::Players => self.players_section(body_rows, cols, color, ascii),
            TuiView::Mods => self.mods_section(body_rows, cols, color, ascii),
            TuiView::Log => self.log_section(body_rows, cols, color, ascii),
        }
    }

    fn section_header(
        &self,
        title: &str,
        cols: usize,
        color: bool,
        ascii: bool,
        detail: Option<&str>,
    ) -> String {
        let bar_ch = if ascii { '-' } else { '─' };
        let lead = format!("{bar_ch} {title} ");
        let right = match detail {
            Some(d) if !d.is_empty() => format!(" {d} {bar_ch}"),
            _ => String::new(),
        };
        let lead_len = lead.chars().count();
        let right_len = right.chars().count();
        if lead_len + right_len < cols {
            let fill_len = cols - lead_len - right_len;
            let fill: String = std::iter::repeat_n(bar_ch, fill_len).collect();
            let detail_text = detail.map_or_else(String::new, |d| format!(" {d} "));
            let tail_text = if right.is_empty() {
                String::new()
            } else {
                bar_ch.to_string()
            };
            return format!(
                "{}{}{}{}{}",
                paint(&format!("{bar_ch} "), "border", color),
                paint(title, "lavender", color),
                paint(&format!(" {fill}"), "border", color),
                paint(&detail_text, "muted", color),
                paint(&tail_text, "border", color),
            );
        }
        paint(&fit_plain(&lead, cols, bar_ch), "muted", color)
    }

    fn log_section(&self, body_rows: usize, cols: usize, color: bool, ascii: bool) -> Vec<String> {
        if body_rows == 0 {
            return Vec::new();
        }
        // On ultra-short terminals (e.g. 4 rows total -> 1 body row), skip the
        // divider so the latest log line stays visible.
        if body_rows == 1 {
            let entries = self.selected_entries(self.log_filter);
            return match entries.last() {
                Some(entry) => vec![format_log_line(entry, 0, color, ascii)],
                None => vec![paint("  no events yet", "muted", color)],
            };
        }
        let mode = self.log_filter;
        let entries = self.selected_entries(mode);
        let dot = if ascii { " - " } else { " · " };
        let mut detail = match mode {
            LogFilterMode::Events => {
                let hidden = self
                    .log
                    .iter()
                    .filter(|entry| entry.category == LogCategory::Telemetry)
                    .count();
                if hidden > 0 {
                    format!("Events{dot}{hidden} hidden")
                } else {
                    "Events".to_string()
                }
            }
            LogFilterMode::Alerts => format!(
                "Alerts{dot}{} alert{}",
                entries.len(),
                if entries.len() == 1 { "" } else { "s" }
            ),
            LogFilterMode::All => format!("All log{dot}{} lines", entries.len()),
        };
        let visible_rows = body_rows - 1;
        let max_scroll = entries.len().saturating_sub(visible_rows);
        let scroll = self.log_scroll.min(max_scroll);
        if scroll > 0 {
            let up = if ascii { "^" } else { "↑" };
            detail.push_str(&format!("{dot}{up}{scroll}"));
        }
        let mut out = vec![self.section_header("Log & Chat", cols, color, ascii, Some(&detail))];
        let end = entries.len().saturating_sub(scroll);
        let start = end.saturating_sub(visible_rows);
        let slice = &entries[start..end];
        if slice.is_empty() {
            let message = if mode == LogFilterMode::Alerts {
                "no alerts"
            } else {
                "no events yet"
            };
            out.push(paint(&format!("  {message}"), "muted", color));
        } else {
            let mut banner_row = 0usize;
            for entry in slice {
                if entry.category == LogCategory::Banner {
                    out.push(format_log_line(entry, banner_row, color, ascii));
                    banner_row += 1;
                } else {
                    out.push(format_log_line(entry, 0, color, ascii));
                }
            }
        }
        out
    }

    fn players_section(
        &self,
        body_rows: usize,
        cols: usize,
        color: bool,
        ascii: bool,
    ) -> Vec<String> {
        if body_rows == 0 {
            return Vec::new();
        }
        let online = self.players_online();
        let max_text = self
            .players_max
            .map_or_else(|| "?".to_string(), |m| m.to_string());
        let dot = if ascii { " - " } else { " · " };
        let mut detail = format!("Status {}", self.status_word());
        if let Some(uptime) = self.uptime_ticks {
            detail.push_str(&format!("{dot}uptime {uptime} ticks"));
        }
        if let Some(chunks) = self.loaded_chunks {
            detail.push_str(&format!("{dot}{} chunks", thousands(chunks)));
        }
        let title = format!("Players ({online}/{max_text})");
        let mut out = vec![self.section_header(&title, cols, color, ascii, Some(&detail))];
        if body_rows == 1 {
            return out;
        }
        if online == 0 {
            out.push(paint("  No players online (server idle)", "muted", color));
            return out;
        }
        let names = self.player_names();
        if names.is_empty() {
            out.push(paint(
                &format!("  {online} player(s) online (names not reported)"),
                "muted",
                color,
            ));
            return out;
        }
        let bullet = if ascii { "*" } else { "●" };
        for name in names.iter().take(body_rows - 1) {
            let address = self
                .players
                .get(*name)
                .and_then(|addr| addr.as_deref())
                .unwrap_or("connected");
            out.push(format!(
                "  {} {}  {}",
                paint(bullet, "mint", color),
                paint(&fit_plain(name, 18, ' '), "white", color),
                paint(address, "muted", color),
            ));
        }
        out
    }

    fn mods_section(&self, body_rows: usize, cols: usize, color: bool, ascii: bool) -> Vec<String> {
        if body_rows == 0 {
            return Vec::new();
        }
        let dot = if ascii { " - " } else { " · " };
        let mut detail = format!("{} active", self.mods.active());
        if self.mods.fallback > 0 {
            detail.push_str(&format!("{dot}{} fallback", self.mods.fallback));
        }
        let warnings = self.mod_warnings();
        if warnings > 0 {
            detail.push_str(&format!(
                "{dot}{warnings} warning{}",
                if warnings == 1 { "" } else { "s" }
            ));
        }
        let mut out = vec![self.section_header("Mods", cols, color, ascii, Some(&detail))];
        if body_rows == 1 {
            return out;
        }
        if self.mod_details.is_empty() {
            out.push(paint("  No adapted mods recorded", "muted", color));
            return out;
        }
        let ok_mark = if ascii { "*" } else { "●" };
        let warn_mark = if ascii { "!" } else { "⚠" };
        for item in self.mod_details.iter().take(body_rows - 1) {
            let low = item.detail.to_lowercase();
            let is_warn = low.contains("missing") || low.contains("damaged");
            let icon = if is_warn {
                paint(warn_mark, "amber", color)
            } else {
                paint(ok_mark, "mint", color)
            };
            let tier = tier_label(&item.tier);
            out.push(format!(
                "  {icon} {}  {}  {}  {}",
                paint(&fit_plain(&item.file, 18, ' '), "white", color),
                paint(&fit_plain(&item.mod_id, 12, ' '), "lavender", color),
                paint(&fit_plain(&tier, 18, ' '), "sky", color),
                paint(&item.detail, if is_warn { "amber" } else { "muted" }, color),
            ));
        }
        out
    }

    fn prompt_line(&self, cols: usize, color: bool, ascii: bool) -> String {
        if self.stopping {
            let text = format!(
                " stopping megu gracefully{}",
                if ascii { "..." } else { "…" }
            );
            return paint_bar(&paint(&text, "amber", color), cols, BG_INPUT, color);
        }
        if let Some(code) = self.exited {
            let dot = if ascii { " - " } else { " · " };
            return paint_bar(
                &paint(&format!(" megu exited{dot}code {code}"), "muted", color),
                cols,
                BG_INPUT,
                color,
            );
        }
        let chevron = paint(">", "lavender_bold", color);
        let hint = if ascii {
            "Up/Dn history - Tab"
        } else {
            "↑↓ history · Tab"
        };
        if self.cmd_buf.is_empty() {
            let placeholder = "Type a command (help, list, say, stop)...";
            let left = format!(" {chevron} {}", paint(placeholder, "dim", color));
            let right = paint(hint, "dim", color);
            let used = vlen(&left) + vlen(&right) + 2;
            if used <= cols {
                let pad = " ".repeat(cols - vlen(&left) - vlen(&right) - 1);
                return paint_bar(&format!("{left}{pad}{right}"), cols, BG_INPUT, color);
            }
            return paint_bar(&left, cols, BG_INPUT, color);
        }

        // Render active command buffer with a visible block cursor at `cmd_cursor`.
        let chars: Vec<char> = self.cmd_buf.chars().collect();
        let cursor_pos = self.cmd_cursor.min(chars.len());
        let before: String = chars[..cursor_pos].iter().collect();
        let (cursor_glyph, after): (String, String) = if cursor_pos < chars.len() {
            let under = chars[cursor_pos].to_string();
            let rest: String = chars[cursor_pos + 1..].iter().collect();
            (
                if color {
                    paint(&under, "cursor", true)
                } else {
                    under
                },
                rest,
            )
        } else {
            let block = if color {
                paint(" ", "cursor", true)
            } else if ascii {
                "_".to_string()
            } else {
                "█".to_string()
            };
            (block, String::new())
        };
        let left = format!(
            " {chevron} {}{}{}",
            paint(&before, "white", color),
            cursor_glyph,
            paint(&after, "white", color),
        );
        let right_hint = if ascii {
            "Enter send"
        } else {
            "Enter send · ↑↓ history"
        };
        let right = paint(right_hint, "dim", color);
        if vlen(&left) + vlen(&right) + 3 <= cols {
            let pad = " ".repeat(cols - vlen(&left) - vlen(&right) - 1);
            return paint_bar(&format!("{left}{pad}{right}"), cols, BG_INPUT, color);
        }
        paint_bar(&left, cols, BG_INPUT, color)
    }

    fn footer_line(&self, cols: usize, color: bool, ascii: bool) -> String {
        let mode = self.log_filter;
        let chip = |key: &str, active: bool| -> String {
            if color {
                paint(
                    &format!(" {key} "),
                    if active { "key_active" } else { "key_chip" },
                    true,
                )
            } else {
                format!("[{key}]")
            }
        };
        let filter_seg = format!(
            "{} {}",
            chip("F2", false),
            paint(&format!("Filter:{}", mode.as_str()), "muted", color),
        );
        let players_seg = format!(
            "{} {}",
            chip("F3", self.view == TuiView::Players),
            paint(
                if self.view == TuiView::Players {
                    "Log"
                } else {
                    "Players"
                },
                if self.view == TuiView::Players {
                    "white"
                } else {
                    "muted"
                },
                color,
            ),
        );
        let mods_seg = format!(
            "{} {}",
            chip("F4", self.view == TuiView::Mods),
            paint(
                if self.view == TuiView::Mods {
                    "Log"
                } else {
                    "Mods"
                },
                if self.view == TuiView::Mods {
                    "white"
                } else {
                    "muted"
                },
                color,
            ),
        );
        let scroll_seg = format!(
            "{} {}",
            chip("PgUp/Dn", self.log_scroll > 0),
            paint("Scroll", "muted", color),
        );
        let stop_seg = format!(
            "{} {}",
            chip("Ctrl+C", false),
            paint("Stop", "muted", color),
        );

        let dot = if ascii { " - " } else { " · " };
        let counts = format!(
            "{}{}{}",
            paint(
                &format!("{} warn", self.warnings),
                if self.warnings > 0 { "amber" } else { "muted" },
                color,
            ),
            paint(dot, "dim", color),
            paint(
                &format!("{} err", self.errors),
                if self.errors > 0 { "red" } else { "muted" },
                color,
            ),
        );
        let compact_counts = paint(&format!("{}w {}e", self.warnings, self.errors), "muted", color);

        let full_keys = [
            filter_seg.clone(),
            players_seg.clone(),
            mods_seg.clone(),
            scroll_seg,
            stop_seg.clone(),
        ];
        let short_keys = [filter_seg, players_seg, mods_seg, stop_seg];

        for (keys, right) in [
            (full_keys.as_slice(), &counts),
            (short_keys.as_slice(), &counts),
            (short_keys.as_slice(), &compact_counts),
        ] {
            let left = format!(" {}", keys.join("  "));
            if vlen(&left) + vlen(right) + 2 <= cols {
                let pad = " ".repeat(cols - vlen(&left) - vlen(right) - 1);
                return paint_bar(&format!("{left}{pad}{right}"), cols, BG_BAR, color);
            }
            let tight = format!(" {}", keys.join(" "));
            if vlen(&tight) + vlen(right) + 2 <= cols {
                let pad = " ".repeat(cols - vlen(&tight) - vlen(right) - 1);
                return paint_bar(&format!("{tight}{pad}{right}"), cols, BG_BAR, color);
            }
        }
        paint_bar(
            &format!(" {} {compact_counts}", short_keys.join(" ")),
            cols,
            BG_BAR,
            color,
        )
    }
}

/// "tier2 shim" -> "compatibility shim", "tier3" -> "runtime adapter".
fn tier_label(tier: &str) -> String {
    let low = tier.to_lowercase();
    if low.contains("tier2") {
        return "compatibility shim".to_string();
    }
    if low.contains("tier3") {
        return "runtime adapter".to_string();
    }
    if tier.is_empty() {
        "adapted".to_string()
    } else {
        tier.to_string()
    }
}

/// Filter bucket for a decoded line, refining the wrapper's `alert` into
/// `Warn`/`Error`/`Security`.
fn classify_kind(line: &str, kind: LogLineKind) -> LogCategory {
    if kind == LogLineKind::Banner || is_banner_line(line) {
        return LogCategory::Banner;
    }
    if matches!(kind, LogLineKind::Tick | LogLineKind::Size | LogLineKind::MsPerChunk)
        || looks_telemetry(line)
    {
        return LogCategory::Telemetry;
    }
    if matches!(kind, LogLineKind::Hotspot | LogLineKind::Quarantine) {
        return LogCategory::Warn;
    }
    if matches!(
        kind,
        LogLineKind::Ready
            | LogLineKind::Join
            | LogLineKind::Leave
            | LogLineKind::Lan
            | LogLineKind::Mods
            | LogLineKind::Mod
            | LogLineKind::Status
            | LogLineKind::Players
            | LogLineKind::Chunks
            | LogLineKind::Memory
            | LogLineKind::Sleep
            | LogLineKind::Wake
    ) {
        return LogCategory::Event;
    }
    classify_line(line)
}

/// Best-effort category for a raw line: banner, telemetry, alert, event.
fn classify_line(line: &str) -> LogCategory {
    if is_banner_line(line) {
        return LogCategory::Banner;
    }
    if looks_telemetry(line) {
        return LogCategory::Telemetry;
    }
    let lower = line.to_lowercase();
    if is_fatal(line) || has_error(line) {
        return LogCategory::Error;
    }
    if line.contains("[security]") {
        return LogCategory::Security;
    }
    if has_warn(line)
        || lower.contains("quarantin")
        || lower.contains("hotspot")
        || lower.contains("slow chunk")
    {
        return LogCategory::Warn;
    }
    LogCategory::Event
}

/// Structured, category-aware rendering for a scrolling-log line.
///
/// Startup banner lines keep their exact text (no tag prefix) and carry a
/// soft lavender → rose gradient when `color` is enabled. Regular log lines
/// get a crisp 4-column category badge (`MOD`, `JOIN`, `CHAT`, `WARN`, `SEC`,
/// `ERR`, `INFO`, …) while preserving the full original line text.
fn format_log_line(entry: &LogEntry, banner_row: usize, color: bool, _ascii: bool) -> String {
    let line = entry.line.as_str();
    if entry.category == LogCategory::Banner {
        if !color {
            return line.to_string();
        }
        let tone = match banner_row {
            0..=2 => "lavender",
            3..=4 => "rose",
            _ => "dim",
        };
        return paint(line, tone, true);
    }

    let stripped = line.trim_start();
    let lower = stripped.to_lowercase();
    let (tag, tag_tone, body_tone) = match entry.category {
        LogCategory::Error => ("ERR ", "red", "red"),
        LogCategory::Security => ("SEC ", "rose", "rose"),
        LogCategory::Warn => ("WARN", "amber", "amber"),
        LogCategory::Telemetry => ("STAT", "dim", "muted"),
        _ if contains_word(&lower, "joined") => ("JOIN", "mint_bold", "mint"),
        _ if contains_word(&lower, "left") || contains_word(&lower, "disconnected") => {
            ("LEFT", "muted", "muted")
        }
        _ if stripped.starts_with("[chat]") || angle_name(stripped) => ("CHAT", "sky", "sky"),
        _ if stripped.starts_with("[cmd]") || stripped.starts_with("[console]") => {
            ("CMD ", "lavender_bold", "text")
        }
        _ if stripped.starts_with("[live-mods]") => ("MOD ", "lavender", "muted"),
        _ => ("INFO", "sky", "text"),
    };

    if !color {
        return format!(" {tag} {line}");
    }
    format!(
        " {} {}",
        paint(tag, tag_tone, true),
        paint(line, body_tone, true),
    )
}

/// `re.match(r"<\S+>", stripped)` — a `<name>` chat prefix.
fn angle_name(stripped: &str) -> bool {
    if !stripped.starts_with('<') {
        return false;
    }
    match stripped.find('>') {
        Some(end) => end > 1 && !stripped[1..end].chars().any(|c| c.is_whitespace()),
        None => false,
    }
}

fn tps_band(cur: Option<f64>) -> &'static str {
    match cur {
        None => "muted",
        Some(v) if v >= 19.5 => "mint_bold",
        Some(v) if v >= 18.0 => "amber",
        Some(_) => "red",
    }
}

fn mspt_band(cur: Option<f64>) -> &'static str {
    match cur {
        None => "muted",
        Some(v) if v < 30.0 => "mint",
        Some(v) if v < 45.0 => "amber",
        Some(_) => "red",
    }
}

fn cpu_band(cur: Option<f64>) -> &'static str {
    match cur {
        None => "muted",
        Some(v) if v < 50.0 => "lavender",
        Some(v) if v < 100.0 => "amber",
        Some(_) => "red",
    }
}

fn mem_band(cur: Option<f64>) -> &'static str {
    match cur {
        None => "muted",
        Some(v) if v < 1024.0 => "sky",
        Some(v) if v < 2048.0 => "amber",
        Some(_) => "red",
    }
}

fn status_pill_style(word: &str) -> &'static str {
    match word {
        "ONLINE" => "pill_ok",
        "SLEEPING" => "pill_sleep",
        "BUSY" | "BOOTING" => "pill_busy",
        "ALERT" => "pill_alert",
        _ => "muted",
    }
}

// ------------------------------------------------------------- line shapes

/// `megu v<ver> listening <bind>:<port>` anywhere in the line.
fn parse_ready(line: &str) -> Option<(String, String, u64)> {
    let start = line.find("megu v")?;
    let mut cursor = Cursor::new(&line[start + 6..]);
    // version token: [0-9]+(.[0-9]+)*
    let token = cursor.next_word()?;
    if !token
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    if !cursor.take_ws1("listening") {
        return None;
    }
    let addr = cursor.next_word()?;
    let colon = addr.rfind(':')?;
    let (bind, port_text) = (&addr[..colon], &addr[colon + 1..]);
    if bind.is_empty()
        || port_text.is_empty()
        || !port_text.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some((
        token.to_string(),
        bind.to_string(),
        port_text.parse::<u64>().ok()?,
    ))
}

/// `[tick] observed_tps=F avg_work_ms=F max_work_ms=F over_budget=a/b`.
fn parse_tick(line: &str) -> Option<(f64, f64, f64, u64, u64)> {
    let start = line.find("[tick]")?;
    let mut cursor = Cursor::new(&line[start + 6..]);
    if !cursor.take_ws1("observed_tps=") {
        return None;
    }
    let tps = cursor.take_float()?;
    if !cursor.take_ws1("avg_work_ms=") {
        return None;
    }
    let work = cursor.take_float()?;
    if !cursor.take_ws1("max_work_ms=") {
        return None;
    }
    let max_work = cursor.take_float()?;
    if !cursor.take_ws1("over_budget=") {
        return None;
    }
    let over = cursor.take_int()?;
    if !cursor.take("/") {
        return None;
    }
    let sampled = cursor.take_int()?;
    Some((
        tps,
        work,
        max_work,
        over.max(0) as u64,
        sampled.max(0) as u64,
    ))
}

/// `[spine] <name> joined from <addr> (uuid <hex>)`.
fn parse_join(line: &str) -> Option<(String, String)> {
    let start = line.find("[spine]")?;
    let mut cursor = Cursor::new(&line[start + 7..]);
    if !cursor.skip_ws() {
        return None;
    }
    let name = cursor.next_word()?;
    if !cursor.take_ws1("joined") || !cursor.take_ws1("from") {
        return None;
    }
    let addr = cursor.next_word()?;
    if !cursor.take_ws1("(uuid") || !cursor.skip_ws() {
        return None;
    }
    let uuid_word = cursor.next_word()?;
    let hex = uuid_word.strip_suffix(')')?;
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((name.to_string(), addr.to_string()))
}

/// `[spine] <name> left (from <addr>)` or `[spine] <name> disconnected:`.
fn parse_leave(line: &str) -> Option<String> {
    let start = line.find("[spine]")?;
    let mut cursor = Cursor::new(&line[start + 7..]);
    if !cursor.skip_ws() {
        return None;
    }
    let name = cursor.next_word()?;
    if !cursor.skip_ws() {
        return None;
    }
    if cursor.take("left") {
        if cursor.take_ws1("(from") && cursor.skip_ws() {
            let addr = cursor.next_word()?;
            if addr.ends_with(')') {
                return Some(name.to_string());
            }
        }
        return None;
    }
    if cursor.take("disconnected:") {
        return Some(name.to_string());
    }
    None
}

/// `[live-mods] N jar(s): a auto-skipped, b native-bundle, c tier2-shim, d tier3`.
fn parse_mods_summary(line: &str) -> Option<(u64, ModCounts)> {
    let start = line.find("[live-mods]")?;
    let mut cursor = Cursor::new(&line[start + 11..]);
    if !cursor.skip_ws() {
        return None;
    }
    let jars = cursor.take_int()?;
    if !cursor.take_ws1("jar(s):") {
        return None;
    }
    let auto_skipped = cursor.take_ws1_int()?;
    if !cursor.take_ws1("auto-skipped,") {
        return None;
    }
    let native = cursor.take_ws1_int()?;
    if !cursor.take_ws1("native-bundle,") {
        return None;
    }
    let shim = cursor.take_ws1_int()?;
    if !cursor.take_ws1("tier2-shim,") {
        return None;
    }
    let fallback = cursor.take_ws1_int()?;
    if !cursor.take_ws1("tier3") {
        return None;
    }
    Some((
        jars.max(0) as u64,
        ModCounts {
            auto_skipped: auto_skipped.max(0) as u64,
            native: native.max(0) as u64,
            shim: shim.max(0) as u64,
            fallback: fallback.max(0) as u64,
        },
    ))
}

/// `[live-mods] dual-loader: u unified, c client-only, s superseded, r
/// rejected, d duplicate ids, m missing deps, h verified damage hooks`.
fn parse_dual_loader(line: &str) -> Option<DualLoader> {
    let start = line.find("[live-mods]")?;
    let mut cursor = Cursor::new(&line[start + 11..]);
    if !cursor.take_ws1("dual-loader:") {
        return None;
    }
    let unified = cursor.take_ws1_int()?;
    if !cursor.take_ws1("unified,") {
        return None;
    }
    let client_only = cursor.take_ws1_int()?;
    if !cursor.take_ws1("client-only,") {
        return None;
    }
    let superseded = cursor.take_ws1_int()?;
    if !cursor.take_ws1("superseded,") {
        return None;
    }
    let rejected = cursor.take_ws1_int()?;
    if !cursor.take_ws1("rejected,") {
        return None;
    }
    let duplicate_ids = cursor.take_ws1_int()?;
    if !cursor.take_ws1("duplicate") || !cursor.take_ws1("ids,") {
        return None;
    }
    let missing_deps = cursor.take_ws1_int()?;
    if !cursor.take_ws1("missing") || !cursor.take_ws1("deps,") {
        return None;
    }
    let damage_hooks = cursor.take_ws1_int()?;
    if !cursor.take_ws1("verified") || !cursor.take_ws1("damage") || !cursor.take_ws1("hooks") {
        return None;
    }
    Some(DualLoader {
        unified: unified.max(0) as u64,
        client_only: client_only.max(0) as u64,
        superseded: superseded.max(0) as u64,
        rejected: rejected.max(0) as u64,
        duplicate_ids: duplicate_ids.max(0) as u64,
        missing_deps: missing_deps.max(0) as u64,
        damage_hooks: damage_hooks.max(0) as u64,
    })
}

/// `[live-mods] <file> (<id>) tier2 shim|tier3: <detail>`.
fn parse_mod_detail(line: &str) -> Option<ModDetail> {
    let start = line.find("[live-mods]")?;
    let mut cursor = Cursor::new(&line[start + 11..]);
    if !cursor.skip_ws() {
        return None;
    }
    let file = cursor.next_word()?;
    if !cursor.take_ws1("(") {
        return None;
    }
    let close = cursor.rest().find(')')?;
    let mod_id = &cursor.rest()[..close];
    if mod_id.is_empty() || mod_id.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    cursor.pos += close + 1;
    if !cursor.skip_ws() {
        return None;
    }
    let tier = if cursor.take("tier2") {
        if !cursor.take_ws1("shim") {
            return None;
        }
        "tier2 shim"
    } else if cursor.take("tier3") {
        "tier3"
    } else {
        return None;
    };
    cursor.skip_ws();
    if !cursor.take(":") {
        return None;
    }
    Some(ModDetail {
        file: file.to_string(),
        mod_id: mod_id.to_string(),
        tier: tier.to_string(),
        detail: cursor.rest().trim().to_string(),
    })
}

/// `[live-mods] mods dir scanned: N jar(s)`.
fn parse_mods_scanned(line: &str) -> Option<u64> {
    let start = line.find("[live-mods]")?;
    let mut cursor = Cursor::new(&line[start + 11..]);
    if !cursor.take_ws1("mods") || !cursor.take_ws1("dir") || !cursor.take_ws1("scanned:") {
        return None;
    }
    let jars = cursor.take_ws1_int()?;
    if !cursor.take_ws1("jar(s)") {
        return None;
    }
    Some(jars.max(0) as u64)
}

/// `Status: <Word> (F TPS, F ms/tick)` — anchored at line start.
fn parse_status(line: &str) -> Option<(String, f64, f64)> {
    let rest = line.strip_prefix("Status:")?;
    let mut cursor = Cursor::new(rest);
    if !cursor.skip_ws() {
        return None;
    }
    let word = cursor.next_word()?;
    if !word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if !cursor.take_ws1("(") {
        return None;
    }
    let tps = cursor.take_float()?;
    if !cursor.take_ws1("TPS,") {
        return None;
    }
    let mspt = cursor.take_ws1_float()?;
    if !cursor.take_ws1("ms/tick)") {
        return None;
    }
    Some((word.to_string(), tps, mspt))
}

/// `Players: n/max (names)` — anchored; names is a comma-separated list.
fn parse_players(line: &str) -> Option<(u64, u64, Option<Vec<String>>)> {
    let rest = line.strip_prefix("Players:")?;
    let mut cursor = Cursor::new(rest);
    if !cursor.skip_ws() {
        return None;
    }
    let online = cursor.take_int()?;
    if !cursor.take("/") {
        return None;
    }
    let max = cursor.take_int()?;
    // `(?:\s+\(([^)]*)\))?` — the names group is optional entirely.
    let names = if cursor.skip_ws() && cursor.take("(") {
        cursor.rest().find(')').map(|close| {
            cursor.rest()[..close]
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        })
    } else {
        None
    };
    Some((online.max(0) as u64, max.max(0) as u64, names))
}

/// `Loaded chunks: n` anchored, or `WorldSummary(chunks=n` anywhere.
fn parse_chunks(line: &str) -> Option<u64> {
    if let Some(rest) = line.strip_prefix("Loaded chunks:") {
        let mut cursor = Cursor::new(rest);
        if cursor.skip_ws() {
            if let Some(chunks) = cursor.take_int() {
                return Some(chunks.max(0) as u64);
            }
        }
    }
    bounded_float_after_marker(line, "WorldSummary(chunks=").map(|v| v as u64)
}

/// `[lan] advertising "motd" on port n`.
fn parse_lan(line: &str) -> Option<(String, u64)> {
    let start = line.find("[lan]")?;
    let mut cursor = Cursor::new(&line[start + 5..]);
    if !cursor.take_ws1("advertising") || !cursor.skip_ws() {
        return None;
    }
    if !cursor.take("\"") {
        return None;
    }
    let close = cursor.rest().find('"')?;
    let motd = cursor.rest()[..close].to_string();
    cursor.pos += close + 1;
    if !cursor.take_ws1("on") || !cursor.take_ws1("port") || !cursor.skip_ws() {
        return None;
    }
    let port = cursor.take_int()?;
    Some((motd, port.max(0) as u64))
}

// ==================================================================== tests

#[cfg(test)]
mod tests {
    use super::*;

    const REALISTIC_LOG: &[&str] = &[
        "[config] loaded server.toml (profile default)",
        "megu v1.21.1 listening 127.0.0.1:25565 (offline identities; no Mojang authentication)",
        "[live-mods] 5 jar(s): 1 auto-skipped, 2 native-bundle, 1 tier2-shim, 1 tier3",
        "[live-mods] dual-loader: 2 unified, 1 client-only, 0 superseded, 0 rejected, 0 duplicate ids, 0 missing deps, 3 verified damage hooks",
        "[live-mods] weirdmod.jar (weirdmod) tier3: 412 classes, 9 site(s) - mixin target missing",
        "[live-mods] liftcraft.jar (liftcraft) tier2 shim: 208 classes, 4 site(s) - damage hooks lifted",
        "[tick] observed_tps=19.97 avg_work_ms=8.21 max_work_ms=21.44 over_budget=0/100",
        "[spine] Alice joined from 127.0.0.1:51112 (uuid 0123456789abcdef0123456789abcdef)",
        "[spine] Bob joined from 127.0.0.1:51130 (uuid fedcba9876543210fedcba9876543210)",
        "[chat] Alice: welcome aboard",
        "[sz] t=400 VmRSS: 523264 kB | mobs=42 drops=7 xps=3 arrows=0 bob=1 pearl=0 primed=0 chests=18 furn=6 crops=11 world=0 inv=2 clients=2",
        "[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms entities=91",
        "[tick] observed_tps=19.41 avg_work_ms=44.10 max_work_ms=58.02 over_budget=3/100",
        "[spine] Alice left (from 127.0.0.1:51112)",
        "Status: Busy (19.4 TPS, 44.1 ms/tick)",
        "Players: 2/20 (Bob, Carol)",
        "Loaded chunks: 1284",
        "Memory: 511 MB",
        "Mods: 2 native, 1 compatibility mode",
        "[security] velocity forwarding refused for Mallory from 10.0.0.9: missing forwarding secret",
        "[FATAL] tick thread panicked: boom; exiting so the server restarts",
    ];

    const DEMO_LINES: &[&str] = &[
        "[live-mods] 5 jar(s): 1 auto-skipped, 2 native-bundle, 1 tier2-shim, 1 tier3",
        "[live-mods] weirdmod.jar (weirdmod) tier3: 412 classes, 9 site(s) - mixin target missing",
        "[live-mods] liftcraft.jar (liftcraft) tier2 shim: 208 classes, 4 site(s) - damage hooks lifted",
        "[anvil] loaded world: WorldSummary(chunks=1284, players=3)",
        "[tick] observed_tps=19.90 avg_work_ms=11.50 max_work_ms=15.77 over_budget=0/100",
        "megu v1.21.1 listening 127.0.0.1:25565 (offline identities; no Mojang authentication)",
        "[spine] Bob joined from 127.0.0.1:51130 (uuid fedcba9876543210fedcba9876543210)",
        "[sz] t=500 VmRSS: 524288 kB | mobs=44 drops=8 xps=4 arrows=0 bob=0 pearl=0 primed=0 chests=18 furn=6 crops=12 world=0 inv=2 clients=2",
        "Players: 2/20 (Bob, Carol)",
        "[security] velocity forwarding refused for Mallory from 10.0.0.9: missing forwarding secret",
        "[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms entities=91",
    ];

    fn parse(lines: &[&str]) -> ServerTuiState {
        let mut state = ServerTuiState::new();
        for line in lines {
            state.ingest_log_line(line);
        }
        state
    }

    /// The deterministic demo snapshot: banner, canned realistic log and a
    /// synthetic metric wave — the `demo_parser()` of the Python wrapper.
    fn demo_state() -> ServerTuiState {
        let mut state = ServerTuiState::new();
        state.inject_banner(false);
        for line in DEMO_LINES {
            state.ingest_log_line(line);
        }
        state.threads = Some(14);
        let round2 = |v: f64| (v * 100.0).round() / 100.0;
        for i in 0..72usize {
            let crest = ((i % 24) as f64 - 12.0).abs() / 12.0;
            let dip = (1.0 - (i as f64 - 52.0).abs() / 9.0).max(0.0);
            push_ring(
                &mut state.tps,
                SAMPLE_HISTORY,
                round2(19.2 + 0.8 * crest - 4.5 * dip),
            );
            push_ring(
                &mut state.mspt,
                SAMPLE_HISTORY,
                round2(11.0 + 6.0 * (1.0 - crest) + 41.0 * dip),
            );
            push_ring(
                &mut state.mem_mib,
                SAMPLE_HISTORY,
                round2(468.0 + i as f64 * 0.7 + (i % 6) as f64),
            );
            push_ring(
                &mut state.cpu_pct,
                SAMPLE_HISTORY,
                round2(14.0 + 26.0 * crest + 48.0 * dip),
            );
            push_ring(
                &mut state.ms_per_chunk,
                SAMPLE_HISTORY,
                round2(2.8 + 1.4 * (1.0 - crest) + 5.6 * dip),
            );
        }
        state
    }

    fn frame(state: &ServerTuiState, cols: usize, rows: usize) -> Vec<String> {
        state.render_frame(cols, rows, false, false)
    }

    fn plain(lines: &[String]) -> Vec<String> {
        lines.iter().map(|line| strip_ansi(line)).collect()
    }

    // ----------------------------------------------------- mode selection

    #[test]
    fn cli_aliases_force_plain() {
        for flag in ["--cli", "--plain", "--no-tui", "--nogui"] {
            let (opts, rest) = ConsoleModeOptions::from_args(&[flag.to_string()]);
            assert!(opts.force_cli, "{flag}");
            assert!(!opts.force_tui, "{flag}");
            assert!(rest.is_empty());
            let mode = resolve_console_mode(&opts, true, true, Some("xterm"), false);
            assert_eq!(mode, ServerConsoleMode::PlainCli, "{flag}");
        }
    }

    #[test]
    fn non_tty_forces_plain() {
        let opts = ConsoleModeOptions::default();
        let term = Some("xterm-256color");
        assert_eq!(
            resolve_console_mode(&opts, false, true, term, false),
            ServerConsoleMode::PlainCli
        );
        assert_eq!(
            resolve_console_mode(&opts, true, false, term, false),
            ServerConsoleMode::PlainCli
        );
        assert_eq!(
            resolve_console_mode(&opts, false, false, term, false),
            ServerConsoleMode::PlainCli
        );
    }

    #[test]
    fn dumb_or_missing_term_forces_plain() {
        let opts = ConsoleModeOptions::default();
        for term in [Some(""), Some("dumb"), Some("unknown"), None] {
            assert_eq!(
                resolve_console_mode(&opts, true, true, term, false),
                ServerConsoleMode::PlainCli,
                "{term:?}"
            );
        }
    }

    #[test]
    fn no_color_forces_plain() {
        let opts = ConsoleModeOptions::default();
        assert_eq!(
            resolve_console_mode(&opts, true, true, Some("xterm"), true),
            ServerConsoleMode::PlainCli
        );
    }

    #[test]
    fn interactive_tty_defaults_to_tui() {
        let opts = ConsoleModeOptions::default();
        let expected = if cfg!(windows) {
            ServerConsoleMode::PlainCli
        } else {
            ServerConsoleMode::Tui
        };
        assert_eq!(
            resolve_console_mode(&opts, true, true, Some("xterm-256color"), false),
            expected
        );
    }

    #[test]
    fn force_tui_still_fails_closed() {
        let (opts, _) = ConsoleModeOptions::from_args(&["--tui".to_string()]);
        assert!(opts.force_tui && !opts.force_cli);
        assert_eq!(
            resolve_console_mode(&opts, false, true, Some("xterm"), false),
            ServerConsoleMode::PlainCli
        );
        assert_eq!(
            resolve_console_mode(&opts, true, true, Some("dumb"), false),
            ServerConsoleMode::PlainCli
        );
        let expected = if cfg!(windows) {
            ServerConsoleMode::PlainCli
        } else {
            ServerConsoleMode::Tui
        };
        assert_eq!(
            resolve_console_mode(&opts, true, true, Some("xterm"), false),
            expected
        );
        // --cli wins over --tui in either order.
        for args in [
            vec!["--tui", "--cli"],
            vec!["--cli", "--tui"],
        ] {
            let (opts, _) =
                ConsoleModeOptions::from_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            assert_eq!(
                resolve_console_mode(&opts, true, true, Some("xterm"), false),
                ServerConsoleMode::PlainCli
            );
        }
    }

    #[test]
    fn flag_parsing_collects_all_options() {
        let (opts, rest) = ConsoleModeOptions::from_args(&[
            "--log-filter".to_string(),
            "alerts".to_string(),
            "--no-banner".to_string(),
            "--ascii".to_string(),
            "--world".to_string(),
            "worlds/save".to_string(),
        ]);
        assert_eq!(opts.log_filter, LogFilterMode::Alerts);
        assert!(opts.no_banner);
        assert!(opts.ascii_only);
        assert_eq!(rest, vec!["--world".to_string(), "worlds/save".to_string()]);
        // --log-filter=mode form and "--" passthrough.
        let (opts, rest) = ConsoleModeOptions::from_args(&[
            "--log-filter=all".to_string(),
            "--".to_string(),
            "--nogui".to_string(),
        ]);
        assert_eq!(opts.log_filter, LogFilterMode::All);
        assert_eq!(rest, vec!["--nogui".to_string()]);
        // An invalid value is handed back, not swallowed.
        let (_, rest) = ConsoleModeOptions::from_args(&["--log-filter=bogus".to_string()]);
        assert_eq!(rest, vec!["--log-filter=bogus".to_string()]);
    }

    #[test]
    fn log_filter_cycle_and_normalization() {
        assert_eq!(LogFilterMode::Events.cycle(), LogFilterMode::Alerts);
        assert_eq!(LogFilterMode::Alerts.cycle(), LogFilterMode::All);
        assert_eq!(LogFilterMode::All.cycle(), LogFilterMode::Events);
        assert_eq!(LogFilterMode::normalize("bogus"), LogFilterMode::Events);
        assert_eq!(LogFilterMode::normalize("all"), LogFilterMode::All);
    }

    // ------------------------------------------------------------ decoding

    #[test]
    fn realistic_session_decodes_all_metrics() {
        let state = parse(REALISTIC_LOG);
        assert!(state.ready());
        assert_eq!(state.version.as_deref(), Some("1.21.1"));
        assert_eq!(state.bind.as_deref(), Some("127.0.0.1"));
        assert_eq!(state.port, Some(25565));
        assert!((state.tps_current().unwrap() - 19.4).abs() < 0.001);
        assert!((state.mspt_current().unwrap() - 44.1).abs() < 0.001);
        assert!((state.mem_current().unwrap() - 511.0).abs() < 0.001);
        assert_eq!(state.players_online(), 2);
        assert_eq!(state.players_max(), Some(20));
        assert_eq!(state.player_names(), vec!["Bob", "Carol"]);
        assert_eq!(state.loaded_chunks(), Some(1284));
        assert_eq!(
            state.mods,
            ModCounts {
                auto_skipped: 1,
                native: 2,
                shim: 1,
                fallback: 1
            }
        );
        assert_eq!(state.jars_seen, 5);
        assert_eq!(state.dual_loader.unwrap().unified, 2);
        assert_eq!(state.dual_loader.unwrap().damage_hooks, 3);
        assert_eq!(state.mod_details.len(), 2);
        assert_eq!(state.mod_details[0].tier, "tier3");
        assert_eq!(state.mod_warnings(), 1);
        assert_eq!(state.panics(), 1);
        assert_eq!(state.errors(), 1);
        assert_eq!(state.warnings(), 1);
        assert_eq!(state.uptime_ticks, Some(400));
        assert_eq!(state.clients, Some(2));
        assert!(!state.sleeping());
        assert!(!state.hotspot_alert());
        assert_eq!(state.lines_seen(), REALISTIC_LOG.len() as u64);
        assert_eq!(state.log.len(), REALISTIC_LOG.len());
    }

    #[test]
    fn feed_returns_kinds() {
        let mut state = ServerTuiState::new();
        assert_eq!(
            state.ingest_log_line("megu v1.21.1 listening 127.0.0.1:25565 (x)"),
            LogLineKind::Ready
        );
        assert_eq!(
            state.ingest_log_line(
                "[tick] observed_tps=20.00 avg_work_ms=1.0 max_work_ms=2.0 over_budget=0/100"
            ),
            LogLineKind::Tick
        );
        assert_eq!(
            state.ingest_log_line("[spine] Zoe joined from 127.0.0.1:9 (uuid aaaa)"),
            LogLineKind::Join
        );
        assert_eq!(
            state.ingest_log_line("totally unrecognized chatter"),
            LogLineKind::Log
        );
        assert_eq!(state.ingest_log_line("[chat] Zoe: hi"), LogLineKind::Log);
    }

    #[test]
    fn join_leave_tracking() {
        let mut state = ServerTuiState::new();
        state.ingest_log_line(
            "[spine] Zoe joined from 127.0.0.1:9 (uuid 0123456789abcdef0123456789abcdef)",
        );
        assert_eq!(state.players_online(), 1);
        state.ingest_log_line("[spine] Zoe left (from 127.0.0.1:9)");
        assert_eq!(state.players_online(), 0);
        state.ingest_log_line("[spine] Zoe joined from 127.0.0.1:9 (uuid aaaa)");
        state.ingest_log_line("[spine] Zoe disconnected: timeout");
        assert_eq!(state.players_online(), 0);
    }

    #[test]
    fn telemetry_is_parsed_and_tagged() {
        let mut state = ServerTuiState::new();
        assert_eq!(
            state.ingest_log_line(
                "[tick] observed_tps=18.25 avg_work_ms=31.0 max_work_ms=40.0 over_budget=2/100"
            ),
            LogLineKind::Tick
        );
        assert_eq!(
            state.ingest_log_line("[sz] t=600 VmRSS: 262144 kB | clients=1"),
            LogLineKind::Size
        );
        assert_eq!(
            state.ingest_log_line("[chunkprof] window=60s chunks=1284 (4.2 ms/chunk) map=15.8ms"),
            LogLineKind::MsPerChunk
        );
        assert_eq!(
            state.log.iter().map(|e| e.category).collect::<Vec<_>>(),
            vec![
                LogCategory::Telemetry,
                LogCategory::Telemetry,
                LogCategory::Telemetry
            ]
        );
        assert!((state.tps_current().unwrap() - 18.25).abs() < 0.001);
        assert!((state.mspt_current().unwrap() - 31.0).abs() < 0.001);
        assert!((state.mem_current().unwrap() - 256.0).abs() < 0.001);
        assert_eq!(state.uptime_ticks, Some(600));
        assert_eq!(state.clients, Some(1));
        assert_eq!(state.over_budget, 2);
    }

    #[test]
    fn events_filter_hides_telemetry_but_keeps_metrics() {
        let state = parse(REALISTIC_LOG);
        let events = state.filtered_log_lines(LogFilterMode::Events);
        assert!(events.iter().any(|l| l.contains("joined")));
        assert!(events.iter().any(|l| l.contains("[chat]")));
        assert!(events.iter().any(|l| l.contains("[security]")));
        for spam in ["[tick]", "[sz]", "[chunkprof]"] {
            assert!(!events.iter().any(|l| l.starts_with(spam)), "{spam}");
        }
        let all = state.filtered_log_lines(LogFilterMode::All);
        for spam in ["[tick]", "[sz]", "[chunkprof]"] {
            assert!(all.iter().any(|l| l.starts_with(spam)), "{spam}");
        }
        // The events header advertises the hidden telemetry count.
        let lines = frame(&state, 100, 24);
        let header = lines
            .iter()
            .find(|l| strip_ansi(l).contains("Log & Chat"))
            .expect("log section header");
        assert!(strip_ansi(header).contains("hidden"), "{header}");
    }

    #[test]
    fn alerts_filter_is_alert_only() {
        let state = parse(REALISTIC_LOG);
        let alerts = state.filtered_log_lines(LogFilterMode::Alerts);
        assert!(alerts.iter().any(|l| l.contains("[security]")));
        assert!(alerts.iter().any(|l| l.contains("[FATAL]")));
        assert!(!alerts.iter().any(|l| l.contains("joined")));
        assert!(!alerts.iter().any(|l| l.starts_with("[tick]")));
        assert!(!alerts.iter().any(|l| l.contains("[chat]")));
        // Security lines without error keywords get the Security bucket and
        // count as warnings, matching the wrapper's alert accounting.
        let mut security = ServerTuiState::new();
        security.ingest_log_line("[security] login allowlist active: 3 name(s)");
        assert_eq!(security.log.back().unwrap().category, LogCategory::Security);
        assert_eq!(security.filtered_log_lines(LogFilterMode::Alerts).len(), 1);
        assert_eq!(security.warnings(), 1);
        assert_eq!(security.errors(), 0);
    }

    #[test]
    fn hotspot_and_quarantine_set_guard_alert() {
        let mut state = ServerTuiState::new();
        state.ingest_log_line("Loaded chunks: 1284");
        assert_eq!(
            state.ingest_log_line("[guard] chunk hotspot (12,-4) throttled after 41 ms"),
            LogLineKind::Hotspot
        );
        assert!(state.hotspot_alert());
        assert_eq!(state.hotspots.back().unwrap().cx, Some(12));
        assert_eq!(state.hotspots.back().unwrap().cz, Some(-4));
        assert_eq!(state.warnings(), 1);
        let text = plain(&state.render_frame(100, 24, false, false)).join("\n");
        assert!(text.contains("Hotspot (12,-4)"), "{text}");

        let mut quarantine = ServerTuiState::new();
        quarantine.ingest_log_line("Loaded chunks: 512");
        assert_eq!(
            quarantine
                .ingest_log_line("[quarantine] chunk (7,9) frozen after 3 consecutive faults"),
            LogLineKind::Quarantine
        );
        assert!(quarantine.hotspot_alert());
        assert_eq!(quarantine.quarantine_alerts, 1);
        assert_eq!(quarantine.hotspots.back().unwrap().cx, Some(7));
        let text = plain(&quarantine.render_frame(100, 24, false, false)).join("\n");
        assert!(text.contains("Quarantine (7,9)"), "{text}");
    }

    #[test]
    fn sleeping_and_wake_lines() {
        let mut state = ServerTuiState::new();
        state.ingest_log_line("megu v1.21.1 listening 127.0.0.1:25565 (offline)");
        state.ingest_log_line("[sleep] SLEEPING until a player joins (wake-on-join)");
        assert!(state.sleeping());
        assert_eq!(state.status_word(), "SLEEPING");
        state.ingest_log_line("[sleep] woke from sleep on player join");
        assert!(!state.sleeping());
        assert_eq!(state.status_word(), "ONLINE");
    }

    #[test]
    fn join_wakes_sleeping_server() {
        let mut state = ServerTuiState::new();
        state.ingest_log_line("megu v1.21.1 listening 127.0.0.1:25565 (offline)");
        state.ingest_log_line("[sleep] SLEEPING until a player joins");
        assert!(state.sleeping());
        state.ingest_log_line("[spine] Zoe joined from 127.0.0.1:9 (uuid aaaa)");
        assert!(!state.sleeping());
    }

    #[test]
    fn status_word_bands() {
        let mut state = ServerTuiState::new();
        state.ready = true;
        state.players_online = Some(1);
        state.tps.push_back(19.9);
        state.mspt.push_back(10.0);
        assert_eq!(state.status_word(), "ONLINE");

        state.tps.clear();
        state.tps.push_back(19.0);
        assert_eq!(state.status_word(), "BUSY");

        state.tps.clear();
        state.tps.push_back(17.0);
        assert_eq!(state.status_word(), "ALERT");

        state.tps.clear();
        state.tps.push_back(19.9);
        state.mspt.clear();
        state.mspt.push_back(46.0);
        assert_eq!(state.status_word(), "ALERT");

        state.mspt.clear();
        state.mspt.push_back(10.0);
        state.sleeping = true;
        state.players_online = Some(0);
        assert_eq!(state.status_word(), "SLEEPING");

        state.sleeping = false;
        state.players_online = Some(1);
        state.panics = 1;
        assert_eq!(state.status_word(), "ALERT");
        state.panics = 0;
        state.errors = 1;
        assert_eq!(state.status_word(), "ALERT");

        assert_eq!(ServerTuiState::new().status_word(), "BOOTING");
    }

    // ------------------------------------------------------------- banner

    #[test]
    fn injected_banner_is_categorized_and_not_an_alert() {
        let mut state = ServerTuiState::new();
        assert!(state.inject_banner(false));
        assert!(!state.inject_banner(false));
        state.ingest_log_line("[config] first server line");
        let banner: Vec<&LogEntry> = state
            .log
            .iter()
            .filter(|e| e.category == LogCategory::Banner)
            .collect();
        assert_eq!(banner.len(), BANNER_UNICODE.len());
        assert_eq!(
            banner.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            BANNER_UNICODE
        );
        assert_eq!(state.log[0].category, LogCategory::Banner);
        assert_eq!(state.lines_seen(), 1);
        assert_eq!(state.warnings(), 0);
        assert_eq!(state.errors(), 0);
        assert_eq!(state.panics(), 0);
        let alerts = state.filtered_log_lines(LogFilterMode::Alerts);
        assert!(!alerts.iter().any(|l| BANNER_UNICODE.contains(l)));
        let events = state.filtered_log_lines(LogFilterMode::Events);
        assert_eq!(&events[..BANNER_UNICODE.len()], BANNER_UNICODE);
    }

    #[test]
    fn captured_banner_lines_are_recognized_without_counting() {
        let mut state = ServerTuiState::new();
        for line in BANNER_ASCII {
            state.ingest_log_line(line);
        }
        assert_eq!(
            state.log.iter().map(|e| e.category).collect::<Vec<_>>(),
            vec![LogCategory::Banner; BANNER_ASCII.len()]
        );
        assert_eq!(state.warnings(), 0);
        assert_eq!(state.errors(), 0);
    }

    #[test]
    fn banner_scrolls_off_naturally() {
        let mut state = ServerTuiState::new();
        state.inject_banner(false);
        for index in 0..3 {
            state.ingest_log_line(&format!("[config] boot line {index}"));
        }
        let early = plain(&state.render_frame(80, 24, false, false)).join("\n");
        assert!(early.contains(BANNER_UNICODE[0].trim_end()));
        for index in 0..40 {
            state.ingest_log_line(&format!("[config] later line {index}"));
        }
        let late = plain(&state.render_frame(80, 24, false, false)).join("\n");
        for line in BANNER_UNICODE {
            assert!(!late.contains(line.trim_end()));
        }
    }

    #[test]
    fn banner_hidden_under_alerts_and_no_banner_option() {
        let state = demo_state();
        let events = plain(&state.render_frame(120, 36, false, false)).join("\n");
        assert!(events.contains(BANNER_UNICODE[0].trim_end()));

        let mut filtered = state;
        filtered.log_filter = LogFilterMode::Alerts;
        let alerts = plain(&filtered.render_frame(120, 36, false, false)).join("\n");
        for line in BANNER_UNICODE {
            assert!(!alerts.contains(line.trim_end()));
        }
        assert!(alerts.contains("[security]"));
        assert!(!alerts.contains("[live-mods]"));

        // --no-banner -> with_options never injects it.
        let (opts, _) = ConsoleModeOptions::from_args(&["--no-banner".to_string()]);
        let quiet = ServerTuiState::with_options(&opts);
        assert!(quiet.log.iter().all(|e| e.category != LogCategory::Banner));
        let loud = ServerTuiState::with_options(&ConsoleModeOptions::default());
        assert_eq!(loud.log.len(), BANNER_UNICODE.len());
    }

    // ------------------------------------------------------------- frames

    #[test]
    fn standard_sizes_never_overflow() {
        for (cols, rows) in [(80, 24), (92, 22), (96, 26), (100, 30), (120, 36), (140, 40)]
        {
            let lines = frame(&demo_state(), cols, rows);
            assert_eq!(lines.len(), rows, "{cols}x{rows}");
            for line in &lines {
                assert_eq!(vlen(line), cols, "{cols}x{rows}: {line}");
            }
        }
    }

    #[test]
    fn tiny_frames_do_not_crash() {
        for (cols, rows) in [(40, 12), (20, 6), (12, 4), (8, 4)] {
            let lines = frame(&demo_state(), cols, rows);
            assert_eq!(lines.len(), rows);
            for line in &lines {
                assert!(vlen(line) <= cols, "{cols}x{rows}: {line}");
            }
        }
    }

    #[test]
    fn ascii_frame_is_pure_ascii() {
        for (cols, rows) in [(80, 24), (120, 36)] {
            let lines = demo_state().render_frame(cols, rows, false, true);
            for line in &lines {
                assert!(line.is_ascii(), "{line}");
                assert!(!line.contains('\u{1b}'), "{line}");
            }
        }
    }

    #[test]
    fn colored_frame_keeps_widths() {
        let lines = demo_state().render_frame(100, 30, true, false);
        assert!(lines.join("\n").contains('\u{1b}'));
        for line in &lines {
            assert_eq!(vlen(line), 100);
        }
    }

    #[test]
    fn pinned_header_is_one_unified_topbar() {
        for (cols, rows) in [(80, 24), (104, 24), (120, 36)] {
            let plain = plain(&frame(&demo_state(), cols, rows));
            assert!(plain[0].contains("MEGU"), "{}", plain[0]);
            assert!(plain[0].contains("ONLINE"), "{}", plain[0]);
            assert!(plain[0].contains("TPS"), "{}", plain[0]);
            assert!(plain[0].contains("RAM"), "{}", plain[0]);
            assert!(plain[0].contains("GB"), "{}", plain[0]);
            assert!(plain[0].contains("CPU"), "{}", plain[0]);
            assert!(plain[0].contains("Players 2/20"), "{}", plain[0]);
            // The logo is scrolling content, never pinned chrome.
            for logo_line in BANNER_UNICODE.iter().chain(BANNER_ASCII.iter()) {
                assert!(!plain[0].contains(logo_line.trim_end()), "{}", plain[0]);
            }
            // Row 1 is immediately the Log & Chat section divider — maximum room for logs.
            assert!(plain[1].contains("Log & Chat"), "{}", plain[1]);
        }
    }

    #[test]
    fn wide_topbar_includes_mspt_chunks_and_mods() {
        let header = strip_ansi(&frame(&demo_state(), 120, 36)[0]);
        assert!(header.contains("MEGU"), "{header}");
        assert!(header.contains("ONLINE"), "{header}");
        assert!(header.contains("TPS 19.9"), "{header}");
        assert!(header.contains("11.5ms"), "{header}");
        assert!(header.contains("RAM 0.51 GB"), "{header}");
        assert!(header.contains("CPU 38%"), "{header}");
        assert!(header.contains("Players 2/20"), "{header}");
        assert!(header.contains("Chunks 1,284"), "{header}");
        assert!(header.contains("Mods 3"), "{header}");
        assert!(header.contains("127.0.0.1:25565"), "{header}");
    }

    #[test]
    fn always_ready_prompt_is_visible_on_penultimate_line() {
        let lines = frame(&demo_state(), 100, 24);
        let prompt = strip_ansi(&lines[lines.len() - 2]);
        assert!(prompt.contains("> Type a command"), "{prompt}");
    }

    #[test]
    fn idle_server_shows_zero_players_and_idle_in_players_view() {
        let mut state = parse(&["Players: 0/20"]);
        let top = strip_ansi(&state.render_frame(100, 24, false, false)[0]);
        assert!(top.contains("Players 0/20"), "{top}");
        state.view = TuiView::Players;
        let text = plain(&state.render_frame(100, 24, false, false)).join("\n");
        assert!(text.contains("No players online (server idle)"), "{text}");
    }

    #[test]
    fn hotspot_alert_is_highlighted() {
        let mut state = demo_state();
        state.ingest_log_line("[guard] chunk hotspot (12,-4) throttled after 41 ms");
        let text = plain(&state.render_frame(120, 30, false, false)).join("\n");
        assert!(text.contains("Hotspot (12,-4)"), "{text}");
        assert!(text.contains("ALERT"), "{text}");
        let ascii = state.render_frame(120, 30, false, true).join("\n");
        assert!(ascii.contains("! Hotspot (12,-4)"), "{ascii}");
    }

    #[test]
    fn footer_shows_filter_counts_and_non_interfering_shortcuts() {
        let state = parse(REALISTIC_LOG);
        let lines = state.render_frame(120, 36, false, false);
        let footer = strip_ansi(lines.last().unwrap());
        assert!(footer.contains("F2"), "{footer}");
        assert!(footer.contains("Filter:events"), "{footer}");
        assert!(footer.contains("F3"), "{footer}");
        assert!(footer.contains("Players"), "{footer}");
        assert!(footer.contains("F4"), "{footer}");
        assert!(footer.contains("Mods"), "{footer}");
        assert!(footer.contains("Ctrl+C"), "{footer}");
        assert!(footer.contains("1 warn"), "{footer}");
        assert!(footer.contains("1 err"), "{footer}");
    }

    #[test]
    fn footer_tracks_active_filter() {
        let mut state = demo_state();
        state.log_filter = LogFilterMode::Alerts;
        let lines = state.render_frame(120, 36, false, false);
        let footer = strip_ansi(lines.last().unwrap());
        assert!(footer.contains("Filter:alerts"), "{footer}");
        assert!(plain(&lines).join("\n").contains("Alerts"));
    }

    #[test]
    fn players_view_lists_names_and_addresses() {
        let mut state = demo_state();
        state.view = TuiView::Players;
        let lines = state.render_frame(120, 30, false, false);
        let text = plain(&lines).join("\n");
        assert!(text.contains("Players (2/20)"), "{text}");
        assert!(text.contains("Status ONLINE"), "{text}");
        assert!(text.contains("uptime 500 ticks"), "{text}");
        assert!(text.contains("Bob"), "{text}");
        assert!(text.contains("127.0.0.1:51130"), "{text}");
        let footer = strip_ansi(lines.last().unwrap());
        assert!(footer.contains("Log"), "{footer}");
    }

    #[test]
    fn players_view_when_empty() {
        let mut state = parse(&["Players: 0/20"]);
        state.view = TuiView::Players;
        let text = plain(&state.render_frame(100, 24, false, false)).join("\n");
        assert!(text.contains("No players online"), "{text}");
    }

    #[test]
    fn mods_view_is_human_readable() {
        let mut state = demo_state();
        state.view = TuiView::Mods;
        let lines = state.render_frame(120, 30, false, false);
        let text = plain(&lines).join("\n");
        assert!(text.contains("weirdmod.jar"), "{text}");
        assert!(text.contains("compatibility shim"), "{text}");
        assert!(text.contains("runtime adapter"), "{text}");
        assert!(!text.to_lowercase().contains("tier2"), "{text}");
        assert!(!text.to_lowercase().contains("tier3"), "{text}");
        assert!(text.contains("3 active"), "{text}");
        let footer = strip_ansi(lines.last().unwrap());
        assert!(footer.contains("Log"), "{footer}");
    }

    #[test]
    fn empty_snapshot_does_not_crash() {
        let state = ServerTuiState::new();
        let lines = state.render_frame(80, 24, false, false);
        assert_eq!(lines.len(), 24);
        for line in &lines {
            assert_eq!(vlen(line), 80);
        }
        assert!(strip_ansi(&lines[0]).contains("BOOTING"));
        assert!(strip_ansi(&lines[0]).contains("-- GB"));
    }

    // -------------------------------------------------------- interactivity

    #[test]
    fn f2_cycles_log_filter() {
        let mut state = ServerTuiState::new();
        for expected in [
            LogFilterMode::Alerts,
            LogFilterMode::All,
            LogFilterMode::Events,
        ] {
            assert_eq!(state.handle_key(TuiKey::F2), TuiAction::None);
            assert_eq!(state.log_filter(), expected);
        }
    }

    #[test]
    fn f3_and_f4_toggle_views_without_stealing_letters() {
        let mut state = ServerTuiState::new();
        // Plain letters p/m/f/q always go straight to the command prompt!
        state.feed_input(b"pmfq");
        assert_eq!(state.cmd_buffer(), "pmfq");
        assert_eq!(state.view(), TuiView::Log);
        assert!(!state.quit_requested());

        state.feed_input(b"\x1bOR"); // F3
        assert_eq!(state.view(), TuiView::Players);
        state.feed_input(b"\x10"); // Ctrl+P
        assert_eq!(state.view(), TuiView::Log);
        state.feed_input(b"\x1bOS"); // F4
        assert_eq!(state.view(), TuiView::Mods);
        state.feed_input(b"\x07"); // Ctrl+G
        assert_eq!(state.view(), TuiView::Log);
    }

    #[test]
    fn ctrl_c_requests_stop() {
        let mut state = ServerTuiState::new();
        assert_eq!(state.handle_key(TuiKey::CtrlC), TuiAction::RequestStop);
        assert!(state.quit_requested());
    }

    #[test]
    fn command_prompt_is_always_ready_and_strips_optional_slash() {
        let mut state = ServerTuiState::new();
        assert!(state.cmd_mode());
        // Without leading slash
        let actions = state.feed_input(b"list\r");
        assert_eq!(actions, vec![TuiAction::SubmitCommand("list".to_string())]);
        assert_eq!(state.command_history(), vec!["list"]);
        // With leading slash
        let actions = state.feed_input(b"/say hello\r");
        assert_eq!(
            actions,
            vec![TuiAction::SubmitCommand("say hello".to_string())]
        );
        assert_eq!(state.command_history(), vec!["list", "say hello"]);
        // Empty input does nothing
        assert!(state.feed_input(b"\r").is_empty());
        assert!(state.feed_input(b"/\r").is_empty());
        assert_eq!(state.command_history(), vec!["list", "say hello"]);
    }

    #[test]
    fn up_and_down_arrow_history() {
        let mut state = ServerTuiState::new();
        assert_eq!(
            state.feed_input(b"first\r"),
            vec![TuiAction::SubmitCommand("first".to_string())]
        );
        assert_eq!(
            state.feed_input(b"second\r"),
            vec![TuiAction::SubmitCommand("second".to_string())]
        );
        state.feed_input(b"\x1b[A");
        assert_eq!(state.cmd_buffer(), "second");
        state.feed_input(b"\x1b[A");
        assert_eq!(state.cmd_buffer(), "first");
        state.feed_input(b"\x1b[A");
        assert_eq!(state.cmd_buffer(), "first");
        state.feed_input(b"\x1b[B");
        assert_eq!(state.cmd_buffer(), "second");
        state.feed_input(b"\x1b[B");
        assert_eq!(state.cmd_buffer(), "");
    }

    #[test]
    fn cursor_editing_tab_completion_and_scroll() {
        let mut state = ServerTuiState::new();
        state.feed_input(b"sy");
        state.feed_input(b"\x1b[D"); // Left
        state.feed_input(b"a");
        assert_eq!(state.cmd_buffer(), "say");
        assert_eq!(state.cmd_cursor(), 2);
        state.feed_input(b"\x1b[H"); // Home
        assert_eq!(state.cmd_cursor(), 0);
        state.feed_input(b"\x1b[F"); // End
        assert_eq!(state.cmd_cursor(), 3);
        state.feed_input(b"\x15"); // Ctrl+U clears line
        assert_eq!(state.cmd_buffer(), "");

        // Tab completes unambiguous prefix
        state.feed_input(b"sav\t");
        assert_eq!(state.cmd_buffer(), "save-all");

        // Escape clears buffer first, then returns to Log view
        state.feed_input(b"\x1b");
        assert_eq!(state.cmd_buffer(), "");
        state.handle_key(TuiKey::F3);
        assert_eq!(state.view(), TuiView::Players);
        state.feed_input(b"\x1b");
        assert_eq!(state.view(), TuiView::Log);

        // PageUp / PageDown scroll log
        for idx in 0..20 {
            state.ingest_log_line(&format!("event line {idx}"));
        }
        state.feed_input(b"\x1b[5~");
        assert_eq!(state.log_scroll(), 5);
        state.feed_input(b"\x1b[6~");
        assert_eq!(state.log_scroll(), 0);
    }

    #[test]
    fn history_is_bounded_at_64() {
        let mut state = ServerTuiState::new();
        for index in 0..100 {
            let cmd = format!("cmd{index}\r");
            let actions = state.feed_input(cmd.as_bytes());
            assert!(actions
                .iter()
                .any(|a| matches!(a, TuiAction::SubmitCommand(_))));
        }
        let history = state.command_history();
        assert_eq!(history.len(), 64);
        assert_eq!(history.last(), Some(&"cmd99"));
        assert_eq!(history.first(), Some(&"cmd36"));
    }

    #[test]
    fn escape_and_backspace_in_command_prompt() {
        let mut state = ServerTuiState::new();
        state.feed_input(b"list");
        state.feed_input(b"\x7f");
        assert_eq!(state.cmd_buffer(), "lis");
        state.feed_input(b"\x1b");
        assert_eq!(state.cmd_buffer(), "");
        state.feed_input(b"q");
        assert_eq!(state.cmd_buffer(), "q");
        assert!(!state.quit_requested());
    }

    #[test]
    fn prompt_shows_typed_command_on_penultimate_line() {
        let mut state = ServerTuiState::new();
        state.feed_input(b"gi");
        let lines = state.render_frame(80, 24, false, false);
        let prompt = strip_ansi(&lines[lines.len() - 2]);
        assert!(prompt.contains("> gi"), "{prompt}");
    }

    #[test]
    fn update_live_metrics_refreshes_without_polluting_log() {
        let mut state = ServerTuiState::new();
        let before_lines = state.log.len();
        state.update_live_metrics(&LiveTuiMetrics {
            tps: Some(20.0),
            mspt_ms: Some(9.4),
            mem_mib: Some(512.0),
            cpu_pct: Some(16.0),
            threads: Some(8),
            players_online: 1,
            players_max: 20,
            player_names: vec!["Alice".to_string()],
            loaded_chunks: 400,
            active_mods: 2,
            sleeping: false,
            top_hotspot: None,
        });
        assert_eq!(state.log.len(), before_lines);
        assert_eq!(state.players_online(), 1);
        assert_eq!(state.loaded_chunks(), Some(400));
        let top = strip_ansi(&state.render_frame(120, 24, false, false)[0]);
        assert!(top.contains("RAM 0.50 GB"), "{top}");
        assert!(top.contains("CPU 16%"), "{top}");
        assert!(top.contains("Players 1/20"), "{top}");
        assert!(top.contains("Chunks 400"), "{top}");
    }

    // ------------------------------------------------------------- helpers

    #[test]
    fn sparkline_semantics() {
        // A flat series sits on the middle rung.
        let mut ring = VecDeque::from(vec![5.0; 6]);
        let line = sparkline(&ring, 6, None, None, false);
        assert!(line.chars().all(|c| c == SPARK_BLOCKS[4]), "{line}");
        // A ramp is monotonic.
        ring = (0..10).map(|v| v as f64).collect();
        let line = sparkline(&ring, 10, None, None, false);
        let ranks: Vec<usize> = line
            .chars()
            .map(|c| SPARK_BLOCKS.iter().position(|b| *b == c).unwrap())
            .collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{line}");
        // Fixed scale + left padding for a short series.
        ring = VecDeque::from(vec![0.0, 7.0]);
        let line = sparkline(&ring, 6, Some(0.0), Some(10.0), false);
        assert_eq!(line.chars().count(), 6);
        assert_eq!(&line[..4], "    ");
        // ASCII charset and clamping to both ends.
        ring = VecDeque::from(vec![-100.0, 0.0, 100.0]);
        let line = sparkline(&ring, 3, Some(0.0), Some(10.0), true);
        assert_eq!(line, "..@");
    }

    #[test]
    fn clip_and_vlen_handle_ansi() {
        let colored = paint("hello", "red", true);
        assert_eq!(vlen(&colored), 5);
        let clipped = clip(&colored, 8);
        assert_eq!(vlen(&clipped), 8);
        // Colors cannot bleed past a truncated line.
        let clipped = clip(&colored, 3);
        assert_eq!(vlen(&clipped), 3);
        assert_eq!(strip_ansi(&clipped), "hel");
        assert!(clipped.ends_with(RESET));
        assert_eq!(fit_plain("ab", 4, '-'), "ab--");
        assert_eq!(fit_plain("abcd", 3, '-'), "abc");
    }

    #[test]
    fn ring_buffer_bound_and_categories_stay_aligned() {
        let mut state = ServerTuiState::new();
        for index in 0..450 {
            state.ingest_log_line(&format!("line {index}"));
        }
        assert_eq!(state.log.len(), 400);
        assert_eq!(state.log.back().unwrap().line, "line 449");
        assert_eq!(state.lines_seen(), 450);
    }

    #[test]
    fn proc_metrics_feed_cpu_and_ram() {
        let mut state = ServerTuiState::new();
        state.note_proc_metrics(Some(512.0), Some(14), Some(37.5));
        state.note_proc_metrics(None, None, None);
        state.note_memory(513.5);
        assert!((state.mem_current().unwrap() - 513.5).abs() < 0.001);
        assert_eq!(state.threads, Some(14));
        assert!((state.cpu_current().unwrap() - 37.5).abs() < 0.001);
        state.note_proc_metrics(Some(f64::NAN), None, Some(-1.0));
        assert_eq!(state.mem_mib.len(), 2);
        assert_eq!(state.cpu_pct.len(), 1);
    }
}
