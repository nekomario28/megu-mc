//! Shared asset/datapack value parser, independent of terrain generation.
//! Booleans use numeric values, preserving false separately from null/missing.

#[derive(Debug, Clone)]
pub(crate) enum J {
    Null,
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub(crate) fn num(&self) -> f64 {
        match self {
            J::Num(n) => *n,
            _ => 0.0,
        }
    }
    pub(crate) fn str(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => "",
        }
    }
    pub(crate) fn as_arr(&self) -> Option<&Vec<J>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub(crate) fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(m) => m.iter().find(|(kk, _)| kk == k).map(|(_, v)| v),
            _ => None,
        }
    }
}

const MAX_JSON_DEPTH: usize = 128;

pub(crate) struct JP<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}
impl<'a> JP<'a> {
    pub(crate) fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    pub(crate) fn val(&mut self) -> Option<J> {
        self.ws();
        match *self.b.get(self.i)? {
            b'{' => {
                if self.depth >= MAX_JSON_DEPTH {
                    return None;
                }
                self.depth += 1;
                self.i += 1;
                let mut m = vec![];
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        self.depth -= 1;
                        return Some(J::Obj(m));
                    }
                    let k = match self.val() {
                        Some(J::Str(s)) => s,
                        _ => {
                            self.depth -= 1;
                            return None;
                        }
                    };
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        self.depth -= 1;
                        return None;
                    }
                    self.i += 1;
                    let v = match self.val() {
                        Some(v) => v,
                        None => {
                            self.depth -= 1;
                            return None;
                        }
                    };
                    m.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(&b',') => self.i += 1,
                        Some(&b'}') => {
                            self.i += 1;
                            self.depth -= 1;
                            return Some(J::Obj(m));
                        }
                        _ => {
                            self.depth -= 1;
                            return None;
                        }
                    }
                }
            }
            b'[' => {
                if self.depth >= MAX_JSON_DEPTH {
                    return None;
                }
                self.depth += 1;
                self.i += 1;
                let mut a = vec![];
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        self.depth -= 1;
                        return Some(J::Arr(a));
                    }
                    let v = match self.val() {
                        Some(v) => v,
                        None => {
                            self.depth -= 1;
                            return None;
                        }
                    };
                    a.push(v);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(&b',') => self.i += 1,
                        Some(&b']') => {
                            self.i += 1;
                            self.depth -= 1;
                            return Some(J::Arr(a));
                        }
                        _ => {
                            self.depth -= 1;
                            return None;
                        }
                    }
                }
            }
            b'"' => {
                self.i += 1;
                let mut s = String::new();
                while self.i < self.b.len() {
                    let c = self.b[self.i];
                    self.i += 1;
                    if c == b'"' {
                        return Some(J::Str(s));
                    }
                    if c == b'\\' {
                        let e = *self.b.get(self.i)?;
                        self.i += 1;
                        s.push(match e {
                            b'n' => '\n',
                            b't' => '\t',
                            b'r' => '\r',
                            b'u' => {
                                let h = self.b.get(self.i..self.i + 4)?;
                                self.i += 4;
                                let u1 =
                                    u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()?;
                                if (0xD800..=0xDBFF).contains(&u1)
                                    && self.b.get(self.i..self.i + 2) == Some(b"\\u")
                                {
                                    if let Some(h2) = self.b.get(self.i + 2..self.i + 6) {
                                        if let Some(u2) = std::str::from_utf8(h2)
                                            .ok()
                                            .and_then(|t| u32::from_str_radix(t, 16).ok())
                                        {
                                            if (0xDC00..=0xDFFF).contains(&u2) {
                                                self.i += 6;
                                                let cp = 0x10000
                                                    + ((u1 - 0xD800) << 10)
                                                    + (u2 - 0xDC00);
                                                s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                                continue;
                                            }
                                        }
                                    }
                                }
                                char::from_u32(u1).unwrap_or('?')
                            }
                            x => x as char,
                        });
                    } else if c < 0x80 {
                        s.push(c as char);
                    } else {
                        let start = self.i - 1;
                        let tail = &self.b[start..];
                        let ch = match std::str::from_utf8(tail) {
                            Ok(text) => text.chars().next()?,
                            Err(err) => {
                                let valid = err.valid_up_to();
                                if valid == 0 {
                                    return None;
                                }
                                std::str::from_utf8(&tail[..valid]).ok()?.chars().next()?
                            }
                        };
                        self.i = start + ch.len_utf8();
                        s.push(ch);
                    }
                }
                None
            }
            b't' => {
                if self.b.get(self.i..self.i + 4) != Some(b"true") {
                    return None;
                }
                self.i += 4;
                Some(J::Num(1.0))
            }
            b'f' => {
                if self.b.get(self.i..self.i + 5) != Some(b"false") {
                    return None;
                }
                self.i += 5;
                Some(J::Num(0.0))
            }
            b'n' => {
                if self.b.get(self.i..self.i + 4) != Some(b"null") {
                    return None;
                }
                self.i += 4;
                Some(J::Null)
            }
            _ => {
                let s0 = self.i;
                while self.i < self.b.len()
                    && matches!(
                        self.b[self.i],
                        b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'
                    )
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.b[s0..self.i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(J::Num)
            }
        }
    }
}
pub(crate) fn jparse(b: &[u8]) -> Option<J> {
    JP { b, i: 0, depth: 0 }.val()
}

// Typed field accessors shared by the registry, providers and feature parsing.
// They move the existing lookup/defaulting contract; they do not change it.
pub(crate) fn js<'a>(j: &'a J, k: &str) -> Option<&'a J> {
    j.get(k)
}
pub(crate) fn jstr(j: &J, k: &str) -> Option<String> {
    js(j, k).map(|v| v.str().to_string())
}
pub(crate) fn jnum(j: &J, k: &str) -> Option<f64> {
    js(j, k).map(|v| v.num())
}
pub(crate) fn jint(j: &J, k: &str, d: i32) -> i32 {
    jnum(j, k).unwrap_or(d as f64) as i32
}
pub(crate) fn jf32(j: &J, k: &str, d: f32) -> f32 {
    jnum(j, k).unwrap_or(d as f64) as f32
}
pub(crate) fn jbool(j: &J, k: &str, d: bool) -> bool {
    match js(j, k) {
        Some(J::Num(n)) => *n != 0.0,
        Some(J::Str(s)) => s == "true",
        _ => d,
    }
}
pub(crate) fn jarr<'a>(j: &'a J, k: &str) -> Option<&'a Vec<J>> {
    js(j, k).and_then(|v| v.as_arr())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_false_overrides_a_true_default() {
        let value = jparse(br#"{"aquifers_enabled":false,"enabled":true,"unset":null}"#).unwrap();
        assert!(!jbool(&value, "aquifers_enabled", true));
        assert!(jbool(&value, "enabled", false));
        assert!(jbool(&value, "missing", true));
        assert!(jbool(&value, "unset", true));
        assert!(matches!(value.get("unset"), Some(J::Null)));
        assert!(!matches!(value.get("aquifers_enabled"), Some(J::Null)));
    }

    #[test]
    fn deeply_nested_json_fails_closed_without_stack_overflow() {
        let deep_arr = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(jparse(deep_arr.as_bytes()).is_none());
        let deep_obj = format!("{}{}", "{\"a\":".repeat(10_000), "}".repeat(10_000));
        assert!(jparse(deep_obj.as_bytes()).is_none());
    }

    #[test]
    fn utf16_surrogate_pair_and_utf8_japanese_decode_cleanly() {
        let value = jparse("{\"icon\":\"\\uD83D\\uDE00\",\"ja\":\"日本語テスト\"}".as_bytes()).unwrap();
        assert_eq!(jstr(&value, "icon").as_deref(), Some("\u{1F600}"));
        assert_eq!(jstr(&value, "ja").as_deref(), Some("日本語テスト"));
        assert!(jparse(b"tr").is_none());
        assert!(jparse(b"nul").is_none());
    }
}
