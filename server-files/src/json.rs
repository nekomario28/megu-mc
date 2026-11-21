//! The smallest JSON reader/writer that carries the Vanilla admin files.
//!
//! [`parse`] fails closed: malformed input is a [`JsonError`], never a partial
//! document, and nesting is bounded at [`MAX_DEPTH`] so a hostile file cannot
//! exhaust the stack. Objects keep the file's key order (`Vec` of pairs), so a
//! parse -> serialize round trip preserves it.

use std::fmt;

/// Deepest nesting [`parse`] accepts; the Vanilla files are `[{…}]`, two deep.
pub const MAX_DEPTH: usize = 64;

/// A parsed JSON value. `Object` pairs are in document order.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Why [`parse`] rejected the input, with the byte offset it stopped at.
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

impl Json {
    /// Object member lookup; non-objects and absent keys are `None`.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }
    /// `key -> value` member of an object as `&str`; anything else is `None`.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }
    /// Object member as `bool`, or `default` when absent/not a bool.
    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(Json::as_bool).unwrap_or(default)
    }
    /// Object member as `i64` (truncated), or `default` when absent/not a number.
    pub fn get_i64(&self, key: &str, default: i64) -> i64 {
        self.get(key)
            .and_then(Json::as_f64)
            .map(|n| n as i64)
            .unwrap_or(default)
    }

    /// Compact serialization: `{"k":v}`, no spaces. Non-finite numbers write
    /// `null` (JSON cannot name them; the files here never produce one).
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => {
                if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
                    out.push_str(&format!("{}", *n as i64));
                } else if n.is_finite() {
                    out.push_str(&format!("{n}"));
                } else {
                    out.push_str("null");
                }
            }
            Json::String(s) => out.push_str(&quote(s)),
            Json::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_to(out);
                }
                out.push(']');
            }
            Json::Object(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&quote(k));
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

/// A JSON string literal for `s` including the surrounding quotes. Escapes the
/// mandatory characters plus the short forms Vanilla's Gson writes; other
/// control bytes go out as `\u00xx`. Non-ASCII stays UTF-8.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
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
    out
}

/// Parse one JSON document; trailing non-whitespace is an error.
pub fn parse(text: &str) -> Result<Json, JsonError> {
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
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

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth >= MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.ws();
        match self.byte() {
            Some(b'{') => {
                let mut members = Vec::new();
                self.ws();
                match self.byte() {
                    Some(b'}') => return Ok(Json::Object(members)),
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
                    members.push((key, self.value(depth + 1)?));
                    self.ws();
                    match self.byte() {
                        Some(b',') => continue,
                        Some(b'}') => return Ok(Json::Object(members)),
                        _ => return Err(self.err("expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                let mut items = Vec::new();
                self.ws();
                match self.byte() {
                    Some(b']') => return Ok(Json::Array(items)),
                    Some(_) => self.i -= 1,
                    None => return Err(self.err("unterminated array")),
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.byte() {
                        Some(b',') => continue,
                        Some(b']') => return Ok(Json::Array(items)),
                        _ => return Err(self.err("expected ',' or ']'")),
                    }
                }
            }
            Some(b'"') => Ok(Json::String(self.string_body()?)),
            Some(b't') => self.literal("rue", Json::Bool(true)),
            Some(b'f') => self.literal("alse", Json::Bool(false)),
            Some(b'n') => self.literal("ull", Json::Null),
            Some(_) => {
                self.i -= 1;
                self.number()
            }
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn literal(&mut self, rest: &str, value: Json) -> Result<Json, JsonError> {
        if self.b.len() - self.i >= rest.len() && &self.b[self.i..self.i + rest.len()] == rest.as_bytes()
        {
            self.i += rest.len();
            Ok(value)
        } else {
            Err(self.err("bad literal"))
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Parser<'a>| -> usize {
            let s = p.i;
            while matches!(p.b.get(p.i), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i - s
        };
        if digits(self) == 0 {
            return Err(self.err("expected a value"));
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return Err(self.err("bad number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(self.err("bad number"));
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("bad utf-8"))?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| self.err("bad number"))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        debug_assert_eq!(self.b.get(self.i), Some(&b'"'));
        self.i += 1;
        self.string_body()
    }

    /// String content after the opening quote: raw UTF-8 bytes pass through,
    /// `\uXXXX` pairs combine, and a lone surrogate is an error.
    fn string_body(&mut self) -> Result<String, JsonError> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.byte().ok_or_else(|| self.err("unterminated string"))?;
            match c {
                b'"' => {
                    return String::from_utf8(out).map_err(|_| self.err("bad utf-8"));
                }
                b'\\' => {
                    let e = self.byte().ok_or_else(|| self.err("unterminated escape"))?;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
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
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                _ => out.push(c),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_each_value_kind() {
        let doc = parse(
            r#"{"s":"hi\nAé","n":-12.5e2,"b":true,"f":false,"z":null,"a":[1,{"k":"v"}],"o":{}}"#,
        )
        .unwrap();
        assert_eq!(doc.get_str("s"), Some("hi\nAé"));
        assert_eq!(doc.get("n").and_then(Json::as_f64), Some(-1250.0));
        assert!(doc.get_bool("b", false));
        assert!(!doc.get_bool("f", true));
        assert!(matches!(doc.get("z"), Some(Json::Null)));
        assert_eq!(doc.get("a").and_then(Json::as_array).unwrap().len(), 2);
        // Round trips byte-identically (object order preserved).
        assert_eq!(parse(&doc.to_json_string()).unwrap(), doc);
        // A lone top-level scalar parses too.
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
    }

    #[test]
    fn escapes_round_trip_and_surrogates_pair() {
        let s = "a\"b\\c\u{1f64f}\u{0c}\u{08}";
        let quoted = quote(s);
        assert_eq!(parse(&quoted).unwrap(), Json::String(s.to_owned()));
        // Lone surrogates and truncated escapes are rejected, never repaired.
        for bad in [r#""\ud800""#, r#""\udc00""#, r#""\ud800x""#, r#""\u12""#, "\"\\x\""] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // Control bytes serialize as escapes a strict parser accepts.
        assert_eq!(quote("a\u{7}b"), "\"a\\u0007b\"");
    }

    #[test]
    fn malformed_input_fails_closed() {
        for bad in [
            "",
            "{",
            r#"{"a"}"#,
            r#"{"a":}"#,
            r#"{"a":1,}"#,
            "[1,]",
            "[1",
            "tru",
            "12x",
            "{\"a\":1} extra",
            "\"unter",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // Depth is bounded: 65 nested arrays fail, 64 succeed.
        let deep = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&deep).is_ok());
        let too_deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert!(parse(&too_deep).is_err());
    }
}
