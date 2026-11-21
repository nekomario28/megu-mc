//! `server.properties`: a Java `.properties` file kept losslessly, plus the
//! typed [`ServerProperties`] view with Vanilla's defaults.
//!
//! [`PropertiesFile`] follows `java.util.Properties.load` for what the files
//! actually contain: `#`/`!` comment lines, `=`/`:` (and bare whitespace)
//! separators, `\uXXXX`/`\n`/`\t`/`\\` escapes, and backslash-newline
//! continuation. What it keeps that `Properties` does not is the document
//! itself — comments, blanks, entry order and the original text of every
//! untouched entry — so a parse -> store round trip is byte-identical except
//! where the caller actually changed a value.
//!
//! A malformed `\uXXXX` escape is an error (Java's `IllegalArgumentException`),
//! never a silently repaired value.

use crate::atomic::write_atomic;
use std::fmt;
use std::io;
use std::path::Path;

/// The file's Vanilla name; [`ServerProperties::load_from`] and
/// [`ServerProperties::save_to`] take any path, but this is what a dedicated
/// server reads in its working directory.
pub const PROPERTIES_FILE: &str = "server.properties";

/// A `\uXXXX` escape without four hex digits (Java's `Malformed \uxxxx
/// encoding.`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MalformedEscape {
    /// Byte offset inside the logical line being unescaped.
    pub offset: usize,
}

impl fmt::Display for MalformedEscape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Malformed \\uxxxx encoding (offset {})", self.offset)
    }
}
impl std::error::Error for MalformedEscape {}

/// One physical-or-logical line of the document.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Line {
    /// A comment or blank line, emitted back exactly as read.
    Verbatim(String),
    /// A `key`/`value` pair. `raw` is the original logical line; `set`
    /// rewrites it, so untouched entries write back byte-identical.
    Entry {
        raw: String,
        key: String,
        value: String,
    },
}

/// A parsed `.properties` document: comment/blank/entry lines in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertiesFile {
    lines: Vec<Line>,
}

impl PropertiesFile {
    /// Parse text already decoded to UTF-8 (see [`Self::parse_bytes`] for the
    /// byte-level entry point with the ISO-8859-1 fallback).
    pub fn parse(text: &str) -> Result<Self, MalformedEscape> {
        let mut file = PropertiesFile::default();
        let physical = physical_lines(text);
        let mut i = 0;
        while i < physical.len() {
            let line = physical[i];
            i += 1;
            let trimmed = trim_leading_ws(line);
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                file.lines.push(Line::Verbatim(line.to_owned()));
                continue;
            }
            // Join continuation lines: an odd run of trailing backslashes eats
            // one and pulls in the next line (whose leading whitespace Java
            // skips). `raw` keeps the untouched spelling when there is one
            // physical line; a joined entry emits its logical form.
            let mut logical = trimmed.to_owned();
            let mut joined = false;
            while trailing_backslash_odd(&logical) {
                logical.pop();
                joined = true;
                if i < physical.len() {
                    logical.push_str(trim_leading_ws(physical[i]));
                    i += 1;
                } else {
                    break;
                }
            }
            let raw = if joined {
                logical.clone()
            } else {
                line.to_owned()
            };
            let (key, value) = split_entry(&logical)?;
            file.lines.push(Line::Entry { raw, key, value });
        }
        Ok(file)
    }

    /// Read `bytes` the way Vanilla's `Settings.loadFromFile` does: UTF-8
    /// first, ISO-8859-1 for the whole file when it is not.
    pub fn parse_bytes(bytes: &[u8]) -> Result<Self, MalformedEscape> {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::parse(text),
            Err(_) => Self::parse(&bytes.iter().map(|&b| char::from(b)).collect::<String>()),
        }
    }

    /// Load `path`; a missing file is an empty document, not an error.
    pub fn load_from(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Self::parse_bytes(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Serialize the document. Untouched lines are emitted verbatim; edited
    /// and appended entries are written `escaped_key=escaped_value`. Always
    /// ends with a newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match line {
                Line::Verbatim(raw) => out.push_str(raw),
                Line::Entry { raw, .. } => out.push_str(raw),
            }
            out.push('\n');
        }
        out
    }

    /// Atomically write [`Self::render`] to `path`.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        write_atomic(path, self.render().as_bytes())
    }

    /// Value for `key`, if the document has one. On duplicates the last
    /// occurrence wins, as Java's `Hashtable.put` resolves them.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().rev().find_map(|line| match line {
            Line::Entry { key: k, value, .. } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Set `key` in place (keeping its position) or append it; `true` when the
    /// stored value actually changed. Empty keys are refused: they can only be
    /// produced by a malformed file, and rewriting one would corrupt the line.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        if key.is_empty() {
            return false;
        }
        // Update the entry `get` resolves to (the last of any duplicates), so
        // a set always takes effect on the value callers read back.
        for line in self.lines.iter_mut().rev() {
            if let Line::Entry {
                raw,
                key: k,
                value: v,
            } = line
            {
                if k == key {
                    if v == value {
                        return false;
                    }
                    *v = value.to_owned();
                    *raw = format!("{}={}", escape_component(key, true), escape_component(value, false));
                    return true;
                }
            }
        }
        self.lines.push(Line::Entry {
            raw: format!("{}={}", escape_component(key, true), escape_component(value, false)),
            key: key.to_owned(),
            value: value.to_owned(),
        });
        true
    }

    /// Drop every entry named `key`; `true` when any went away.
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.lines.len();
        self.lines.retain(|line| !matches!(line, Line::Entry { key: k, .. } if k == key));
        before != self.lines.len()
    }

    /// Keys in document order (duplicates included, as the file has them).
    pub fn keys(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Entry { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// `\r\n`, `\n` and bare `\r` all terminate a physical line.
fn physical_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                lines.push(&text[start..i]);
                i += 1;
                if bytes.get(i) == Some(&b'\n') {
                    i += 1;
                }
                start = i;
            }
            b'\n' => {
                lines.push(&text[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn trim_leading_ws(s: &str) -> &str {
    s.trim_start_matches(|c| c == ' ' || c == '\t' || c == '\u{0c}')
}

/// `true` when `s` ends in an odd run of backslashes: the line continues.
fn trailing_backslash_odd(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Split a logical line into its unescaped key and value, Java-style: the key
/// runs to the first unescaped `=`/`:` or whitespace, an optional separator
/// follows, and the remainder is the value (trailing whitespace kept, as Java
/// keeps it).
fn split_entry(line: &str) -> Result<(String, String), MalformedEscape> {
    let chars: Vec<char> = line.chars().collect();
    let mut key_len = 0;
    let mut value_start = chars.len();
    let mut has_sep = false;
    let mut backslash = false;
    while key_len < chars.len() {
        let c = chars[key_len];
        if (c == '=' || c == ':') && !backslash {
            value_start = key_len + 1;
            has_sep = true;
            break;
        } else if (c == ' ' || c == '\t' || c == '\u{0c}') && !backslash {
            value_start = key_len + 1;
            break;
        }
        backslash = c == '\\' && !backslash;
        key_len += 1;
    }
    while value_start < chars.len() {
        let c = chars[value_start];
        if c != ' ' && c != '\t' && c != '\u{0c}' {
            if !has_sep && (c == '=' || c == ':') {
                has_sep = true;
            } else {
                break;
            }
        }
        value_start += 1;
    }
    let key: String = chars[..key_len].iter().collect();
    let value: String = chars[value_start..].iter().collect();
    Ok((unescape(&key)?, unescape(&value)?))
}

/// `loadConvert`: `\t` `\r` `\n` `\f` and `\uXXXX` produce the named
/// character; any other escaped character (including `\=`, `\:` and `\\`)
/// stands for itself. `\u` escapes decode as UTF-16 units so a surrogate pair
/// combines; a lone one reads as U+FFFD.
fn unescape(input: &str) -> Result<String, MalformedEscape> {
    fn flush(units: &mut Vec<u16>, out: &mut String) {
        if !units.is_empty() {
            out.extend(char::decode_utf16(units.drain(..)).map(|r| r.unwrap_or('\u{fffd}')));
        }
    }
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut units: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' {
            flush(&mut units, &mut out);
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        match chars.get(i) {
            None => break, // a trailing lone backslash is dropped
            Some(&'u') => {
                if i + 4 >= chars.len() {
                    return Err(MalformedEscape { offset: i - 1 });
                }
                let mut unit: u16 = 0;
                for k in 1..=4 {
                    let d = chars[i + k]
                        .to_digit(16)
                        .ok_or(MalformedEscape { offset: i + k })?;
                    unit = unit * 16 + d as u16;
                }
                i += 5;
                units.push(unit);
                // A pending high surrogate waits for the next `\u`; anything
                // else completes the run (a lone low one reads as U+FFFD).
                if !(0xD800..0xDC00).contains(&unit) {
                    flush(&mut units, &mut out);
                }
            }
            Some(&c) => {
                i += 1;
                flush(&mut units, &mut out);
                out.push(match c {
                    't' => '\t',
                    'r' => '\r',
                    'n' => '\n',
                    'f' => '\u{0c}',
                    other => other,
                });
            }
        }
    }
    flush(&mut units, &mut out);
    Ok(out)
}

/// `saveConvert` for one key or value: `\`, the separators, `#`/`!`, control
/// characters and everything non-ASCII are escaped (`\uXXXX`, so the file
/// stays ASCII regardless of platform encoding). `escape_space` is Java's
/// key-side rule (every space); values escape only a leading space.
fn escape_component(s: &str, escape_space: bool) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for (index, unit) in s.encode_utf16().enumerate() {
        match unit {
            u if u == u16::from(b'\\') => out.push_str("\\\\"),
            0x20 => {
                if index == 0 || escape_space {
                    out.push('\\');
                }
                out.push(' ');
            }
            0x09 => out.push_str("\\t"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x0c => out.push_str("\\f"),
            u if matches!(u, 0x3d | 0x3a | 0x23 | 0x21) => {
                out.push('\\');
                out.push(char::from(u as u8));
            }
            u if !(0x20..=0x7e).contains(&u) => out.push_str(&format!("\\u{u:04x}")),
            u => out.push(char::from_u32(u32::from(u)).unwrap_or('\u{fffd}')),
        }
    }
    out
}

// -------------------------------------------------------- typed server view

fn prop_bool(doc: &PropertiesFile, key: &str, default: bool) -> bool {
    doc.get(key)
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(default)
}

fn prop_str(doc: &PropertiesFile, key: &str, default: &str) -> String {
    doc.get(key)
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
        .to_owned()
}

fn prop_u16(doc: &PropertiesFile, key: &str, default: u16) -> u16 {
    doc.get(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|n| n.clamp(0, u16::MAX as i64) as u16)
        .unwrap_or(default)
}

fn prop_i64(doc: &PropertiesFile, key: &str, default: i64) -> i64 {
    doc.get(key)
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

/// Typed `server.properties`: every accessor returns the file's value or the
/// Vanilla default, and the matching setter writes it back into the document.
/// Unknown keys are untouched — [`Self::doc`] exposes them.
#[derive(Clone, Default)]
pub struct ServerProperties {
    doc: PropertiesFile,
}

impl ServerProperties {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parse(text: &str) -> Result<Self, MalformedEscape> {
        Ok(Self {
            doc: PropertiesFile::parse(text)?,
        })
    }

    /// Load `path` (typically `dir.join(PROPERTIES_FILE)`); a missing file is
    /// the defaults.
    pub fn load_from(path: &Path) -> io::Result<Self> {
        Ok(Self {
            doc: PropertiesFile::load_from(path)?,
        })
    }

    /// Atomically write the document to `path`.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        self.doc.save_to(path)
    }

    /// The underlying document (unknown keys included).
    pub fn doc(&self) -> &PropertiesFile {
        &self.doc
    }
    pub fn doc_mut(&mut self) -> &mut PropertiesFile {
        &mut self.doc
    }

    pub fn server_port(&self) -> u16 {
        prop_u16(&self.doc, "server-port", 25565)
    }
    pub fn set_server_port(&mut self, port: u16) {
        self.doc.set("server-port", &port.to_string());
    }

    /// Vanilla default: `A Minecraft Server`.
    pub fn motd(&self) -> String {
        prop_str(&self.doc, "motd", "A Minecraft Server")
    }
    pub fn set_motd(&mut self, motd: &str) {
        self.doc.set("motd", motd);
    }

    pub fn max_players(&self) -> i64 {
        prop_i64(&self.doc, "max-players", 20)
    }
    pub fn set_max_players(&mut self, max_players: i64) {
        self.doc.set("max-players", &max_players.to_string());
    }

    /// Vanilla default `true`: authentication on unless the file says
    /// otherwise. [`Self::set_online_mode`] is the explicit override.
    pub fn online_mode(&self) -> bool {
        prop_bool(&self.doc, "online-mode", true)
    }
    pub fn set_online_mode(&mut self, online: bool) {
        self.doc.set("online-mode", if online { "true" } else { "false" });
    }

    pub fn white_list(&self) -> bool {
        prop_bool(&self.doc, "white-list", false)
    }
    pub fn set_white_list(&mut self, enabled: bool) {
        self.doc.set("white-list", if enabled { "true" } else { "false" });
    }

    /// `enforce-whitelist`: when `white-list` is on, kick players who get
    /// removed from it. Vanilla default `false`.
    pub fn enforce_whitelist(&self) -> bool {
        prop_bool(&self.doc, "enforce-whitelist", false)
    }
    pub fn set_enforce_whitelist(&mut self, enabled: bool) {
        self.doc
            .set("enforce-whitelist", if enabled { "true" } else { "false" });
    }

    pub fn view_distance(&self) -> i64 {
        prop_i64(&self.doc, "view-distance", 10)
    }
    pub fn set_view_distance(&mut self, distance: i64) {
        self.doc.set("view-distance", &distance.to_string());
    }

    pub fn simulation_distance(&self) -> i64 {
        prop_i64(&self.doc, "simulation-distance", 10)
    }
    pub fn set_simulation_distance(&mut self, distance: i64) {
        self.doc.set("simulation-distance", &distance.to_string());
    }

    /// `level-seed`; empty means a random seed, as Vanilla treats it.
    pub fn level_seed(&self) -> String {
        prop_str(&self.doc, "level-seed", "")
    }
    pub fn set_level_seed(&mut self, seed: &str) {
        self.doc.set("level-seed", seed);
    }

    pub fn level_name(&self) -> String {
        prop_str(&self.doc, "level-name", "world")
    }
    pub fn set_level_name(&mut self, name: &str) {
        self.doc.set("level-name", name);
    }

    /// `difficulty` as the file writes it (`peaceful`/`easy`/`normal`/`hard`
    /// or the legacy numeric); Vanilla default `easy`.
    pub fn difficulty(&self) -> String {
        prop_str(&self.doc, "difficulty", "easy")
    }
    pub fn set_difficulty(&mut self, difficulty: &str) {
        self.doc.set("difficulty", difficulty);
    }

    /// `gamemode` as the file writes it; Vanilla default `survival`.
    pub fn gamemode(&self) -> String {
        prop_str(&self.doc, "gamemode", "survival")
    }
    pub fn set_gamemode(&mut self, gamemode: &str) {
        self.doc.set("gamemode", gamemode);
    }

    pub fn pvp(&self) -> bool {
        prop_bool(&self.doc, "pvp", true)
    }
    pub fn set_pvp(&mut self, pvp: bool) {
        self.doc.set("pvp", if pvp { "true" } else { "false" });
    }

    pub fn enable_rcon(&self) -> bool {
        prop_bool(&self.doc, "enable-rcon", false)
    }
    pub fn set_enable_rcon(&mut self, enabled: bool) {
        self.doc.set("enable-rcon", if enabled { "true" } else { "false" });
    }

    pub fn rcon_port(&self) -> u16 {
        prop_u16(&self.doc, "rcon.port", 25575)
    }
    pub fn set_rcon_port(&mut self, port: u16) {
        self.doc.set("rcon.port", &port.to_string());
    }

    /// `rcon.password`, empty when unset — never log it; [`fmt::Debug`] for
    /// this struct redacts it as `[REDACTED]`.
    pub fn rcon_password(&self) -> String {
        self.doc.get("rcon.password").unwrap_or("").to_owned()
    }
    pub fn set_rcon_password(&mut self, password: &str) {
        self.doc.set("rcon.password", password);
    }

    /// `network-compression-threshold`; Vanilla default `256`.
    pub fn network_compression_threshold(&self) -> i64 {
        prop_i64(&self.doc, "network-compression-threshold", 256)
    }
    pub fn set_network_compression_threshold(&mut self, threshold: i64) {
        self.doc
            .set("network-compression-threshold", &threshold.to_string());
    }
}

/// Every field except `rcon.password`, which always prints `[REDACTED]` — a
/// `Debug` dump in a log must never carry the live password.
impl fmt::Debug for ServerProperties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerProperties")
            .field("server_port", &self.server_port())
            .field("motd", &self.motd())
            .field("max_players", &self.max_players())
            .field("online_mode", &self.online_mode())
            .field("white_list", &self.white_list())
            .field("enforce_whitelist", &self.enforce_whitelist())
            .field("view_distance", &self.view_distance())
            .field("simulation_distance", &self.simulation_distance())
            .field("level_seed", &self.level_seed())
            .field("level_name", &self.level_name())
            .field("difficulty", &self.difficulty())
            .field("gamemode", &self.gamemode())
            .field("pvp", &self.pvp())
            .field("enable_rcon", &self.enable_rcon())
            .field("rcon_port", &self.rcon_port())
            .field("rcon_password", &"[REDACTED]")
            .field(
                "network_compression_threshold",
                &self.network_compression_threshold(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("server-files-props-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_comments_separators_and_escapes_like_java() {
        let doc = PropertiesFile::parse(
            "# comment\n  ! other\nkey=value\na:b\nc d\n e  =  f g \nmulti=one\\\n    two\nesc=\\u00e9\\t\\x\nempty\nlast=1\nlast=2",
        )
        .unwrap();
        assert_eq!(doc.get("key"), Some("value"));
        assert_eq!(doc.get("a"), Some("b"));
        assert_eq!(doc.get("c"), Some("d"));
        assert_eq!(doc.get("e"), Some("f g ")); // trailing space kept, as Java
        assert_eq!(doc.get("multi"), Some("onetwo")); // backslash continuation
        assert_eq!(doc.get("esc"), Some("é\tx"));
        assert_eq!(doc.get("empty"), Some("")); // no separator -> empty value
        assert_eq!(doc.get("last"), Some("2")); // last occurrence wins, as Java
        assert_eq!(doc.keys(), ["key", "a", "c", "e", "multi", "esc", "empty", "last", "last"]);
    }

    #[test]
    fn roundtrip_is_byte_identical_until_edited() {
        let original = "#Minecraft server properties\n#Thu Nov 21 00:00:00 UTC 2026\n\
                        server-port=25565\nmotd=A\\: line\\ntwo\nmodded-key=keep me\n\n!tail\n";
        let mut doc = PropertiesFile::parse(original).unwrap();
        assert_eq!(doc.render(), original, "untouched parse must re-emit verbatim");

        doc.set("server-port", "25570");
        doc.set("new-key", "a b");
        let rendered = doc.render();
        assert!(rendered.contains("server-port=25570"), "{rendered}");
        assert!(rendered.contains("new-key=a b"), "{rendered}");
        // Comments, blanks and the unknown key survived, in order; the new key
        // appended at the end.
        assert!(rendered.contains("#Minecraft server properties"), "{rendered}");
        assert!(rendered.contains("modded-key=keep me"), "{rendered}");
        assert!(rendered.contains("!tail\n"), "{rendered}");
        assert!(rendered.find("!tail") < rendered.find("new-key"), "{rendered}");

        let reparsed = PropertiesFile::parse(&rendered).unwrap();
        assert_eq!(reparsed.get("server-port"), Some("25570"));
        assert_eq!(reparsed.get("motd"), Some("A: line\ntwo"));
        assert_eq!(reparsed.get("new-key"), Some("a b"));
        assert_eq!(reparsed.get("modded-key"), Some("keep me"));
    }

    #[test]
    fn escapes_and_malformed_unicode() {
        let mut doc = PropertiesFile::default();
        doc.set("weird key", "va=lue: #é\u{1f64f}");
        let rendered = doc.render();
        assert_eq!(rendered, "weird\\ key=va\\=lue\\: \\#\\u00e9\\ud83d\\ude4f\n");
        let back = PropertiesFile::parse(&rendered).unwrap();
        assert_eq!(back.get("weird key"), Some("va=lue: #é\u{1f64f}"));

        for bad in ["k=\\u12", "k=\\uzzzz", "k=\\u"] {
            assert!(PropertiesFile::parse(bad).is_err(), "{bad}");
        }
        // Latin-1 bytes still read (ISO-8859-1 fallback).
        let doc = PropertiesFile::parse_bytes(b"motd=caf\xe9").unwrap();
        assert_eq!(doc.get("motd"), Some("café"));
    }

    #[test]
    fn typed_accessors_use_vanilla_defaults_and_override() {
        let mut props = ServerProperties::new();
        assert_eq!(props.server_port(), 25565);
        assert_eq!(props.motd(), "A Minecraft Server");
        assert_eq!(props.max_players(), 20);
        assert!(props.online_mode()); // Vanilla default is authenticated
        assert!(!props.white_list());
        assert!(!props.enforce_whitelist());
        assert_eq!(props.view_distance(), 10);
        assert_eq!(props.simulation_distance(), 10);
        assert_eq!(props.level_seed(), "");
        assert_eq!(props.level_name(), "world");
        assert_eq!(props.difficulty(), "easy");
        assert_eq!(props.gamemode(), "survival");
        assert!(props.pvp());
        assert!(!props.enable_rcon());
        assert_eq!(props.rcon_port(), 25575);
        assert_eq!(props.rcon_password(), "");
        assert_eq!(props.network_compression_threshold(), 256);

        props.set_online_mode(false); // explicit offline-mode override
        props.set_server_port(25570);
        props.set_rcon_password("hunter2");
        props.set_white_list(true);
        assert!(!props.online_mode());
        assert_eq!(props.server_port(), 25570);
        assert_eq!(props.rcon_password(), "hunter2");
        assert!(props.white_list());
    }

    #[test]
    fn rcon_password_is_redacted_in_debug() {
        let mut props = ServerProperties::new();
        props.set_rcon_password("super-secret-123");
        let debug = format!("{props:?}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(!debug.contains("super-secret-123"), "{debug}");
        // The value is still in the document itself; only Debug hides it.
        assert_eq!(props.rcon_password(), "super-secret-123");
        assert!(props.doc().get("rcon.password").is_some());
    }

    #[test]
    fn load_save_roundtrip_through_disk() {
        let dir = tmp_dir("disk");
        let path = dir.join(PROPERTIES_FILE);
        let mut props = ServerProperties::load_from(&path).unwrap(); // missing -> defaults
        assert_eq!(props.server_port(), 25565);
        props.set_max_players(8);
        props.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("max-players=8"), "{text}");
        assert_eq!(ServerProperties::load_from(&path).unwrap().max_players(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
