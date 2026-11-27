//! Java big-endian NBT, preserving UTF-16 strings and every value type.
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NbtString(pub Vec<u16>);
impl NbtString {
    pub(crate) fn text(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }
    pub(crate) fn to_utf8(&self) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tag {
    End,
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(u32),
    Double(u64),
    ByteArray(Vec<u8>),
    String(NbtString),
    List(u8, Vec<Tag>),
    Compound(BTreeMap<NbtString, Tag>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}
impl Tag {
    pub(crate) fn id(&self) -> u8 {
        match self {
            Self::End => 0,
            Self::Byte(_) => 1,
            Self::Short(_) => 2,
            Self::Int(_) => 3,
            Self::Long(_) => 4,
            Self::Float(_) => 5,
            Self::Double(_) => 6,
            Self::ByteArray(_) => 7,
            Self::String(_) => 8,
            Self::List(..) => 9,
            Self::Compound(_) => 10,
            Self::IntArray(_) => 11,
            Self::LongArray(_) => 12,
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&Tag> {
        if let Self::Compound(values) = self {
            values.get(&NbtString::text(key))
        } else {
            None
        }
    }
    pub(crate) fn list(&self) -> Option<&[Tag]> {
        if let Self::List(_, values) = self {
            Some(values)
        } else {
            None
        }
    }
    pub(crate) fn int(&self) -> Option<i32> {
        if let Self::Int(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub(crate) fn string(&self) -> Option<&NbtString> {
        if let Self::String(v) = self {
            Some(v)
        } else {
            None
        }
    }
}

const MAX_NBT_TAGS: usize = 250_000;

struct Reader<'a> {
    input: &'a [u8],
    remaining_tags: usize,
}
impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let (v, rest) = self.input.split_at_checked(n).ok_or("truncated NBT")?;
        self.input = rest;
        Ok(v)
    }
    fn number<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let bytes = self.bytes(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }
    fn length(&mut self, element_size: usize) -> Result<usize, &'static str> {
        let n = i32::from_be_bytes(self.number()?);
        if n < 0 || n as usize > self.input.len() / element_size {
            return Err("invalid NBT collection length");
        }
        Ok(n as usize)
    }
    fn string(&mut self) -> Result<NbtString, &'static str> {
        let len = u16::from_be_bytes(self.number()?) as usize;
        let bytes = self.bytes(len)?;
        let mut units = Vec::new();
        let mut i = 0;
        // DataInput.readUTF accepts single zero and overlong 2/3-byte forms;
        // rejects four-byte UTF-8, and preserves individual surrogate units.
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            let unit = match b >> 4 {
                0..=7 => u16::from(b),
                12 | 13 => {
                    let c = *bytes.get(i).ok_or("truncated modified UTF-8")?;
                    i += 1;
                    if c & 0xc0 != 0x80 {
                        return Err("invalid modified UTF-8 continuation");
                    }
                    (u16::from(b & 31) << 6) | u16::from(c & 63)
                }
                14 => {
                    let c = *bytes.get(i).ok_or("truncated modified UTF-8")?;
                    let d = *bytes.get(i + 1).ok_or("truncated modified UTF-8")?;
                    i += 2;
                    if c & 0xc0 != 0x80 || d & 0xc0 != 0x80 {
                        return Err("invalid modified UTF-8 continuation");
                    }
                    (u16::from(b & 15) << 12) | (u16::from(c & 63) << 6) | u16::from(d & 63)
                }
                _ => return Err("invalid modified UTF-8 leading byte"),
            };
            units.push(unit);
        }
        Ok(NbtString(units))
    }
    fn tag(&mut self, id: u8, depth: usize) -> Result<Tag, &'static str> {
        if self.remaining_tags == 0 {
            return Err("NBT exceeds tag count budget");
        }
        self.remaining_tags -= 1;
        Ok(match id {
            0 => Tag::End,
            1 => Tag::Byte(i8::from_be_bytes(self.number()?)),
            2 => Tag::Short(i16::from_be_bytes(self.number()?)),
            3 => Tag::Int(i32::from_be_bytes(self.number()?)),
            4 => Tag::Long(i64::from_be_bytes(self.number()?)),
            5 => Tag::Float(u32::from_be_bytes(self.number()?)),
            6 => Tag::Double(u64::from_be_bytes(self.number()?)),
            7 => {
                let n = self.length(1)?;
                Tag::ByteArray(self.bytes(n)?.to_vec())
            }
            8 => Tag::String(self.string()?),
            9 => {
                if depth >= 512 {
                    return Err("NBT exceeds container depth");
                }
                let kind = self.number::<1>()?[0];
                let n = self.length(1)?;
                if kind > 12 || (kind == 0 && n != 0) {
                    return Err("invalid NBT list element type");
                }
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(self.tag(kind, depth + 1)?);
                }
                Tag::List(kind, values)
            }
            10 => {
                if depth >= 512 {
                    return Err("NBT exceeds container depth");
                }
                let mut values = BTreeMap::new();
                loop {
                    let kind = self.number::<1>()?[0];
                    if kind == 0 {
                        break;
                    }
                    if kind > 12 {
                        return Err("unknown NBT compound tag");
                    }
                    let key = self.string()?;
                    let value = self.tag(kind, depth + 1)?;
                    values.insert(key, value);
                }
                Tag::Compound(values)
            }
            11 => {
                let n = self.length(4)?;
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(i32::from_be_bytes(self.number()?));
                }
                Tag::IntArray(values)
            }
            12 => {
                let n = self.length(8)?;
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(i64::from_be_bytes(self.number()?));
                }
                Tag::LongArray(values)
            }
            _ => return Err("unknown NBT tag"),
        })
    }
}

pub(crate) fn read_named(input: &[u8]) -> Result<(NbtString, Tag), &'static str> {
    let mut reader = Reader {
        input,
        remaining_tags: MAX_NBT_TAGS,
    };
    let id = reader.number::<1>()?[0];
    let name = if id == 0 {
        NbtString(Vec::new())
    } else {
        reader.string()?
    };
    let value = reader.tag(id, 0)?;
    if !reader.input.is_empty() {
        return Err("trailing bytes after named NBT");
    }
    Ok((name, value))
}

pub(crate) fn read_gzip(
    input: &[u8],
    max_uncompressed: usize,
) -> Result<(NbtString, Tag), &'static str> {
    use std::io::Read;
    let limit = max_uncompressed
        .checked_add(1)
        .ok_or("NBT gzip size limit overflow")?;
    let mut decoded = Vec::new();
    flate2::read::MultiGzDecoder::new(input)
        .take(limit as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| "invalid gzip NBT")?;
    if decoded.len() > max_uncompressed {
        return Err("NBT gzip exceeds caller size budget");
    }
    read_named(&decoded)
}

fn write_string(value: &NbtString, out: &mut Vec<u8>) -> Result<(), &'static str> {
    let mut bytes = Vec::new();
    for &unit in &value.0 {
        if (1..=127).contains(&unit) {
            bytes.push(unit as u8);
        } else if unit <= 0x7ff {
            bytes.push(0xc0 | (unit >> 6) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        } else {
            bytes.push(0xe0 | (unit >> 12) as u8);
            bytes.push(0x80 | ((unit >> 6) & 63) as u8);
            bytes.push(0x80 | (unit & 63) as u8);
        }
    }
    let len =
        u16::try_from(bytes.len()).map_err(|_| "modified UTF-8 string exceeds 65535 bytes")?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend(bytes);
    Ok(())
}
fn write_length(n: usize, out: &mut Vec<u8>) -> Result<(), &'static str> {
    out.extend_from_slice(
        &i32::try_from(n)
            .map_err(|_| "NBT collection too large")?
            .to_be_bytes(),
    );
    Ok(())
}
fn write_payload(tag: &Tag, out: &mut Vec<u8>, depth: usize) -> Result<(), &'static str> {
    match tag {
        Tag::End => {}
        Tag::Byte(v) => out.push(*v as u8),
        Tag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
        Tag::Float(v) => {
            let bits = if f32::from_bits(*v).is_nan() {
                0x7fc00000
            } else {
                *v
            };
            out.extend_from_slice(&bits.to_be_bytes());
        }
        Tag::Double(v) => {
            let bits = if f64::from_bits(*v).is_nan() {
                0x7ff8000000000000
            } else {
                *v
            };
            out.extend_from_slice(&bits.to_be_bytes());
        }
        Tag::ByteArray(v) => {
            write_length(v.len(), out)?;
            out.extend_from_slice(v);
        }
        Tag::String(v) => write_string(v, out)?,
        Tag::List(kind, values) => {
            if depth >= 512 {
                return Err("NBT exceeds container depth");
            }
            if *kind > 12 || values.iter().any(|v| v.id() != *kind || *kind == 0) {
                return Err("invalid NBT list elements");
            }
            out.push(if values.is_empty() { 0 } else { *kind });
            write_length(values.len(), out)?;
            for value in values {
                write_payload(value, out, depth + 1)?;
            }
        }
        Tag::Compound(values) => {
            if depth >= 512 {
                return Err("NBT exceeds container depth");
            }
            for (key, value) in values {
                if value.id() == 0 {
                    return Err("NBT compound cannot contain end values");
                }
                out.push(value.id());
                write_string(key, out)?;
                write_payload(value, out, depth + 1)?;
            }
            out.push(0);
        }
        Tag::IntArray(v) => {
            write_length(v.len(), out)?;
            for n in v {
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
        Tag::LongArray(v) => {
            write_length(v.len(), out)?;
            for n in v {
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
    }
    Ok(())
}

/// Canonical named NBT: Java UTF-16 key order, canonical NaNs and empty lists.
/// Compound order is semantic, not a promise to preserve source byte order.
pub(crate) fn write_named(name: &NbtString, tag: &Tag) -> Result<Vec<u8>, &'static str> {
    let mut out = vec![tag.id()];
    if tag.id() != 0 {
        write_string(name, &mut out)?;
    }
    write_payload(tag, &mut out, 0)?;
    Ok(out)
}

/// Read one Protocol 767 registry compound, leaving subsequent packet bytes unread.
pub(crate) fn read_network_nbt(input: &mut &[u8]) -> Result<Tag, &'static str> {
    let Some((&10, rest)) = input.split_first() else {
        return Err("network registry NBT root must be a compound");
    };
    let mut reader = Reader {
        input: rest,
        remaining_tags: MAX_NBT_TAGS,
    };
    let tag = reader.tag(10, 0)?;
    *input = reader.input;
    Ok(tag)
}

/// Protocol 767 omits the root name and its length. Child tags use normal Java NBT.
pub(crate) fn write_network_nbt(tag: &Tag) -> Result<Vec<u8>, &'static str> {
    if !matches!(tag, Tag::Compound(_)) {
        return Err("network registry NBT root must be a compound");
    }
    let mut out = vec![10];
    write_payload(tag, &mut out, 0)?;
    Ok(out)
}

/// Parse a complete datapack JSON object into NBT. The shared asset parser
/// collapses booleans into numbers and parses all numbers through f64.
pub(crate) fn registry_json_to_nbt(bytes: &[u8]) -> Result<Tag, &'static str> {
    let input = std::str::from_utf8(bytes).map_err(|_| "registry JSON must be UTF-8")?;
    let mut json = RegistryJson { input };
    json.whitespace();
    if !json.take('{') {
        return Err("registry JSON root must be an object");
    }
    let tag = json.compound(1)?;
    json.whitespace();
    if !json.input.is_empty() {
        return Err("trailing bytes after registry JSON");
    }
    Ok(tag)
}

struct RegistryJson<'a> {
    input: &'a str,
}

impl RegistryJson<'_> {
    fn whitespace(&mut self) {
        self.input = self.input.trim_start_matches([' ', '\t', '\r', '\n']);
    }

    fn take(&mut self, ch: char) -> bool {
        if let Some(rest) = self.input.strip_prefix(ch) {
            self.input = rest;
            true
        } else {
            false
        }
    }

    fn next_char(&mut self) -> Result<char, &'static str> {
        let ch = self.input.chars().next().ok_or("truncated registry JSON")?;
        self.input = &self.input[ch.len_utf8()..];
        Ok(ch)
    }

    fn string(&mut self) -> Result<NbtString, &'static str> {
        if !self.take('"') {
            return Err("expected registry JSON string");
        }
        let mut units = Vec::new();
        loop {
            match self.next_char()? {
                '"' => return Ok(NbtString(units)),
                '\\' => units.push(match self.next_char()? {
                    '"' => 0x22,
                    '\\' => 0x5c,
                    '/' => 0x2f,
                    'b' => 0x08,
                    'f' => 0x0c,
                    'n' => 0x0a,
                    'r' => 0x0d,
                    't' => 0x09,
                    'u' => {
                        let (digits, rest) = self
                            .input
                            .split_at_checked(4)
                            .filter(|(digits, _)| digits.bytes().all(|b| b.is_ascii_hexdigit()))
                            .ok_or("invalid registry JSON Unicode escape")?;
                        self.input = rest;
                        u16::from_str_radix(digits, 16)
                            .map_err(|_| "invalid registry JSON Unicode escape")?
                    }
                    _ => return Err("invalid registry JSON escape"),
                }),
                '\0'..='\u{1f}' => return Err("unescaped control character in registry JSON"),
                ch => units.extend_from_slice(ch.encode_utf16(&mut [0; 2])),
            }
        }
    }

    fn digits(&mut self) -> Result<(), &'static str> {
        let rest = self
            .input
            .trim_start_matches(|ch: char| ch.is_ascii_digit());
        if rest.len() == self.input.len() {
            return Err("expected registry JSON number digits");
        }
        self.input = rest;
        Ok(())
    }

    fn number(&mut self) -> Result<Tag, &'static str> {
        let start = self.input;
        self.take('-');
        if !self.take('0') {
            self.digits()?;
        }
        let fractional = self.take('.');
        if fractional {
            self.digits()?;
        }
        let exponent = self.take('e') || self.take('E');
        if exponent {
            if !self.take('+') {
                self.take('-');
            }
            self.digits()?;
        }
        let number = &start[..start.len() - self.input.len()];
        if fractional || exponent {
            let value: f64 = number.parse().map_err(|_| "invalid registry JSON number")?;
            if !value.is_finite() {
                return Err("registry JSON number exceeds NBT double range");
            }
            Ok(Tag::Double(value.to_bits()))
        } else {
            let value: i64 = number
                .parse()
                .map_err(|_| "registry JSON integer exceeds NBT long range")?;
            Ok(i32::try_from(value).map_or(Tag::Long(value), Tag::Int))
        }
    }

    fn more(&mut self, end: char) -> Result<bool, &'static str> {
        self.whitespace();
        if self.take(end) {
            Ok(false)
        } else if self.take(',') {
            self.whitespace();
            Ok(true)
        } else {
            Err("expected comma or closing delimiter in registry JSON")
        }
    }

    fn compound(&mut self, depth: usize) -> Result<Tag, &'static str> {
        let mut values = BTreeMap::new();
        self.whitespace();
        if !self.take('}') {
            loop {
                let key = self.string()?;
                self.whitespace();
                if !self.take(':') {
                    return Err("expected colon after registry JSON object key");
                }
                let value = self.value(depth)?;
                if values.insert(key, value).is_some() {
                    return Err("duplicate registry JSON object key");
                }
                if !self.more('}')? {
                    break;
                }
            }
        }
        Ok(Tag::Compound(values))
    }

    fn list(&mut self, depth: usize) -> Result<Tag, &'static str> {
        let mut values = Vec::new();
        self.whitespace();
        if !self.take(']') {
            loop {
                values.push(self.value(depth)?);
                if !self.more(']')? {
                    break;
                }
            }
        }
        let mut kind = values.first().map_or(0, Tag::id);
        if values.iter().any(|value| value.id() != kind) {
            kind = 10;
        }
        if kind == 10 {
            // NbtOps reads a heterogeneous list as compounds, using a singleton
            // empty key to wrap non-compounds or escape a compound with that shape.
            let wrapper = NbtString(Vec::new());
            for value in &mut values {
                if matches!(value, Tag::Compound(fields) if fields.len() != 1 || !fields.contains_key(&wrapper))
                {
                    continue;
                }
                let inner = std::mem::replace(value, Tag::End);
                *value = Tag::Compound(BTreeMap::from([(wrapper.clone(), inner)]));
            }
        }
        Ok(Tag::List(kind, values))
    }

    fn value(&mut self, depth: usize) -> Result<Tag, &'static str> {
        self.whitespace();
        if depth >= 512 {
            return Err("registry JSON exceeds NBT container depth");
        }
        match self.input.as_bytes().first() {
            Some(b'{') => {
                self.input = &self.input[1..];
                self.compound(depth + 1)
            }
            Some(b'[') => {
                self.input = &self.input[1..];
                self.list(depth + 1)
            }
            Some(b'"') => self.string().map(Tag::String),
            Some(b't') if self.input.starts_with("true") => {
                self.input = &self.input[4..];
                Ok(Tag::Byte(1))
            }
            Some(b'f') if self.input.starts_with("false") => {
                self.input = &self.input[5..];
                Ok(Tag::Byte(0))
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b'n') if self.input.starts_with("null") => Err("NBT has no JSON null value"),
            _ => Err("invalid registry JSON value"),
        }
    }
}

#[cfg(test)]
#[cfg(megu_vanilla_data)]
mod tests {
    use super::*;
    fn bytes(hex: &str) -> Vec<u8> {
        let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty(), "odd native hexadecimal length");
        pairs
            .iter()
            .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16).unwrap())
            .collect()
    }
    #[test]
    fn typed_values_utf16_and_malformed_input_match_native_nbtio() {
        let mut cases = 0;
        for row in include_str!("../tests/data/nbt_codec_1211.tsv").lines() {
            let (input, expected) = row.split_once('\t').unwrap();
            let decoded = read_named(&bytes(input));
            if expected == "REJECT" {
                assert!(decoded.is_err(), "native rejected {input}");
            } else {
                let (name, tag) =
                    decoded.unwrap_or_else(|e| panic!("native accepted {input}: {e}"));
                assert_eq!(
                    write_named(&name, &tag).unwrap(),
                    bytes(expected),
                    "native normalized {input}"
                );
            }
            cases += 1;
        }
        assert_eq!(cases, 41);
    }
}
