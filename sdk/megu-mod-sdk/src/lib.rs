// SPDX-License-Identifier: LGPL-3.0-or-later

//! `megu-mod-sdk`: the standalone, zero-dependency Mod Developer SDK.
//!
//! One dependency (`megu-core`, path-local) supplies the canonical
//! [`ResourceLocation`] validator, [`megu_core::hash::crc32_ieee`], and the
//! [`megu_core::zip_archive`] reader this SDK's packager verifies against.
//! There are **zero crates.io dependencies**, which keeps `cargo-deny` 100%
//! clean and makes the crate embeddable anywhere `std` runs.
//!
//! # Authoring flow
//!
//! ```ignore
//! let project = ModProjectBuilder::new("my_mod", "1.0.0")
//!     .display_name("My Mod")
//!     .mc_version_range("[1.21.1,1.22)")
//!     .block(BlockBuilder::new("brass_block").hardness(3.5))
//!     .item(ItemBuilder::new("brass_ingot").max_stack(64))
//!     .recipe(RecipeBuilder::shapeless(
//!         "brass_ingot_from_nuggets",
//!         &["my_mod:brass_nugget"; 9],
//!         "my_mod:brass_ingot",
//!     ))
//!     .build()?;
//! let bundle_json = project.export_megu_bundle_json()?; // strict megu-mod-bundle/1
//! let mmod = project.export_mmod_bytes()?;              // native .mmod archive
//! ModTestHarness::from_project(&project)?.assert_bundle_valid()?;
//! ```
//!
//! # `.mmod` — the native package format
//!
//! A `.mmod` is a deterministic stored-ZIP archive:
//!
//! * `megu-mod.toml` — the native manifest (schema `megu-mod/1`),
//! * `megu-mod-bundle.json` — a **strict** `megu-mod-bundle/1` document
//!   byte-acceptable to `mod-adapter-core`'s `DeclarativeModBundle::from_json`,
//! * `data/<ns>/recipe/*.json`, `data/<ns>/tags/{item,block}/*.json`,
//!   `data/<mod>/dimension_type/*.json`, `data/<mod>/megu/registry.json`
//!   (block/item/capability/creature extension records),
//! * `assets/<mod>/lang/<locale>.json`, `assets/<mod>/blockstates/*.json`.
//!
//! [`ModProject::export_compat_jar_bytes`] additionally emits the Java
//! dual-loader manifests (`META-INF/neoforge.mods.toml`, `fabric.mod.json`)
//! — an explicit opt-in for authors shipping one artifact to legacy loaders.
//! Native `.mmod` carries **no** Java metadata.
//!
//! # `megu-mod-bundle/1` note
//!
//! `unified_bundle.rs` is fail-closed on unknown keys: the only top-level
//! keys `from_json` accepts are `schema`, `mod_id`, `mc_version_range`,
//! `portals`, `progression_barriers`, `biome_regions`, `creatures`,
//! `custom_channels`, and the optional `spells` / `custom_statuses` /
//! `rituals`. Blocks, items, recipes, tags, capabilities and dimensions are
//! therefore exported as datapack/registry files inside the archive, while
//! [`CreatureBuilder`] declarations map natively onto the bundle's
//! `creatures` section.

#![deny(unsafe_code)]

use std::error::Error;
use std::fmt;

pub mod builders;
pub mod jar_packer;
pub mod native_abi;
pub mod test_harness;

pub use builders::{
    BlockBuilder, BlockSpec, CapabilitySpec, CreatureBuilder, CreatureSpec, DependencySpec,
    DimensionBuilder, DimensionSpec, ElementalBreath, FoodProps, ItemBuilder, ItemResult,
    ItemSpec, LocaleBuilder, LocaleSpec, ModProject, ModProjectBuilder, Movement, Rarity,
    RecipeBuilder, RecipeKind, RecipeSpec, TagBuilder, TagKind, TagSpec, BUNDLE_SCHEMA,
};
pub use jar_packer::{verify_stored_zip, VerifiedEntry, BUNDLE_ENTRY, MMOD_MANIFEST};
pub use test_harness::{diagnostics_to_json, ModTestHarness, SdkDiagnostic};

/// Failure type for export/pack operations. Model problems are reported as
/// structured [`SdkDiagnostic`]s — one `Vec` carrying every problem found,
/// so an author (or an AI agent) can fix them in one pass.
#[derive(Clone, Debug, PartialEq)]
pub enum SdkError {
    /// One or more structured model diagnostics.
    Diagnostics(Vec<SdkDiagnostic>),
    /// Archive emission or round-trip verification failed.
    Archive(String),
}

impl SdkError {
    /// The diagnostics carried by this error (`[&]` for archive failures).
    pub fn diagnostics(&self) -> &[SdkDiagnostic] {
        match self {
            Self::Diagnostics(diags) => diags,
            Self::Archive(_) => &[],
        }
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diagnostics(diags) => {
                write!(f, "{} diagnostic(s)", diags.len())?;
                if let Some(first) = diags.first() {
                    write!(f, ": [{}] {}: {}", first.code, first.path, first.message)?;
                }
                Ok(())
            }
            Self::Archive(msg) => write!(f, "archive: {msg}"),
        }
    }
}

impl Error for SdkError {}

impl From<Vec<SdkDiagnostic>> for SdkError {
    fn from(diags: Vec<SdkDiagnostic>) -> Self {
        Self::Diagnostics(diags)
    }
}

impl From<SdkDiagnostic> for SdkError {
    fn from(diag: SdkDiagnostic) -> Self {
        Self::Diagnostics(vec![diag])
    }
}

// ---------------------------------------------------------------------------
// Shared bounds (mirroring unified_bundle.rs fail-closed caps)
// ---------------------------------------------------------------------------

/// Top-level section entry cap in `megu-mod-bundle/1`.
pub(crate) const MAX_SECTION_ENTRIES: usize = 1024;
/// Nested list/map entry cap in `megu-mod-bundle/1`.
pub(crate) const MAX_INNER_ENTRIES: usize = 512;
/// Fabric's mod-id length cap — applied to every mod/dependency id.
pub(crate) const MAX_MOD_ID_LEN: usize = 64;
/// Total archive byte cap for the packager (a mod data pack is small).
pub(crate) const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
/// Bound on archive member count (well under the u16 ZIP field).
pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 8 * 1024;

/// `ResourceLocation` namespace charset check (`[a-z0-9_.-]`, non-empty,
/// no `..` traversal segments), plus the Fabric 64-char ceiling.
pub(crate) fn check_mod_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("mod id must be non-empty".to_string());
    }
    if id.len() > MAX_MOD_ID_LEN {
        return Err(format!("mod id exceeds {MAX_MOD_ID_LEN} characters"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
    {
        return Err("mod id must match [a-z0-9_.-]+".to_string());
    }
    if id.split('.').any(|seg| seg.is_empty()) {
        return Err("mod id contains an empty '.'-separated segment".to_string());
    }
    if id.contains("..") || id.contains('/') || id.contains('\\') {
        return Err("mod id contains a traversal sequence".to_string());
    }
    Ok(())
}

/// A `&'static str` diagnostic code + structured message, used internally
/// before [`SdkDiagnostic`] construction.
pub(crate) mod diag {
    use crate::test_harness::SdkDiagnostic;

    pub(crate) fn mk(
        code: &'static str,
        path: impl AsRef<str>,
        message: impl Into<String>,
        fix_hint: impl Into<String>,
    ) -> SdkDiagnostic {
        SdkDiagnostic {
            code,
            path: path.as_ref().to_string(),
            message: message.into(),
            fix_hint: fix_hint.into(),
        }
    }
}

/// Validate a *declaration* id (block/item/recipe/tag/creature/dimension).
/// Bare `path` ids are qualified with `default_ns`; namespaced ids keep
/// their namespace. The result is always a valid `ResourceLocation`.
pub(crate) fn qualify_decl_id(
    raw: &str,
    default_ns: &str,
    ctx: &str,
) -> Result<String, SdkDiagnostic> {
    let qualified = if raw.contains(':') {
        raw.to_string()
    } else {
        format!("{default_ns}:{raw}")
    };
    megu_core::id::ResourceLocation::parse(&qualified)
        .map(|rl| rl.as_str().to_string())
        .map_err(|e| {
            diag::mk(
                "E_ID",
                ctx,
                format!("invalid id {qualified:?}: {e}"),
                "use lowercase [a-z0-9_.-] namespaces and [a-z0-9_./-] paths",
            )
        })
}

/// Validate a *reference* id (ingredient, loot drop, tag member, tame item).
/// `#`-prefixed strings are tag references; the `#` is preserved and the
/// remainder normalized as a `ResourceLocation` (bare → `minecraft:`).
pub(crate) fn normalize_ref(raw: &str, ctx: &str) -> Result<String, SdkDiagnostic> {
    let (prefix, body) = match raw.strip_prefix('#') {
        Some(rest) => ("#", rest),
        None => ("", raw),
    };
    megu_core::id::ResourceLocation::parse(body)
        .map(|rl| format!("{prefix}{}", rl.as_str()))
        .map_err(|e| {
            diag::mk(
                "E_REF_ID",
                ctx,
                format!("invalid reference {raw:?}: {e}"),
                "use 'namespace:path' or '#namespace:tag' resource ids",
            )
        })
}

// ---------------------------------------------------------------------------
// Minimal Maven/SemVer version-range well-formedness check
// ---------------------------------------------------------------------------

fn numeric_version_ok(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn semver_spec_ok(s: &str) -> bool {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = s.split('+').next().unwrap_or("");
    if s.is_empty() || s.contains('-') {
        return false;
    }
    if s == "*" {
        return true;
    }
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() <= 3
        && !parts.is_empty()
        && parts.iter().enumerate().all(|(i, p)| {
            if p.is_empty() {
                return false;
            }
            if p.bytes().all(|b| b.is_ascii_digit()) {
                true
            } else {
                // Wildcards may only appear as the trailing component.
                matches!(*p, "x" | "X" | "*") && i + 1 == parts.len()
            }
        })
}

/// Well-formedness check for `mc_version_range` / dependency ranges, accepting
/// both Maven interval syntax (`[1.21.1,1.22)`) and Fabric SemVer predicates
/// (`>=1.21.1 <1.22`, `1.21.x`, `^1.21`). The authoritative parser lives in
/// `mod-adapter-core::version_compat`; this is the SDK's early-error mirror.
pub(crate) fn check_version_range(raw: &str) -> Result<(), String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("version range must be non-empty".to_string());
    }
    if s.starts_with(['[', '(']) {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if !matches!(bytes[i], b'[' | b'(') {
                return Err(format!("malformed Maven interval in {s:?}"));
            }
            let rel = s[i + 1..]
                .find([']', ')'])
                .ok_or_else(|| format!("unterminated interval in {s:?}"))?;
            let close = i + 1 + rel;
            let inner = &s[i + 1..close];
            let parts: Vec<&str> = inner.split(',').collect();
            match parts.len() {
                1 => {
                    if !numeric_version_ok(parts[0]) {
                        return Err(format!("bad exact version {:?} in {s:?}", parts[0]));
                    }
                }
                2 => {
                    for p in &parts {
                        let p = p.trim();
                        if !p.is_empty() && !numeric_version_ok(p) {
                            return Err(format!("bad interval bound {p:?} in {s:?}"));
                        }
                    }
                }
                _ => return Err(format!("interval has too many bounds in {s:?}")),
            }
            i = close + 1;
        }
        return Ok(());
    }
    for alt in s.split("||") {
        let alt = alt.trim();
        if alt.is_empty() {
            return Err(format!("empty alternative in {s:?}"));
        }
        for tok in alt.split_whitespace() {
            let body = [">=", "<=", "~", "^", ">", "<", "="]
                .iter()
                .find_map(|op| tok.strip_prefix(op))
                .unwrap_or(tok);
            if body.is_empty() || !semver_spec_ok(body) {
                return Err(format!("bad predicate {tok:?} in {s:?}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Canonical JSON emitter (deterministic sorted-key rendering)
// ---------------------------------------------------------------------------

/// Deterministic JSON AST + renderer mirroring `unified_bundle.rs`'s
/// `emit_json`: `BTreeMap` objects sort keys, numbers render in the
/// shortest reparseable form (f32-narrowed when lossless), strings escape
/// every non-ASCII/control character. Two-space indent, `": "` separators.
pub(crate) mod json {
    use std::collections::BTreeMap;
    use std::fmt::Write;

    /// A JSON value. `Obj` keys sort on construction via `BTreeMap`.
    #[derive(Clone, Debug, PartialEq)]
    pub(crate) enum J {
        Bool(bool),
        Num(f64),
        Str(String),
        Arr(Vec<J>),
        Obj(BTreeMap<String, J>),
    }

    pub(crate) fn s(v: &str) -> J {
        J::Str(v.to_string())
    }

    pub(crate) fn n(v: f64) -> J {
        J::Num(v)
    }

    pub(crate) fn u(v: u64) -> J {
        J::Num(v as f64)
    }

    pub(crate) fn i(v: i64) -> J {
        J::Num(v as f64)
    }

    pub(crate) fn b(v: bool) -> J {
        J::Bool(v)
    }

    pub(crate) fn arr(items: Vec<J>) -> J {
        J::Arr(items)
    }

    pub(crate) fn obj(pairs: Vec<(&str, J)>) -> J {
        J::Obj(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// RFC 8259 string quoting: `"` and `\` escapes, the `\b`/`\f`/`\n`/
    /// `\r`/`\t` shorthands, printable ASCII verbatim, everything else as
    /// `\uXXXX` UTF-16 units — byte-identical to the canonical emitter.
    pub(crate) fn quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (' '..='~').contains(&c) => out.push(c),
                c => {
                    let mut buf = [0u16; 2];
                    for unit in c.encode_utf16(&mut buf) {
                        let _ = write!(out, "\\u{unit:04x}");
                    }
                }
            }
        }
        out.push('"');
        out
    }

    /// Render `v` with the canonical two-space-indented layout.
    pub(crate) fn render(v: &J) -> String {
        let mut out = String::with_capacity(256);
        emit(v, &mut out, 0);
        out
    }

    fn indent(out: &mut String, depth: usize) {
        for _ in 0..depth {
            out.push_str("  ");
        }
    }

    fn emit(v: &J, out: &mut String, depth: usize) {
        match v {
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => {
                if !n.is_finite() {
                    out.push('0');
                } else {
                    let narrowed = *n as f32;
                    if n.to_bits() == f64::from(narrowed).to_bits() {
                        let _ = write!(out, "{narrowed}");
                    } else {
                        let _ = write!(out, "{n}");
                    }
                }
            }
            J::Str(s) => out.push_str(&quote(s)),
            J::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (k, item) in items.iter().enumerate() {
                    indent(out, depth + 1);
                    emit(item, out, depth + 1);
                    out.push_str(if k + 1 == items.len() { "\n" } else { ",\n" });
                }
                indent(out, depth);
                out.push(']');
            }
            J::Obj(map) => {
                if map.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                let last = map.len() - 1;
                for (k, (key, value)) in map.iter().enumerate() {
                    indent(out, depth + 1);
                    out.push_str(&quote(key));
                    out.push_str(": ");
                    emit(value, out, depth + 1);
                    out.push_str(if k == last { "\n" } else { ",\n" });
                }
                indent(out, depth);
                out.push('}');
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TOML emission helpers (subset accepted by ecosystem_compat's parser)
// ---------------------------------------------------------------------------

/// Quote a TOML basic string (escapes `\`, `"`, and control chars).
pub(crate) fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => {
                use fmt::Write;
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A TOML key part: bare when `[A-Za-z0-9_-]+`, basic-quoted otherwise
/// (e.g. mod ids containing `.`).
pub(crate) fn toml_key(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && !s.is_empty()
    {
        s.to_string()
    } else {
        toml_string(s)
    }
}
