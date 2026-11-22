// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Pure-`std`, zero-dependency JVM `.class` bytecode Mixin scanner.
//!
//! Parses the JVMS §4 class-file format directly — magic, constant pool
//! (all standard tags, dual-slot `Long`/`Double` handling), fields,
//! methods, and the `RuntimeVisibleAnnotations` /
//! `RuntimeInvisibleAnnotations` attributes where SpongePowered Mixin
//! records its metadata — and reduces every `org.spongepowered.asm.mixin`
//! method annotation to the [`MixinSite`] records
//! [`crate::mixin_lifter::lift_mixin_sites`] consumes. No JVM is launched
//! and no external crate is used; ZIP member extraction reuses
//! [`crate::modpack`]'s fail-closed stored/deflate reader and
//! `*.mixins.json` parsing reuses [`crate::data_loader::parse_json`].
//!
//! [`scan_jar_mixins`] inspects a complete mod JAR: when one or more
//! `*.mixins.json` configs are present, only the classes they register
//! (the `mixins` + `server` arrays — plus `client` unless a server-side
//! scan asks to skip them) are parsed, matching what the Mixin runtime
//! would actually apply. With no config, every `.class` member is scanned
//! defensively. Either way the collected sites feed
//! [`lift_mixin_sites`] and the verdict lands in
//! [`JarMixinScanReport::verdict`], ready for
//! [`crate::fallback::classify_mod_with_mixins`].
//!
//! Everything is fail closed: bad magic, truncation, out-of-range or
//! wrong-tag constant-pool indices, malformed modified UTF-8, excessive
//! annotation nesting, corrupt archives, unreadable mixin configs, and
//! config-registered classes missing from the archive are all hard errors
//! — never a panic, never a partial scan silently treated as clean.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use crate::data_loader::{parse_json, DataLoadError, JsonValue};
use crate::mixin_lifter::{
    lift_mixin_sites, InjectPoint, MixinAnnotationKind, MixinLiftVerdict, MixinSite,
};
use crate::modpack::{self, ModpackError};

/// Deepest `@`-annotation/`[`-array nesting accepted inside an
/// `Runtime*Annotations` attribute — far past anything `javac` emits,
/// small enough that a hostile element-value graph cannot blow the stack.
const MAX_ANNOTATION_DEPTH: usize = 16;

/// Most `.class` members parsed in one no-config archive scan.
const MAX_CLASS_MEMBERS_SCANNED: usize = 65_536;

/// Total decompressed member bytes one JAR scan may read. Each member is
/// already capped at 16 MiB by the shared archive reader; this bounds the
/// cumulative work on member-heavy archives.
const MAX_JAR_SCAN_BYTES: u64 = 256 * 1024 * 1024;

/// Why a `.class` parse or JAR scan failed. Every variant keeps the mod
/// at Tier 3 — the scanner never degrades to a partial result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClassScanError {
    /// `magic != 0xCAFEBABE` — not a JVM class file.
    BadMagic,
    /// Fewer bytes than the JVMS §4 structure being read requires.
    Truncated(&'static str),
    /// A constant-pool index was 0 or past `constant_pool_count - 1`.
    BadConstantPoolIndex(&'static str),
    /// A constant-pool slot held a tag other than the one the JVMS §4.7.16
    /// consumer requires (e.g. a `CONSTANT_Integer` where a
    /// `CONSTANT_Utf8` belongs).
    BadConstantPoolTag(&'static str),
    /// Constant-pool tag byte outside the standard `1..=20` set.
    UnsupportedConstantPoolTag(u8),
    /// A `CONSTANT_Utf8` payload is not well-formed modified UTF-8.
    InvalidUtf8,
    /// Nested `@`/`[` element values deeper than [`MAX_ANNOTATION_DEPTH`].
    AnnotationNestingTooDeep,
    /// Structurally invalid in some other way; the string names where.
    Malformed(&'static str),
    /// The archive could not be listed or a member could not be decoded.
    Zip(String),
    /// A `*.mixins.json` member was not valid JSON or had the wrong shape.
    Json(String),
    /// File I/O failure inside [`scan_jar_path`].
    Io(String),
    /// A `*.mixins.json` config registers a class absent from the archive.
    MissingMixinClass(String),
}

impl fmt::Display for ClassScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => f.write_str("bad magic (not a 0xCAFEBABE class file)"),
            Self::Truncated(what) => write!(f, "truncated class file reading {what}"),
            Self::BadConstantPoolIndex(ctx) => {
                write!(f, "constant-pool index out of range for {ctx}")
            }
            Self::BadConstantPoolTag(ctx) => {
                write!(f, "constant-pool slot has the wrong tag for {ctx}")
            }
            Self::UnsupportedConstantPoolTag(tag) => {
                write!(f, "unsupported constant-pool tag {tag}")
            }
            Self::InvalidUtf8 => f.write_str("invalid modified UTF-8 in CONSTANT_Utf8"),
            Self::AnnotationNestingTooDeep => {
                f.write_str("annotation element nesting too deep")
            }
            Self::Malformed(what) => write!(f, "malformed input: {what}"),
            Self::Zip(msg) | Self::Json(msg) | Self::Io(msg) => f.write_str(msg),
            Self::MissingMixinClass(name) => {
                write!(f, "mixin config registers missing class {name}")
            }
        }
    }
}

impl std::error::Error for ClassScanError {}

impl From<ModpackError> for ClassScanError {
    fn from(e: ModpackError) -> Self {
        Self::Zip(e.reason().to_string())
    }
}

impl From<DataLoadError> for ClassScanError {
    fn from(e: DataLoadError) -> Self {
        match e {
            DataLoadError::Io(m) | DataLoadError::Json(m) | DataLoadError::Zip(m) => Self::Json(m),
        }
    }
}

/// Which mixin classes a JAR scan parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JarScanOptions {
    /// `true` (the default — mc-spine is a dedicated server): classes a
    /// config lists only under `client` never apply server-side and are
    /// skipped entirely. `false` scans them too.
    pub server_only: bool,
}

impl Default for JarScanOptions {
    fn default() -> Self {
        Self { server_only: true }
    }
}

/// The complete result of one mod-JAR Mixin scan.
#[derive(Clone, Debug)]
pub struct JarMixinScanReport {
    /// `*.mixins.json` configs found and parsed in the archive.
    pub mixin_configs_found: usize,
    /// `.class` members parsed (config-registered classes, or every class
    /// member when no config exists).
    pub classes_scanned: usize,
    /// `client`-array entries skipped by a server-side scan.
    pub skipped_client_mixins: usize,
    /// Every [`MixinSite`] extracted, in member order.
    pub sites: Vec<MixinSite>,
    /// [`lift_mixin_sites`] verdict over `sites`.
    pub verdict: MixinLiftVerdict,
}

// ---------------------------------------------------------------------------
// Byte cursor
// ---------------------------------------------------------------------------

/// Big-endian bounds-checked cursor over one byte slice.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], ClassScanError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(ClassScanError::Malformed("length overflow"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(ClassScanError::Truncated(what))?;
        self.pos = end;
        Ok(slice)
    }

    fn skip(&mut self, n: usize, what: &'static str) -> Result<(), ClassScanError> {
        self.take(n, what).map(|_| ())
    }

    fn u1(&mut self, what: &'static str) -> Result<u8, ClassScanError> {
        Ok(self.take(1, what)?[0])
    }

    fn u2(&mut self, what: &'static str) -> Result<u16, ClassScanError> {
        let b = self.take(2, what)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u4(&mut self, what: &'static str) -> Result<u32, ClassScanError> {
        let b = self.take(4, what)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

// ---------------------------------------------------------------------------
// Constant pool
// ---------------------------------------------------------------------------

/// The constant-pool slots this scanner resolves. Everything else parses
/// structurally but holds no data.
enum CpEntry {
    /// Index 0 and the second slot of `Long`/`Double` — never addressable.
    Vacant,
    Utf8(String),
    /// `CONSTANT_Integer`: backs `Z`/`I`/... annotation element values.
    Integer(i32),
    /// `CONSTANT_Class`: `name_index` of a `CONSTANT_Utf8`.
    Class { name_index: u16 },
    /// `CONSTANT_String`: `string_index` of a `CONSTANT_Utf8`.
    String { utf8_index: u16 },
    /// Tags `4`, `5`, `6`, `9..=12`, `15..=20` — structurally skipped.
    Other,
}

struct ConstantPool(Vec<CpEntry>);

impl ConstantPool {
    fn entry(&self, index: u16, ctx: &'static str) -> Result<&CpEntry, ClassScanError> {
        self.0
            .get(usize::from(index))
            .ok_or(ClassScanError::BadConstantPoolIndex(ctx))
    }

    /// A `CONSTANT_Utf8` slot — the JVMS target of `s` element values,
    /// annotation/element names, attribute names, and class names.
    fn utf8(&self, index: u16, ctx: &'static str) -> Result<&str, ClassScanError> {
        match self.entry(index, ctx)? {
            CpEntry::Utf8(s) => Ok(s),
            _ => Err(ClassScanError::BadConstantPoolTag(ctx)),
        }
    }

    /// An `s` element value: nominally `CONSTANT_Utf8` per JVMS §4.7.16.1;
    /// a `CONSTANT_String` slot is tolerated and resolved through it.
    fn string(&self, index: u16, ctx: &'static str) -> Result<&str, ClassScanError> {
        match self.entry(index, ctx)? {
            CpEntry::Utf8(s) => Ok(s),
            CpEntry::String { utf8_index } => self.utf8(*utf8_index, ctx),
            _ => Err(ClassScanError::BadConstantPoolTag(ctx)),
        }
    }

    /// A `CONSTANT_Class` slot resolved to its `CONSTANT_Utf8` name.
    fn class_name(&self, index: u16, ctx: &'static str) -> Result<&str, ClassScanError> {
        match self.entry(index, ctx)? {
            CpEntry::Class { name_index } => self.utf8(*name_index, ctx),
            _ => Err(ClassScanError::BadConstantPoolTag(ctx)),
        }
    }

    /// A `CONSTANT_Integer` slot — the storage behind `Z` element values.
    fn int(&self, index: u16, ctx: &'static str) -> Result<i32, ClassScanError> {
        match self.entry(index, ctx)? {
            CpEntry::Integer(v) => Ok(*v),
            _ => Err(ClassScanError::BadConstantPoolTag(ctx)),
        }
    }
}

/// Decode one `CONSTANT_Utf8` payload: modified UTF-8 — `0xC0 0x80` is
/// NUL, supplementary characters arrive as paired UTF-16 surrogates, and
/// a bare `0x00` byte or broken continuation is a hard error.
fn mutf8_decode(bytes: &[u8]) -> Result<String, ClassScanError> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            return Err(ClassScanError::InvalidUtf8);
        } else if b < 0x80 {
            units.push(u16::from(b));
            i += 1;
        } else if b >> 5 == 0b110 {
            let Some(&b2) = bytes.get(i + 1) else {
                return Err(ClassScanError::InvalidUtf8);
            };
            if b2 & 0xC0 != 0x80 {
                return Err(ClassScanError::InvalidUtf8);
            }
            units.push((u16::from(b & 0x1F) << 6) | u16::from(b2 & 0x3F));
            i += 2;
        } else if b >> 4 == 0b1110 {
            let (Some(&b2), Some(&b3)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                return Err(ClassScanError::InvalidUtf8);
            };
            if b2 & 0xC0 != 0x80 || b3 & 0xC0 != 0x80 {
                return Err(ClassScanError::InvalidUtf8);
            }
            units.push(
                (u16::from(b & 0x0F) << 12)
                    | (u16::from(b2 & 0x3F) << 6)
                    | u16::from(b3 & 0x3F),
            );
            i += 3;
        } else {
            return Err(ClassScanError::InvalidUtf8);
        }
    }
    String::from_utf16(&units).map_err(|_| ClassScanError::InvalidUtf8)
}

fn read_constant_pool(r: &mut Cursor<'_>, count: u16) -> Result<ConstantPool, ClassScanError> {
    let mut entries = Vec::with_capacity(usize::from(count));
    entries.push(CpEntry::Vacant);
    let mut index = 1u32;
    while index < u32::from(count) {
        let tag = r.u1("constant pool tag")?;
        let entry = match tag {
            1 => {
                let len = usize::from(r.u2("CONSTANT_Utf8 length")?);
                CpEntry::Utf8(mutf8_decode(r.take(len, "CONSTANT_Utf8 bytes")?)?)
            }
            3 => CpEntry::Integer(r.u4("CONSTANT_Integer")? as i32),
            4 => {
                r.skip(4, "CONSTANT_Float")?;
                CpEntry::Other
            }
            5 | 6 => {
                // Long and Double occupy two constant-pool slots each.
                r.skip(8, "CONSTANT_Long/Double")?;
                entries.push(CpEntry::Other);
                entries.push(CpEntry::Vacant);
                index += 2;
                continue;
            }
            7 => CpEntry::Class {
                name_index: r.u2("CONSTANT_Class name_index")?,
            },
            8 => CpEntry::String {
                utf8_index: r.u2("CONSTANT_String string_index")?,
            },
            9..=12 => {
                r.skip(4, "CONSTANT_*ref/NameAndType")?;
                CpEntry::Other
            }
            15 => {
                r.skip(3, "CONSTANT_MethodHandle")?;
                CpEntry::Other
            }
            16 | 19 | 20 => {
                r.skip(2, "CONSTANT_MethodType/Module/Package")?;
                CpEntry::Other
            }
            17 | 18 => {
                r.skip(4, "CONSTANT_Dynamic/InvokeDynamic")?;
                CpEntry::Other
            }
            other => return Err(ClassScanError::UnsupportedConstantPoolTag(other)),
        };
        entries.push(entry);
        index += 1;
    }
    Ok(ConstantPool(entries))
}

// ---------------------------------------------------------------------------
// Annotations (JVMS §4.7.16–4.7.17)
// ---------------------------------------------------------------------------

/// One `element_value`, stored as raw constant-pool indices — resolution
/// happens at consumption so a wrong-tag slot fails closed with context.
enum ElementValue {
    /// `B C D F I J S Z s` — `const_value_index`.
    Const(u16),
    /// `e` — `type_name_index` + `const_name_index`.
    Enum { const_name: u16 },
    /// `c` — `class_info_index` (`CONSTANT_Class`).
    ClassInfo(u16),
    /// `@` — nested annotation.
    Nested(Annotation),
    /// `[` — array of element values.
    Array(Vec<ElementValue>),
}

struct Annotation {
    type_index: u16,
    elements: Vec<(u16, ElementValue)>,
}

fn parse_annotation(r: &mut Cursor<'_>, depth: usize) -> Result<Annotation, ClassScanError> {
    let type_index = r.u2("annotation type_index")?;
    let pairs = usize::from(r.u2("annotation num_element_value_pairs")?);
    let mut elements = Vec::with_capacity(pairs.min(64));
    for _ in 0..pairs {
        let name_index = r.u2("element_name_index")?;
        elements.push((name_index, parse_element_value(r, depth)?));
    }
    Ok(Annotation {
        type_index,
        elements,
    })
}

fn parse_element_value(r: &mut Cursor<'_>, depth: usize) -> Result<ElementValue, ClassScanError> {
    let tag = r.u1("element_value tag")?;
    match tag {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b's' => {
            Ok(ElementValue::Const(r.u2("const_value_index")?))
        }
        b'e' => {
            let _type_name = r.u2("enum type_name_index")?;
            Ok(ElementValue::Enum {
                const_name: r.u2("enum const_name_index")?,
            })
        }
        b'c' => Ok(ElementValue::ClassInfo(r.u2("class_info_index")?)),
        b'@' | b'[' => {
            if depth >= MAX_ANNOTATION_DEPTH {
                return Err(ClassScanError::AnnotationNestingTooDeep);
            }
            if tag == b'@' {
                Ok(ElementValue::Nested(parse_annotation(r, depth + 1)?))
            } else {
                let count = usize::from(r.u2("array num_values")?);
                let mut values = Vec::with_capacity(count.min(256));
                for _ in 0..count {
                    values.push(parse_element_value(r, depth + 1)?);
                }
                Ok(ElementValue::Array(values))
            }
        }
        _ => Err(ClassScanError::Malformed("unknown element_value tag")),
    }
}

/// Walk one `field_info`/`method_info`/`ClassFile` attribute table,
/// collecting every annotation in `RuntimeVisibleAnnotations` and
/// `RuntimeInvisibleAnnotations` and skipping all other attributes by
/// declared length.
fn read_annotation_attributes(
    r: &mut Cursor<'_>,
    cp: &ConstantPool,
) -> Result<Vec<Annotation>, ClassScanError> {
    let count = usize::from(r.u2("attributes_count")?);
    let mut annotations = Vec::new();
    for _ in 0..count {
        let name_index = r.u2("attribute_name_index")?;
        let name = cp.utf8(name_index, "attribute name")?;
        let len = r.u4("attribute_length")? as usize;
        let payload = r.take(len, "attribute payload")?;
        if name == "RuntimeVisibleAnnotations" || name == "RuntimeInvisibleAnnotations" {
            let mut sub = Cursor::new(payload);
            let num = usize::from(sub.u2("num_annotations")?);
            for _ in 0..num {
                annotations.push(parse_annotation(&mut sub, 0)?);
            }
            if sub.pos != payload.len() {
                return Err(ClassScanError::Malformed(
                    "trailing bytes in annotations attribute",
                ));
            }
        }
    }
    Ok(annotations)
}

// ---------------------------------------------------------------------------
// Mixin annotation semantics
// ---------------------------------------------------------------------------

/// Map an annotation type descriptor under `org/spongepowered/asm/mixin/`
/// to its [`MixinAnnotationKind`], or `None` for unrelated annotations.
fn mixin_annotation_kind(desc: &str) -> Option<MixinAnnotationKind> {
    let path = desc
        .strip_prefix('L')?
        .strip_suffix(';')?
        .strip_prefix("org/spongepowered/asm/mixin/")?;
    Some(match path {
        "injection/Inject" => MixinAnnotationKind::Inject,
        "Overwrite" => MixinAnnotationKind::Overwrite,
        "injection/Redirect" => MixinAnnotationKind::Redirect,
        "injection/ModifyVariable" => MixinAnnotationKind::ModifyVariable,
        "injection/ModifyArg" => MixinAnnotationKind::ModifyArg,
        "injection/ModifyConstant" => MixinAnnotationKind::ModifyConstant,
        "gen/Accessor" => MixinAnnotationKind::Accessor,
        "gen/Invoker" => MixinAnnotationKind::Invoker,
        other => MixinAnnotationKind::Unknown(format!(
            "@{}",
            other.rsplit('/').next().unwrap_or(other)
        )),
    })
}

/// Flatten a string (`s`) element value or an array of them to owned
/// strings; non-string members are ignored.
fn string_elements(
    value: &ElementValue,
    cp: &ConstantPool,
    ctx: &'static str,
) -> Result<Vec<String>, ClassScanError> {
    match value {
        ElementValue::Const(index) => Ok(vec![cp.string(*index, ctx)?.to_string()]),
        ElementValue::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let ElementValue::Const(index) = item {
                    out.push(cp.string(*index, ctx)?.to_string());
                }
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

/// `value`/`targets`-style `c` (class-info) element values to internal
/// class names.
fn class_elements(
    value: &ElementValue,
    cp: &ConstantPool,
) -> Result<Vec<String>, ClassScanError> {
    match value {
        ElementValue::ClassInfo(index) => {
            Ok(vec![cp.class_name(*index, "@Mixin target class")?.to_string()])
        }
        ElementValue::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let ElementValue::ClassInfo(index) = item {
                    out.push(cp.class_name(*index, "@Mixin target class")?.to_string());
                }
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

/// Reduce one nested `@At` annotation to an [`InjectPoint`]: `value` names
/// the point (`HEAD`/`TAIL`/`RETURN`/`INVOKE`/`FIELD`/...), `target` the
/// member for `INVOKE`/`FIELD` points. A non-`@At` nested annotation or a
/// missing `value` maps to `Other`/`Head`-less points that fail closed
/// downstream.
fn inject_point(ann: &Annotation, cp: &ConstantPool) -> Result<InjectPoint, ClassScanError> {
    let desc = cp.utf8(ann.type_index, "@At type")?;
    if desc != "Lorg/spongepowered/asm/mixin/injection/At;" {
        let simple = desc
            .trim_start_matches('L')
            .trim_end_matches(';')
            .rsplit('/')
            .next()
            .unwrap_or(desc);
        return Ok(InjectPoint::Other(simple.to_string()));
    }
    let mut value: Option<String> = None;
    let mut target: Option<String> = None;
    for (name_index, ev) in &ann.elements {
        match cp.utf8(*name_index, "@At element name")? {
            "value" => value = string_elements(ev, cp, "@At value")?.into_iter().next(),
            "target" => target = string_elements(ev, cp, "@At target")?.into_iter().next(),
            _ => {}
        }
    }
    Ok(inject_point_by_name(
        value.as_deref().unwrap_or(""),
        target,
    ))
}

fn inject_point_by_name(name: &str, target: Option<String>) -> InjectPoint {
    match name {
        "HEAD" => InjectPoint::Head,
        "TAIL" => InjectPoint::Tail,
        "RETURN" => InjectPoint::Return,
        "INVOKE" => InjectPoint::Invoke(target.unwrap_or_default()),
        "FIELD" => InjectPoint::Field(target.unwrap_or_default()),
        other => InjectPoint::Other(other.to_string()),
    }
}

/// Append the [`InjectPoint`]s declared by one `at` element — a single
/// nested `@At`, an array of them, or (defensively) a bare string.
fn collect_ats(
    value: &ElementValue,
    cp: &ConstantPool,
    out: &mut Vec<Option<InjectPoint>>,
) -> Result<(), ClassScanError> {
    match value {
        ElementValue::Nested(ann) => out.push(Some(inject_point(ann, cp)?)),
        ElementValue::Array(items) => {
            for item in items {
                match item {
                    ElementValue::Nested(ann) => out.push(Some(inject_point(ann, cp)?)),
                    ElementValue::Const(index) => out.push(Some(inject_point_by_name(
                        cp.string(*index, "@At array member")?,
                        None,
                    ))),
                    _ => out.push(Some(InjectPoint::Other("<non-annotation at>".into()))),
                }
            }
        }
        ElementValue::Const(index) => out.push(Some(inject_point_by_name(
            cp.string(*index, "at element")?,
            None,
        ))),
        _ => out.push(Some(InjectPoint::Other("<unrecognized at>".into()))),
    }
    Ok(())
}

/// Everything one method-level mixin annotation contributes toward
/// [`MixinSite`]s.
struct SiteSpec {
    /// `method` element values (or `value` for `@Accessor`/`@Invoker`);
    /// empty means "handler name" per Mixin's defaulting rule.
    methods: Vec<String>,
    /// Resolved `at` points; empty means no usable `at=` — the site is
    /// emitted with `at: None` and fails closed downstream.
    ats: Vec<Option<InjectPoint>>,
    cancellable: bool,
    captures_locals: bool,
}

fn site_spec(ann: &Annotation, cp: &ConstantPool) -> Result<SiteSpec, ClassScanError> {
    let mut spec = SiteSpec {
        methods: Vec::new(),
        ats: Vec::new(),
        cancellable: false,
        captures_locals: false,
    };
    for (name_index, ev) in &ann.elements {
        match cp.utf8(*name_index, "annotation element name")? {
            "method" => spec
                .methods
                .extend(string_elements(ev, cp, "method element")?),
            "at" => collect_ats(ev, cp, &mut spec.ats)?,
            "cancellable" => {
                spec.cancellable = match ev {
                    ElementValue::Const(index) => {
                        cp.int(*index, "cancellable element")? != 0
                    }
                    _ => {
                        return Err(ClassScanError::Malformed(
                            "cancellable is not a boolean constant",
                        ))
                    }
                }
            }
            "locals" => {
                spec.captures_locals = match ev {
                    // Any LocalCapture enum constant other than NO_CAPTURE
                    // captures locals; an unrecognized `locals` shape is
                    // treated as capturing — fail closed either way.
                    ElementValue::Enum { const_name } => {
                        cp.utf8(*const_name, "locals enum")? != "NO_CAPTURE"
                    }
                    ElementValue::Array(items) => {
                        let mut capture = false;
                        for item in items {
                            match item {
                                ElementValue::Enum { const_name } => {
                                    if cp.utf8(*const_name, "locals enum")? != "NO_CAPTURE" {
                                        capture = true;
                                    }
                                }
                                _ => capture = true,
                            }
                        }
                        capture
                    }
                    _ => true,
                }
            }
            _ => {}
        }
    }
    if spec.methods.is_empty() {
        // @Accessor/@Invoker name their member in `value` instead.
        for (name_index, ev) in &ann.elements {
            if cp.utf8(*name_index, "annotation element name")? == "value" {
                spec.methods
                    .extend(string_elements(ev, cp, "value element")?);
            }
        }
    }
    Ok(spec)
}

/// Append `value` (`c` class-info) and `targets` (`s` string) element
/// values of one `@Mixin` annotation to `targets`, deduplicated.
fn collect_mixin_targets(
    ann: &Annotation,
    cp: &ConstantPool,
    out: &mut Vec<String>,
) -> Result<(), ClassScanError> {
    for (name_index, ev) in &ann.elements {
        let collected = match cp.utf8(*name_index, "@Mixin element name")? {
            "value" => class_elements(ev, cp)?,
            "targets" => string_elements(ev, cp, "@Mixin targets")?,
            _ => continue,
        };
        for name in collected {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse one JVM `.class` file (JVMS §4) and reduce every
/// `org.spongepowered.asm.mixin` method annotation to [`MixinSite`]s.
///
/// Class-level `Runtime*Annotations` are searched for
/// `@org.spongepowered.asm.mixin.Mixin`, whose `value` (class literals)
/// and `targets` (dotted names) elements give the target classes; each
/// method-level mixin annotation then produces one site per
/// `(target_class, method_name, at_point)` combination. A `@Inject`
/// without an explicit `method` element falls back to the handler's own
/// name, matching Mixin's defaulting rule; a missing `at` yields
/// `at: None` so [`lift_mixin_sites`] fails closed on it.
///
/// A class carrying no mixin annotations at all yields `Ok(vec![])`.
/// Anything malformed — bad magic, truncation, out-of-range or wrong-tag
/// constant-pool references, invalid modified UTF-8, runaway nesting —
/// is a [`ClassScanError`], never a panic.
pub fn parse_mixin_class(bytes: &[u8]) -> Result<Vec<MixinSite>, ClassScanError> {
    let mut r = Cursor::new(bytes);
    if r.u4("magic")? != 0xCAFE_BABE {
        return Err(ClassScanError::BadMagic);
    }
    let _minor = r.u2("minor_version")?;
    let _major = r.u2("major_version")?;
    let cp_count = r.u2("constant_pool_count")?;
    if cp_count == 0 {
        return Err(ClassScanError::Malformed("empty constant pool"));
    }
    let cp = read_constant_pool(&mut r, cp_count)?;

    let _access_flags = r.u2("access_flags")?;
    let mixin_class = cp
        .class_name(r.u2("this_class")?, "this_class")?
        .to_string();
    let _super_class = r.u2("super_class")?;
    let interfaces = usize::from(r.u2("interfaces_count")?);
    r.skip(
        interfaces
            .checked_mul(2)
            .ok_or(ClassScanError::Malformed("interfaces overflow"))?,
        "interfaces",
    )?;

    // Fields: everything is skipped by declared length — @Shadow fields
    // declare state, not injection sites.
    for _ in 0..usize::from(r.u2("fields_count")?) {
        r.skip(6, "field_info")?;
        for _ in 0..usize::from(r.u2("field attributes_count")?) {
            r.skip(2, "field attribute_name_index")?;
            let len = r.u4("field attribute_length")? as usize;
            r.skip(len, "field attribute")?;
        }
    }

    // Methods: name + descriptor + annotation attributes (Code etc.
    // skipped). The descriptor is kept so read-only `@Accessor` getters
    // (`()T`) can be told apart from mutating `set*` methods (`(T)V`).
    let mut methods = Vec::new();
    for _ in 0..usize::from(r.u2("methods_count")?) {
        r.skip(2, "method access_flags")?;
        let name_index = r.u2("method name_index")?;
        let descriptor_index = r.u2("method descriptor_index")?;
        methods.push((
            name_index,
            descriptor_index,
            read_annotation_attributes(&mut r, &cp)?,
        ));
    }

    // Class-level attributes: @Mixin itself plus anything else ignored.
    let class_annotations = read_annotation_attributes(&mut r, &cp)?;

    let mut targets = Vec::new();
    for ann in &class_annotations {
        if cp.utf8(ann.type_index, "class annotation type")?
            == "Lorg/spongepowered/asm/mixin/Mixin;"
        {
            collect_mixin_targets(ann, &cp, &mut targets)?;
        }
    }
    if targets.is_empty() {
        // A class with mixin method annotations but no @Mixin (or an
        // empty target list) still emits sites — with an empty target
        // class, which the lifter rejects as UnknownTarget.
        targets.push(String::new());
    }

    let mut sites = Vec::new();
    for (name_index, descriptor_index, annotations) in &methods {
        for ann in annotations {
            let desc = cp.utf8(ann.type_index, "method annotation type")?;
            let Some(kind) = mixin_annotation_kind(desc) else {
                continue;
            };
            let mut spec = site_spec(ann, &cp)?;
            if spec.methods.is_empty() {
                spec.methods
                    .push(cp.utf8(*name_index, "method name")?.to_string());
            }
            if spec.ats.is_empty() {
                spec.ats.push(None);
            }
            let handler_name = cp.utf8(*name_index, "method name")?.to_string();
            let handler_descriptor =
                cp.utf8(*descriptor_index, "method descriptor")?.to_string();
            for target in &targets {
                for method in &spec.methods {
                    for at in &spec.ats {
                        sites.push(MixinSite {
                            mixin_class: mixin_class.clone(),
                            target_class: target.clone(),
                            target_method: method.clone(),
                            annotation: kind.clone(),
                            at: at.clone(),
                            cancellable: spec.cancellable,
                            captures_locals: spec.captures_locals,
                            handler_name: handler_name.clone(),
                            handler_descriptor: handler_descriptor.clone(),
                        });
                    }
                }
            }
        }
    }
    Ok(sites)
}

/// Resolve one mixin-config class entry (`"LivingEntityMixin"`,
/// `"sub.Foo"`, object form `{"name": ...}` handled by the caller) to the
/// archive member path `package` + entry names it.
fn mixin_class_path(package: &str, entry: &str) -> String {
    let package = package.trim().trim_matches('/').replace('.', "/");
    let entry = entry.trim().trim_start_matches('/');
    let entry = entry.strip_suffix(".class").unwrap_or(entry);
    let rel = entry.replace('.', "/");
    if package.is_empty() {
        format!("{rel}.class")
    } else {
        format!("{package}/{rel}.class")
    }
}

/// The class-name list under one mixin-config key (`mixins`, `server`,
/// `client`): strings, or `{"name": "..."}` objects. Absent keys give an
/// empty list; a present-but-misshaped value is a hard error.
fn config_class_names(
    obj: &BTreeMap<String, JsonValue>,
    key: &str,
    config: &str,
) -> Result<Vec<String>, ClassScanError> {
    let Some(value) = obj.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or(ClassScanError::Malformed("mixin config array is not an array"))?;
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        if let Some(name) = item.as_str() {
            names.push(name.to_string());
        } else if let Some(name) = item
            .as_object()
            .and_then(|o| o.get("name"))
            .and_then(JsonValue::as_str)
        {
            names.push(name.to_string());
        } else {
            return Err(ClassScanError::Json(format!(
                "{config}: unparseable entry in \"{key}\" array"
            )));
        }
    }
    Ok(names)
}

/// Extract one archive member under the cumulative scan budget.
fn read_member_budget(
    jar_bytes: &[u8],
    entry: &modpack::ZipMemberEntry,
    display: &str,
    extracted: &mut u64,
) -> Result<Vec<u8>, ClassScanError> {
    let bytes = modpack::read_zip_member(jar_bytes, entry, display)?;
    *extracted = extracted
        .checked_add(bytes.len() as u64)
        .ok_or(ClassScanError::Malformed("scan byte budget overflow"))?;
    if *extracted > MAX_JAR_SCAN_BYTES {
        return Err(ClassScanError::Malformed(
            "JAR scan byte budget exceeded",
        ));
    }
    Ok(bytes)
}

fn scan_jar(
    jar_bytes: &[u8],
    display: &str,
    opts: JarScanOptions,
) -> Result<JarMixinScanReport, ClassScanError> {
    let entries = modpack::zip_member_list(jar_bytes, display)?;
    let mut wanted = BTreeSet::new();
    let mut configs_found = 0usize;
    let mut skipped_client = 0usize;
    let mut extracted = 0u64;

    for entry in &entries {
        if !entry.name.ends_with(".mixins.json") {
            continue;
        }
        let bytes = read_member_budget(jar_bytes, entry, display, &mut extracted)?;
        let root = parse_json(&bytes)?;
        let obj = root
            .as_object()
            .ok_or(ClassScanError::Malformed("mixin config root is not an object"))?;
        configs_found += 1;
        let package = obj.get("package").and_then(JsonValue::as_str).unwrap_or("");
        for name in config_class_names(obj, "mixins", &entry.name)? {
            wanted.insert(mixin_class_path(package, &name));
        }
        for name in config_class_names(obj, "server", &entry.name)? {
            wanted.insert(mixin_class_path(package, &name));
        }
        let client = config_class_names(obj, "client", &entry.name)?;
        if opts.server_only {
            skipped_client += client.len();
        } else {
            for name in client {
                wanted.insert(mixin_class_path(package, &name));
            }
        }
    }

    let mut sites = Vec::new();
    let mut classes_scanned = 0usize;
    if configs_found > 0 {
        // Configs exist: only the registered classes can apply, so only
        // they are parsed. A registered class missing from the archive is
        // unscannable — fail closed.
        for path in &wanted {
            let entry = entries
                .iter()
                .find(|e| &e.name == path)
                .ok_or_else(|| ClassScanError::MissingMixinClass(path.clone()))?;
            let bytes = read_member_budget(jar_bytes, entry, display, &mut extracted)?;
            classes_scanned += 1;
            sites.extend(parse_mixin_class(&bytes)?);
        }
    } else {
        // No config: defensively parse every class member.
        for entry in &entries {
            if !entry.name.ends_with(".class") || entry.name.starts_with("META-INF/") {
                continue;
            }
            if classes_scanned >= MAX_CLASS_MEMBERS_SCANNED {
                return Err(ClassScanError::Malformed("too many class members"));
            }
            let bytes = read_member_budget(jar_bytes, entry, display, &mut extracted)?;
            classes_scanned += 1;
            sites.extend(parse_mixin_class(&bytes)?);
        }
    }

    let verdict = lift_mixin_sites(&sites);
    Ok(JarMixinScanReport {
        mixin_configs_found: configs_found,
        classes_scanned,
        skipped_client_mixins: skipped_client,
        sites,
        verdict,
    })
}

/// Scan a complete mod JAR in memory with the default
/// [`JarScanOptions`] (server-side: `client`-only mixin classes skipped).
/// See the module docs for the scan strategy and fail-closed rules.
pub fn scan_jar_mixins(jar_bytes: &[u8]) -> Result<JarMixinScanReport, ClassScanError> {
    scan_jar(jar_bytes, "mod JAR", JarScanOptions::default())
}

/// [`scan_jar_mixins`] with explicit scan options.
pub fn scan_jar_mixins_with_options(
    jar_bytes: &[u8],
    opts: JarScanOptions,
) -> Result<JarMixinScanReport, ClassScanError> {
    scan_jar(jar_bytes, "mod JAR", opts)
}

/// Read `path` and scan it as a mod JAR with default options.
pub fn scan_jar_path(path: &Path) -> Result<JarMixinScanReport, ClassScanError> {
    scan_jar_path_with_options(path, JarScanOptions::default())
}

/// [`scan_jar_path`] with explicit scan options.
pub fn scan_jar_path_with_options(
    path: &Path,
    opts: JarScanOptions,
) -> Result<JarMixinScanReport, ClassScanError> {
    let display = path.display().to_string();
    let bytes = std::fs::read(path)
        .map_err(|e| ClassScanError::Io(format!("{display}: cannot read JAR: {e}")))?;
    scan_jar(&bytes, &display, opts)
}
