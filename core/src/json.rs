// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! The canonical, fail-closed JSON reader/writer every megu crate shares.
//!
//! [`parse_json`] accepts only a complete RFC 8259 document: malformed input
//! is a [`JsonError`] naming the byte offset it stopped at, never a partial
//! value. Hostile files are bounded on three axes by [`JsonLimits`] — nesting
//! depth (no stack exhaustion), total node count, and input bytes — and
//! [`parse_json_with_limits`] can additionally reject duplicate object keys
//! so `{"bypass":false,"bypass":true}` cannot parse differently for two
//! readers.
//!
//! On the write side [`quote_json_string`] / [`write_json_string`] emit one
//! canonical escaping (RFC 8259 short escapes plus `\u00XX` for the rest of
//! the control range), and [`JsonValue::to_json_string`] /
//! [`JsonValue::to_json_pretty`] serialize whole values. Objects keep
//! document order (`Vec` of pairs), so parse → serialize preserves key order.

use std::fmt;

/// A parsed JSON value. `Object` pairs are in document order.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

/// Fail-closed bounds [`parse_json_with_limits`] enforces. All three trip a
/// [`JsonError`], never a truncated document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonLimits {
    /// Deepest `[`/`{` nesting accepted; a value *at* `max_depth` is rejected.
    pub max_depth: usize,
    /// Total [`JsonValue`] nodes (containers and scalars) a document may hold.
    pub max_nodes: usize,
    /// Largest input in bytes; longer inputs fail before parsing begins.
    pub max_bytes: usize,
}

impl Default for JsonLimits {
    /// Defaults sized for admin/config files: 64-deep nesting (matching the
    /// historical workspace bound), 200 000 nodes, 16 MiB of input.
    fn default() -> Self {
        Self {
            max_depth: 64,
            max_nodes: 200_000,
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Why parsing rejected the input, with the byte offset it stopped at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at byte {}: {}", self.offset, self.message)
    }
}
impl std::error::Error for JsonError {}

impl JsonValue {
    /// Object member lookup; non-objects and absent keys are `None`.
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number(n) => Some(*n),
            _ => None,
        }
    }
    /// The number truncated toward zero, like Java's `(long)` cast;
    /// non-numbers are `None`.
    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|n| n as i64)
    }
    /// The number truncated toward zero as `u64`; non-numbers are `None`.
    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|n| n as u64)
    }
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(a) => Some(a),
            _ => None,
        }
    }
    /// The object's `(key, value)` pairs in document order; non-objects are
    /// `None`.
    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(m) => Some(m),
            _ => None,
        }
    }
    /// `key -> value` member of an object as `&str`; anything else is `None`.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(JsonValue::as_str)
    }
    /// Object member as `bool`, or `default` when absent/not a bool.
    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(JsonValue::as_bool).unwrap_or(default)
    }
    /// Object member as `i64` (truncated), or `default` when absent/not a number.
    pub fn get_i64(&self, key: &str, default: i64) -> i64 {
        self.get(key)
            .and_then(JsonValue::as_f64)
            .map(|n| n as i64)
            .unwrap_or(default)
    }

    /// Compact serialization: `{"k":v}`, no spaces. Non-finite numbers write
    /// `null` — RFC 8259 cannot name them.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    /// Indented serialization: two-space steps, one member per line.
    pub fn to_json_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_number(out: &mut String, n: f64) {
        if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
            out.push_str(&format!("{}", n as i64));
        } else if n.is_finite() {
            out.push_str(&format!("{n}"));
        } else {
            out.push_str("null");
        }
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            JsonValue::Number(n) => Self::write_number(out, *n),
            JsonValue::String(s) => write_json_string(out, s),
            JsonValue::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_compact(out);
                }
                out.push(']');
            }
            JsonValue::Object(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(out, k);
                    out.push(':');
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            JsonValue::Number(n) => Self::write_number(out, *n),
            JsonValue::String(s) => write_json_string(out, s),
            JsonValue::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent + 1));
                    v.write_pretty(out, indent + 1);
                }
                out.push('\n');
                out.push_str(&"  ".repeat(indent));
                out.push(']');
            }
            JsonValue::Object(m) => {
                if m.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent + 1));
                    write_json_string(out, k);
                    out.push_str(": ");
                    v.write_pretty(out, indent + 1);
                }
                out.push('\n');
                out.push_str(&"  ".repeat(indent));
                out.push('}');
            }
        }
    }
}

/// Append `s` to `out` as a quoted JSON string literal. The mandatory escapes
/// plus the RFC 8259 short forms (`\b \f \n \r \t`) are used; the remaining
/// control characters go out as `\u00XX`. Non-ASCII stays UTF-8.
pub fn write_json_string(out: &mut String, s: &str) {
    out.reserve(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// [`write_json_string`] returned as an owned `String`.
pub fn quote_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_json_string(&mut out, s);
    out
}

/// Parse one JSON document under the default [`JsonLimits`]; trailing
/// non-whitespace is an error and duplicate object keys are tolerated
/// (RFC 8259 leaves their handling to the reader — use
/// [`parse_json_with_limits`] to reject them).
pub fn parse_json(input: &str) -> Result<JsonValue, JsonError> {
    parse_json_with_limits(input, JsonLimits::default(), false)
}

/// Parse one JSON document under explicit limits. `reject_duplicate_keys`
/// fails closed on a repeated object key instead of letting the last (or
/// first) writer win silently.
pub fn parse_json_with_limits(
    input: &str,
    limits: JsonLimits,
    reject_duplicate_keys: bool,
) -> Result<JsonValue, JsonError> {
    if input.len() > limits.max_bytes {
        return Err(JsonError {
            offset: limits.max_bytes,
            message: format!("input exceeds {}-byte limit", limits.max_bytes),
        });
    }
    let mut p = Parser {
        b: input.as_bytes(),
        i: 0,
        limits,
        nodes: 0,
        reject_duplicate_keys,
    };
    p.ws();
    let value = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    limits: JsonLimits,
    nodes: usize,
    reject_duplicate_keys: bool,
}

impl<'a> Parser<'a> {
    fn err(&self, message: &str) -> JsonError {
        JsonError {
            offset: self.i,
            message: message.to_owned(),
        }
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn byte(&mut self) -> Option<u8> {
        let c = *self.b.get(self.i)?;
        self.i += 1;
        Some(c)
    }

    fn value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth >= self.limits.max_depth {
            return Err(self.err("nesting too deep"));
        }
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(self.err("too many nodes"));
        }
        self.ws();
        match self.byte() {
            Some(b'{') => {
                let mut members: Vec<(String, JsonValue)> = Vec::new();
                self.ws();
                match self.byte() {
                    Some(b'}') => return Ok(JsonValue::Object(members)),
                    Some(_) => self.i -= 1,
                    None => return Err(self.err("unterminated object")),
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return Err(self.err("expected object key"));
                    }
                    let key = self.string()?;
                    self.ws();
                    if self.byte() != Some(b':') {
                        return Err(self.err("expected ':'"));
                    }
                    let member = self.value(depth + 1)?;
                    if self.reject_duplicate_keys
                        && members.iter().any(|(k, _)| *k == key)
                    {
                        return Err(self.err("duplicate object key"));
                    }
                    members.push((key, member));
                    self.ws();
                    match self.byte() {
                        Some(b',') => continue,
                        Some(b'}') => return Ok(JsonValue::Object(members)),
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                let mut items = Vec::new();
                self.ws();
                match self.byte() {
                    Some(b']') => return Ok(JsonValue::Array(items)),
                    Some(_) => self.i -= 1,
                    None => return Err(self.err("unterminated array")),
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.byte() {
                        Some(b',') => continue,
                        Some(b']') => return Ok(JsonValue::Array(items)),
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            Some(b'"') => Ok(JsonValue::String(self.string_body()?)),
            Some(b't') => self.literal("rue", JsonValue::Bool(true)),
            Some(b'f') => self.literal("alse", JsonValue::Bool(false)),
            Some(b'n') => self.literal("ull", JsonValue::Null),
            Some(_) => {
                self.i -= 1;
                self.number()
            }
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn literal(&mut self, rest: &str, value: JsonValue) -> Result<JsonValue, JsonError> {
        if self.b.len() - self.i >= rest.len() && &self.b[self.i..self.i + rest.len()] == rest.as_bytes()
        {
            self.i += rest.len();
            Ok(value)
        } else {
            Err(self.err("bad literal"))
        }
    }

    /// RFC 8259 `number`: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?` —
    /// leading zeros, a bare `-`, a trailing `.` and a bare exponent all
    /// fail closed.
    fn number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let int_start = self.i;
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        let int_len = self.i - int_start;
        if int_len == 0 {
            return Err(self.err("expected a value"));
        }
        if int_len > 1 && self.b[int_start] == b'0' {
            return Err(self.err("leading zeros are not valid JSON numbers"));
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            let frac_start = self.i;
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            if self.i == frac_start {
                return Err(self.err("bad number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            let exp_start = self.i;
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            if self.i == exp_start {
                return Err(self.err("bad number"));
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("bad utf-8"))?;
        text.parse::<f64>()
            .map(JsonValue::Number)
            .map_err(|_| self.err("bad number"))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        debug_assert_eq!(self.b.get(self.i), Some(&b'"'));
        self.i += 1;
        self.string_body()
    }

    /// String content after the opening quote: raw UTF-8 passes through, raw
    /// control bytes (`< 0x20`) are rejected per RFC 8259 §7, `\uXXXX` pairs
    /// combine, and a lone surrogate is an error.
    fn string_body(&mut self) -> Result<String, JsonError> {
        let mut out = String::new();
        loop {
            let c = self.byte().ok_or_else(|| self.err("unterminated string"))?;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.byte().ok_or_else(|| self.err("unterminated escape"))?;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&unit) {
                                // High surrogate: only a `\uDC00..\uDFFF` partner
                                // completes it.
                                if self.byte() != Some(b'\\') || self.byte() != Some(b'u') {
                                    return Err(self.err("lone surrogate"));
                                }
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(self.err("lone surrogate"));
                                }
                                0x1_0000
                                    + (((unit as u32 - 0xD800) << 10) | (low as u32 - 0xDC00))
                            } else if (0xDC00..0xE000).contains(&unit) {
                                return Err(self.err("lone surrogate"));
                            } else {
                                unit as u32
                            };
                            let ch = char::from_u32(code)
                                .ok_or_else(|| self.err("bad \\u escape"))?;
                            out.push(ch);
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                // Multi-byte UTF-8 sequences and plain ASCII all pass through;
                // only raw control bytes are forbidden inside a string.
                _ if c < 0x20 => return Err(self.err("unescaped control character in string")),
                _ => {
                    let start = self.i - 1;
                    let end = if c < 0x80 {
                        start + 1
                    } else {
                        // `&str` input guarantees valid UTF-8; find the char
                        // boundary so multibyte sequences copy as one unit.
                        let width = utf8_width(c);
                        start + width
                    };
                    if end > self.b.len() {
                        return Err(self.err("truncated utf-8 in string"));
                    }
                    self.i = end;
                    out.push_str(
                        std::str::from_utf8(&self.b[start..end])
                            .map_err(|_| self.err("bad utf-8"))?,
                    );
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, JsonError> {
        if self.b.len() - self.i < 4 {
            return Err(self.err("short \\u escape"));
        }
        let mut value: u16 = 0;
        for _ in 0..4 {
            let d = (self.b[self.i] as char)
                .to_digit(16)
                .ok_or_else(|| self.err("bad \\u escape"))?;
            value = value * 16 + d as u16;
            self.i += 1;
        }
        Ok(value)
    }
}

/// UTF-8 sequence width from the leading byte (input is `&str`, so a byte
/// `>= 0x80` here is always a valid lead).
fn utf8_width(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_each_value_kind() {
        let doc = parse_json(
            r#"{"s":"hi\nAé","n":-12.5e2,"b":true,"f":false,"z":null,"a":[1,{"k":"v"}],"o":{}}"#,
        )
        .unwrap();
        assert_eq!(doc.get_str("s"), Some("hi\nAé"));
        assert_eq!(doc.get("n").and_then(JsonValue::as_f64), Some(-1250.0));
        assert_eq!(doc.get("n").and_then(JsonValue::as_i64), Some(-1250));
        assert!(doc.get_bool("b", false));
        assert!(!doc.get_bool("f", true));
        assert!(matches!(doc.get("z"), Some(JsonValue::Null)));
        assert_eq!(doc.get("a").and_then(JsonValue::as_array).unwrap().len(), 2);
        assert_eq!(
            doc.get("o").and_then(JsonValue::as_object).unwrap().len(),
            0
        );
        // Round trips byte-identically (object order preserved).
        assert_eq!(parse_json(&doc.to_json_string()).unwrap(), doc);
        // A lone top-level scalar parses too.
        assert_eq!(parse_json("true").unwrap(), JsonValue::Bool(true));
        assert_eq!(parse_json("  \"x\" \n").unwrap().as_str(), Some("x"));
    }

    #[test]
    fn escapes_round_trip_and_surrogates_pair() {
        let s = "a\"b\\c\u{1f64f}\u{0c}\u{08}/";
        let quoted = quote_json_string(s);
        assert_eq!(parse_json(&quoted).unwrap(), JsonValue::String(s.to_owned()));
        // Surrogate pair decodes to the astral char.
        assert_eq!(
            parse_json(r#""\ud83d\ude4f""#).unwrap(),
            JsonValue::String("\u{1f64f}".to_owned())
        );
        // Lone surrogates and truncated escapes are rejected, never repaired.
        for bad in [
            r#""\ud800""#,
            r#""\udc00""#,
            r#""\ud800x""#,
            r#""\ud800\n""#,
            r#""\ud800\u0041""#,
            r#""\u12""#,
            r#""\uzzzz""#,
            "\"\\x\"",
        ] {
            assert!(parse_json(bad).is_err(), "{bad}");
        }
        // Control bytes serialize as escapes a strict parser accepts.
        assert_eq!(quote_json_string("a\u{7}b"), "\"a\\u0007b\"");
        assert_eq!(quote_json_string("\n\r\t\u{8}\u{c}"), "\"\\n\\r\\t\\b\\f\"");
    }

    #[test]
    fn every_control_char_round_trips() {
        for code in 0u32..0x20 {
            let c = char::from_u32(code).unwrap();
            let quoted = quote_json_string(&c.to_string());
            assert_eq!(
                parse_json(&quoted).unwrap(),
                JsonValue::String(c.to_string()),
                "code {code:#04x} via {quoted}"
            );
        }
        // But a raw (unescaped) control byte inside a string is rejected.
        assert!(parse_json("\"a\u{7}b\"").is_err());
        assert!(parse_json("\"line\nbreak\"").is_err());
    }

    #[test]
    fn malformed_input_fails_closed() {
        for bad in [
            "",
            "{",
            r#"{"a"}"#,
            r#"{"a":}"#,
            r#"{"a":1,}"#,
            "{,}",
            "[1,]",
            "[,1]",
            "[1",
            "tru",
            "nulL",
            "12x",
            "{\"a\":1} extra",
            "\"unter",
            "\"bad\ttab\"",
            "-",
            "+1",
            ".5",
            "1.",
            "1e",
            "1e+",
            "01",
            "-01",
            "00",
            "NaN",
            "Infinity",
            "[1;2]",
            "{a:1}",
        ] {
            assert!(parse_json(bad).is_err(), "{bad:?}");
        }
        // Valid edge numbers still parse.
        for good in ["0", "-0", "0.5", "10", "-12.5e2", "1E+9", "3.14159"] {
            assert!(parse_json(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn depth_limit_is_enforced() {
        let limits = JsonLimits::default();
        let deep = "[".repeat(limits.max_depth) + &"]".repeat(limits.max_depth);
        assert!(parse_json(&deep).is_ok());
        let too_deep =
            "[".repeat(limits.max_depth + 1) + &"]".repeat(limits.max_depth + 1);
        assert!(parse_json(&too_deep).is_err());
        // A tighter caller-supplied bound works the same way: the check runs
        // at every value, so a scalar one level under the cap is rejected.
        let shallow = JsonLimits {
            max_depth: 2,
            ..JsonLimits::default()
        };
        assert!(parse_json_with_limits("[[]]", shallow, false).is_ok());
        assert!(parse_json_with_limits("[[1]]", shallow, false).is_err());
        assert!(parse_json_with_limits("[[[]]]", shallow, false).is_err());
    }

    #[test]
    fn node_and_byte_limits_are_enforced() {
        let tight_nodes = JsonLimits {
            max_nodes: 3,
            ..JsonLimits::default()
        };
        // Array node + three elements = 4 nodes > 3.
        assert!(parse_json_with_limits("[1,2,3]", tight_nodes, false).is_err());
        assert!(parse_json_with_limits("[1,2]", tight_nodes, false).is_ok());

        let tight_bytes = JsonLimits {
            max_bytes: 8,
            ..JsonLimits::default()
        };
        assert!(parse_json_with_limits("[1,2,3]", tight_bytes, false).is_ok());
        assert!(parse_json_with_limits("[1,2,3,4]", tight_bytes, false).is_err());
    }

    #[test]
    fn duplicate_keys_rejected_only_when_asked() {
        let doc = r#"{"a":1,"b":2,"a":3}"#;
        // Tolerant default: the pair list keeps both, `get` sees the first.
        let parsed = parse_json(doc).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 3);
        assert_eq!(
            parse_json_with_limits(doc, JsonLimits::default(), true).unwrap_err().message,
            "duplicate object key"
        );
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        for bad in ["{} {}", "[1] x", "true false", "\"a\"\"b\"", "1 2", "[]\u{feff}"] {
            assert!(parse_json(bad).is_err(), "{bad:?}");
        }
        assert!(parse_json(" [1] \t\n").is_ok());
    }

    #[test]
    fn pretty_printing_indents_and_still_parses() {
        let doc = parse_json(r#"{"k":[1,{"x":true}],"e":{},"n":null}"#).unwrap();
        let pretty = doc.to_json_pretty();
        assert_eq!(
            pretty,
            "{\n  \"k\": [\n    1,\n    {\n      \"x\": true\n    }\n  ],\n  \"e\": {},\n  \"n\": null\n}"
        );
        assert_eq!(parse_json(&pretty).unwrap(), doc);
    }

    #[test]
    fn numbers_serialize_canonically() {
        assert_eq!(JsonValue::Number(3.0).to_json_string(), "3");
        assert_eq!(JsonValue::Number(-2.5).to_json_string(), "-2.5");
        assert_eq!(JsonValue::Number(f64::NAN).to_json_string(), "null");
        assert_eq!(JsonValue::Number(f64::INFINITY).to_json_string(), "null");
        // Integer-looking numbers inside the exact-integer range stay integers.
        assert_eq!(JsonValue::Number(9.0e14).to_json_string(), "900000000000000");
    }

    #[test]
    fn error_reports_the_byte_offset() {
        let err = parse_json("[1, oops]").unwrap_err();
        assert_eq!(err.offset, 4);
        assert!(format!("{err}").contains("byte 4"));
    }
}
