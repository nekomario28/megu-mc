//! Shared asset/datapack value parser, independent of terrain generation.
//! This moves the existing parsing contract; it does not redefine JSON acceptance.

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

pub(crate) struct JP<'a> {
    b: &'a [u8],
    i: usize,
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
                self.i += 1;
                let mut m = vec![];
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Some(J::Obj(m));
                    }
                    let k = match self.val()? {
                        J::Str(s) => s,
                        _ => return None,
                    };
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return None;
                    }
                    self.i += 1;
                    let v = self.val()?;
                    m.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(&b',') => self.i += 1,
                        Some(&b'}') => {
                            self.i += 1;
                            return Some(J::Obj(m));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut a = vec![];
                loop {
                    self.ws();
                    if self.b.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Some(J::Arr(a));
                    }
                    a.push(self.val()?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(&b',') => self.i += 1,
                        Some(&b']') => {
                            self.i += 1;
                            return Some(J::Arr(a));
                        }
                        _ => return None,
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
                                char::from_u32(
                                    u32::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()?,
                                )
                                .unwrap_or('?')
                            }
                            x => x as char,
                        });
                    } else {
                        s.push(c as char);
                    }
                }
                None
            }
            b't' => {
                self.i += 4;
                Some(J::Num(1.0))
            }
            b'f' => {
                self.i += 5;
                Some(J::Null)
            }
            b'n' => {
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
    JP { b, i: 0 }.val()
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
