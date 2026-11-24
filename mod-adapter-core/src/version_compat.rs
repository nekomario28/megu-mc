// SPDX-License-Identifier: LGPL-3.0-or-later

//! Multi-version Minecraft compatibility engine: a pure-`std`, zero-dependency
//! parser for the two version-range syntaxes real modloader ecosystems ship
//! plus the datapack `pack_format` / `DataVersion` lookup table.
//!
//! Supported range syntaxes:
//! - **NeoForge / Forge / Maven interval syntax** (used by `neoforge.mods.toml`
//!   `versionRange` and Forge `mods.toml`): exact pins `[1.21.1]`, closed/open
//!   intervals `[1.21.1,1.21.5)` / `(1.21,1.22]`, unbounded ends `[1.21.1,)` /
//!   `(,1.21.5)`, comma-separated union intervals `(,1.20.5],[1.21.1,)`, and
//!   exact bare versions (`1.21.1`).
//! - **Fabric / SemVer syntax** (used by `fabric.mod.json` `depends`):
//!   comparators `>=1.21.1`, `<=1.21.4`, `>1.21`, `<1.22`, `=1.21.1`, tilde
//!   `~1.21.1` (`>=1.21.1 <1.22.0`), caret `^1.21.1` (`>=1.21.1 <2.0.0`),
//!   wildcards `1.21.x` / `1.21.*` / `*`, space-separated AND predicates
//!   (`>=1.21.1 <1.21.5`), and `||` OR unions (`1.21.1 || 1.21.4`).
//!   Partial versions follow npm range convention and cover their whole
//!   band: bare/`=` `1.21` means `1.21.x`, so `>1.21` is `>=1.22.0` (write
//!   `>1.21.0` for "newer than 1.21.0" literally).
//!
//! Everything parses fail-closed: a malformed or empty range string yields a
//! [`VersionRangeError`] and never panics, so a corrupt mod manifest degrades
//! to a clean "incompatible" diagnostic instead of a startup crash.
//!
//! The `1.21.1` and `26.3` rows of the `pack_format` / `DataVersion` lookup
//! table source their numbers from [`megu_core::version::TargetVersion`],
//! the single source of truth the whole crate DAG shares; historical rows
//! (`1.20.4`-`1.21.5`, `1.21.0`) stay local since `megu-core` only pins
//! the two current targets.

use std::fmt;

use megu_core::version::TargetVersion;

/// A Minecraft release version triple (`major.minor.patch`).
///
/// Minecraft releases are either `1.<minor>[.<patch>]` (the 1.10-1.21.x line)
/// or the year-based `<year>.<minor>[.<patch>]` scheme (26.x onward);
/// `parse` accepts the two-component shorthand (`"1.21"` means `1.21.0`,
/// `"26.3"` means `26.3.0`) so bounds copied from mod manifests compare
/// correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl McVersion {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub const V1_20_4: Self = Self::new(1, 20, 4);
    pub const V1_20_6: Self = Self::new(1, 20, 6);
    pub const V1_21_0: Self = Self::new(1, 21, 0);
    pub const V1_21_1: Self = Self::new(1, 21, 1);
    pub const V1_21_3: Self = Self::new(1, 21, 3);
    pub const V1_21_4: Self = Self::new(1, 21, 4);
    pub const V1_21_5: Self = Self::new(1, 21, 5);
    /// Latest vanilla 26.3 (protocol 777, `DataVersion` 5023): the second
    /// first-class target, on the year-based `26.<minor>` scheme.
    pub const V26_3: Self = Self::new(26, 3, 0);

    /// The Minecraft version this `megu` runtime currently implements.
    /// Used as the default target for version-aware mod classification; bump
    /// when the wire/runtime moves to a newer release line.
    pub const CURRENT: Self = Self::V1_21_1;

    /// Parse `"1.21.1"`, `"1.21"` (= `1.21.0`), `"26.3"` (= `26.3.0`), or
    /// `"1"` (= `1.0.0`).
    /// Returns `None` on empty input, non-numeric or missing components, more
    /// than three components, or components outside `u16`.
    pub fn parse(s: &str) -> Option<Self> {
        parse_numeric_version(s.trim()).ok()
    }
}

impl fmt::Display for McVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Error from `TryFrom<McVersion>` for [`TargetVersion`]: the version is not
/// one of the two canonical targets (`1.21.1`, `26.3`).
///
/// Other table rows (`1.20.4`-`1.21.5`, `1.21.0`) are real versions the range
/// engine understands but they sit outside the `megu-core` target set —
/// they fail here instead of silently collapsing onto the nearest target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoTargetVersion(pub McVersion);

impl fmt::Display for NoTargetVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not a canonical megu target version", self.0)
    }
}

impl std::error::Error for NoTargetVersion {}

impl From<TargetVersion> for McVersion {
    fn from(target: TargetVersion) -> Self {
        match target {
            TargetVersion::V1_21_1 => Self::V1_21_1,
            TargetVersion::V26_3 => Self::V26_3,
        }
    }
}

impl TryFrom<McVersion> for TargetVersion {
    type Error = NoTargetVersion;

    fn try_from(version: McVersion) -> Result<Self, Self::Error> {
        if version == McVersion::V1_21_1 {
            Ok(Self::V1_21_1)
        } else if version == McVersion::V26_3 {
            Ok(Self::V26_3)
        } else {
            Err(NoTargetVersion(version))
        }
    }
}

/// Total-order key packing a version into one integer so open/closed bounds
/// become inclusive integer interval math (`key(1.21.65535)+1 == key(1.22.0)`
/// falls out of the layout, which is exactly what `>1.21.x` should mean).
fn vkey(v: McVersion) -> u64 {
    ((v.major as u64) << 32) | ((v.minor as u64) << 16) | v.patch as u64
}

const KEY_MAX: u64 = u64::MAX;

/// Numeric-only version parser shared by [`McVersion::parse`] and the Maven
/// interval path: 1-3 all-digit components, no wildcards.
fn parse_numeric_version(s: &str) -> Result<McVersion, String> {
    if s.is_empty() {
        return Err("empty version".to_string());
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 3 {
        return Err(format!("version '{s}' has more than three components"));
    }
    let mut n = [0u16; 3];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("version '{s}' has non-numeric component '{part}'"));
        }
        n[i] = part
            .parse::<u16>()
            .map_err(|_| format!("version '{s}' component '{part}' out of range"))?;
    }
    Ok(McVersion::new(n[0], n[1], n[2]))
}

/// A SemVer-ish version *spec*: dotted components where a trailing `x`/`X`/`*`
/// wildcard (or an omitted component) widens the match to a whole band.
/// `specified` counts the leading numeric components: `1.21.x` -> 2,
/// `1.x`/`1` -> 1, `*` -> 0.
struct VersionSpec {
    lo: [u64; 3],
    specified: usize,
}

fn parse_spec(s: &str) -> Result<VersionSpec, String> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let s = s.split('+').next().unwrap_or("");
    if s.is_empty() {
        return Err("empty version spec".to_string());
    }
    if s.contains('-') {
        return Err(format!(
            "prerelease/hyphenated spec '{s}' is unsupported (use an explicit range)"
        ));
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 3 {
        return Err(format!("version spec '{s}' has more than three components"));
    }
    let mut lo = [0u64; 3];
    let mut specified = parts.len();
    let mut saw_wildcard = false;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(format!("version spec '{s}' has an empty component"));
        }
        if matches!(*part, "x" | "X" | "*") {
            saw_wildcard = true;
            if specified == parts.len() {
                specified = i;
            }
            continue;
        }
        if saw_wildcard {
            return Err(format!(
                "version spec '{s}' has a numeric component after a wildcard"
            ));
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "version spec '{s}' has non-numeric component '{part}'"
            ));
        }
        let n: u64 = part
            .parse()
            .map_err(|_| format!("version spec '{s}' component '{part}' out of range"))?;
        if n > u16::MAX as u64 {
            return Err(format!(
                "version spec '{s}' component '{part}' out of range"
            ));
        }
        lo[i] = n;
    }
    Ok(VersionSpec { lo, specified })
}

fn spec_lo_key(spec: &VersionSpec) -> u64 {
    (spec.lo[0] << 32) | (spec.lo[1] << 16) | spec.lo[2]
}

/// Exclusive upper key of the band the spec covers: `1.21.1` -> `1.21.2`,
/// `1.21.x` -> `1.22.0`, `1.x` -> `2.0.0`, `*` -> unbounded.
fn spec_band_hi(spec: &VersionSpec) -> u64 {
    match spec.specified {
        0 => KEY_MAX,
        1 => (spec.lo[0] + 1) << 32,
        2 => (spec.lo[0] << 32) | ((spec.lo[1] + 1) << 16),
        _ => spec_lo_key(spec) + 1,
    }
}

/// One SemVer predicate token -> inclusive `[lo, hi]` key interval.
fn predicate_interval(tok: &str) -> Result<(u64, u64), String> {
    if matches!(tok, "*" | "x" | "X") {
        return Ok((0, KEY_MAX));
    }
    let (op, body) = ["~", "^", ">=", "<=", ">", "<", "="]
        .iter()
        .find_map(|op| tok.strip_prefix(op).map(|b| (*op, b)))
        .unwrap_or(("", tok));
    if body.is_empty() {
        return Err(format!("predicate '{tok}' is missing a version"));
    }
    let spec = parse_spec(body)?;
    if spec.specified == 0 {
        // `*`, `~*`, `^*`, `>=1.x` style: wildcard-only spec matches anything.
        return Ok((0, KEY_MAX));
    }
    let lo = spec_lo_key(&spec);
    let hi_band = spec_band_hi(&spec);
    Ok(match op {
        ">=" => (lo, KEY_MAX),
        ">" => (hi_band, KEY_MAX),
        "<=" => {
            if spec.specified == 3 {
                (0, lo)
            } else {
                (0, hi_band - 1)
            }
        }
        "<" => {
            if lo == 0 {
                (1, 0) // `<0.0.0` matches nothing; empty interval marker.
            } else {
                (0, lo - 1)
            }
        }
        "=" => (lo, hi_band - 1),
        "~" => {
            // Tilde keeps the minor band: `~1.21.1` is `>=1.21.1 <1.22.0`,
            // `~1` is `>=1.0.0 <2.0.0`.
            let hi = if spec.specified >= 2 {
                (spec.lo[0] << 32) | ((spec.lo[1] + 1) << 16)
            } else {
                (spec.lo[0] + 1) << 32
            };
            (lo, hi - 1)
        }
        "^" => {
            // Caret allows everything below the leftmost non-zero component's
            // increment: `^1.21.1` <2.0.0, `^0.2.3` <0.3.0, `^0.0.3` <0.0.4,
            // `^0.0` <0.1.0, `^0` <1.0.0.
            let hi = if spec.lo[0] > 0 {
                (spec.lo[0] + 1) << 32
            } else if spec.specified >= 2 && spec.lo[1] > 0 {
                (spec.lo[1] + 1) << 16
            } else if spec.specified == 3 {
                spec.lo[2] + 1
            } else if spec.specified == 2 {
                1 << 16
            } else {
                1 << 32
            };
            (lo, hi - 1)
        }
        // Bare spec: a full `1.21.1` pins exactly, a partial `1.21`/`1` covers
        // its whole band (`1.21.x`-style), matching SemVer range convention.
        _ => (lo, hi_band - 1),
    })
}

/// Parse one bracketed Maven interval member starting at `s[i]` (`[` or `(`);
/// returns `(lo, hi)` inclusive keys and the index just past the closer.
fn parse_maven_member(s: &str, i: usize) -> Result<(u64, u64, usize), String> {
    let bytes = s.as_bytes();
    let open_incl = bytes[i] == b'[';
    let close = s[i + 1..]
        .find([']', ')'])
        .ok_or_else(|| format!("unterminated interval in '{s}'"))?
        + i
        + 1;
    let close_incl = bytes[close] == b']';
    let inner = &s[i + 1..close];
    let parts: Vec<&str> = inner.split(',').collect();
    let interval = match parts.len() {
        1 => {
            let v = parts[0].trim();
            if v.is_empty() {
                return Err(format!("empty interval '{inner}' in '{s}'"));
            }
            if !open_incl {
                return Err(format!(
                    "exact pin '{inner}' in '{s}' must use square brackets"
                ));
            }
            let k = vkey(parse_numeric_version(v)?);
            (k, k)
        }
        2 => {
            let lo_s = parts[0].trim();
            let hi_s = parts[1].trim();
            let lo = if lo_s.is_empty() {
                0
            } else {
                let k = vkey(parse_numeric_version(lo_s)?);
                if open_incl {
                    k
                } else {
                    k + 1
                }
            };
            let hi = if hi_s.is_empty() {
                KEY_MAX
            } else {
                let k = vkey(parse_numeric_version(hi_s)?);
                if close_incl {
                    k
                } else {
                    k.saturating_sub(1)
                }
            };
            (lo, hi)
        }
        _ => {
            return Err(format!(
                "interval '{inner}' in '{s}' has more than two bounds"
            ))
        }
    };
    Ok((interval.0, interval.1, close + 1))
}

/// Maven / NeoForge range: a sequence of bracketed intervals (plus bare
/// exact pins) separated by commas.
fn parse_maven_range(s: &str) -> Result<Vec<(u64, u64)>, String> {
    let bytes = s.as_bytes();
    let mut intervals = Vec::new();
    let mut i = 0usize;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            if intervals.is_empty() {
                return Err("empty version range".to_string());
            }
            return Err(format!("trailing separator in '{s}'"));
        }
        if matches!(bytes[i], b'[' | b'(') {
            let (lo, hi, next) = parse_maven_member(s, i)?;
            intervals.push((lo, hi));
            i = next;
        } else {
            // Bare member inside the list: exact pin (`(,1.0],1.2` style).
            let end = s[i..].find(',').map(|o| i + o).unwrap_or(bytes.len());
            let tok = s[i..end].trim();
            if tok.is_empty() {
                return Err(format!("empty list member in '{s}'"));
            }
            let k = vkey(parse_numeric_version(tok)?);
            intervals.push((k, k));
            i = end;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] != b',' {
            return Err(format!("expected ',' between intervals in '{s}'"));
        }
        i += 1;
    }
    Ok(intervals)
}

/// SemVer range: `||`-separated union arms, each an AND of space-separated
/// comparator/tilde/caret/wildcard/bare predicates.
fn parse_semver_range(s: &str) -> Result<Vec<(u64, u64)>, String> {
    let mut intervals = Vec::new();
    for arm in s.split("||") {
        let arm = arm.trim();
        if arm.is_empty() {
            return Err(format!("empty '||' arm in '{s}'"));
        }
        let mut lo = 0u64;
        let mut hi = KEY_MAX;
        let mut tokens = arm.split_whitespace().peekable();
        while let Some(tok) = tokens.next() {
            // Tolerate `>= 1.21.1`: a lone operator binds the next token.
            let tok = if matches!(tok, "~" | "^" | ">=" | "<=" | ">" | "<" | "=")
                && tokens.peek().is_some()
            {
                format!("{}{}", tok, tokens.next().unwrap())
            } else {
                tok.to_string()
            };
            let (a, b) = predicate_interval(&tok)?;
            lo = lo.max(a);
            hi = hi.min(b);
        }
        intervals.push((lo, hi));
    }
    Ok(intervals)
}

/// Error from parsing a declared version range. Parsing is fail-closed: any
/// malformed input produces this error rather than a panic or a wrong match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRangeError(String);

impl VersionRangeError {
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VersionRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VersionRangeError {}

/// A parsed Minecraft version range: a union of inclusive version intervals
/// covering both the NeoForge/Forge Maven syntax and the Fabric SemVer syntax.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRange {
    declared: String,
    intervals: Vec<(u64, u64)>,
}

impl VersionRange {
    /// Parse `declared` as a Maven interval range when it opens with `[`/`(`,
    /// otherwise as a Fabric SemVer range. Empty or malformed input is an
    /// error, never a panic.
    pub fn parse(declared: &str) -> Result<Self, VersionRangeError> {
        let trimmed = declared.trim();
        if trimmed.is_empty() {
            return Err(VersionRangeError("empty version range".to_string()));
        }
        let intervals = if trimmed.starts_with('[') || trimmed.starts_with('(') {
            parse_maven_range(trimmed)
        } else {
            parse_semver_range(trimmed)
        }
        .map_err(VersionRangeError)?;
        Ok(Self {
            declared: trimmed.to_string(),
            intervals,
        })
    }

    /// The declared range string as passed to [`Self::parse`], trimmed.
    pub fn declared(&self) -> &str {
        &self.declared
    }

    /// Whether `v` falls inside the union of declared intervals.
    pub fn contains(&self, v: McVersion) -> bool {
        let k = vkey(v);
        self.intervals.iter().any(|&(lo, hi)| lo <= k && k <= hi)
    }

    /// Whether the range admits at least one version on `v`'s `major.minor`
    /// line — distinguishes a minor mismatch (same `1.21.x` line, wrong patch
    /// window) from a major one (mod built for another release line).
    fn overlaps_minor_band(&self, v: McVersion) -> bool {
        let lo = vkey(McVersion::new(v.major, v.minor, 0));
        let hi = vkey(McVersion::new(v.major, v.minor, u16::MAX));
        self.intervals.iter().any(|&(a, b)| a <= hi && b >= lo)
    }
}

/// Verdict from checking a mod's declared Minecraft range against the version
/// the server is actually running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionCompatVerdict {
    /// `target` satisfies the declared range.
    Compatible,
    /// `target` is outside the declared range but shares its `major.minor`
    /// release line (e.g. a `>=1.21.3` mod on a `1.21.1` server) — usually a
    /// "wrong patch" problem with a clear upgrade/downgrade path.
    MinorMismatch { target: McVersion, declared: String },
    /// `target` is on a different `major.minor` release line than every
    /// version the mod declares (e.g. a `1.20.1` mod on a `1.21.1` server).
    MajorMismatch { target: McVersion, declared: String },
    /// The declared range could not be parsed in either supported syntax.
    InvalidRange { declared: String, reason: String },
}

/// Check one mod's declared `mc_version_range` against the server's target
/// version. Both Maven and SemVer syntaxes are accepted; empty/malformed
/// declarations produce [`VersionCompatVerdict::InvalidRange`].
pub fn check_mod_mc_version(target: McVersion, declared_range: &str) -> VersionCompatVerdict {
    let declared = declared_range.trim().to_string();
    match VersionRange::parse(declared_range) {
        Err(e) => VersionCompatVerdict::InvalidRange {
            declared,
            reason: e.reason().to_string(),
        },
        Ok(range) => {
            if range.contains(target) {
                VersionCompatVerdict::Compatible
            } else if range.overlaps_minor_band(target) {
                VersionCompatVerdict::MinorMismatch { target, declared }
            } else {
                VersionCompatVerdict::MajorMismatch { target, declared }
            }
        }
    }
}

/// `pack_format` datapack/data format number for `v` (`pack.mcmeta`
/// `pack.pack_format`), covering the 1.20.4-1.21.5 line megu ships
/// profiles for plus latest vanilla 26.3. `None` for versions outside the
/// table.
pub fn data_pack_format_for(v: McVersion) -> Option<u32> {
    Some(match v {
        McVersion::V1_20_4 => 26,
        McVersion::V1_20_6 => 41,
        McVersion::V1_21_0 => 48,
        McVersion::V1_21_1 => TargetVersion::V1_21_1.data_pack_format().0,
        McVersion::V1_21_3 => 57,
        McVersion::V1_21_4 => 61,
        McVersion::V1_21_5 => 71,
        McVersion::V26_3 => TargetVersion::V26_3.data_pack_format().0,
        _ => return None,
    })
}

/// `(major, minor)` data pack format tuple for `v` — the shape `pack.mcmeta`
/// `min_format`/`max_format` use on the 26.x line. The two canonical targets
/// report the tuple `megu-core` pins: `1.21.1` -> `(48, 0)`, `26.3` ->
/// `(121, 0)`. Other scalar-`pack_format` versions report `(format, 0)`.
/// `None` for versions outside the table.
pub fn data_pack_format_tuple_for(v: McVersion) -> Option<(u32, u32)> {
    match v {
        McVersion::V1_21_1 => Some(TargetVersion::V1_21_1.data_pack_format()),
        McVersion::V26_3 => Some(TargetVersion::V26_3.data_pack_format()),
        _ => data_pack_format_for(v).map(|major| (major, 0)),
    }
}

/// Mojang `DataVersion` for `v` (the number stored in world/player saves),
/// covering the same 1.20.4-1.21.5 + 26.3 table as [`data_pack_format_for`].
pub fn data_version_for(v: McVersion) -> Option<i32> {
    Some(match v {
        McVersion::V1_20_4 => 3700,
        McVersion::V1_20_6 => 3839,
        McVersion::V1_21_0 => 3953,
        McVersion::V1_21_1 => TargetVersion::V1_21_1.data_version(),
        McVersion::V1_21_3 => 4082,
        McVersion::V1_21_4 => 4189,
        McVersion::V1_21_5 => 4325,
        McVersion::V26_3 => TargetVersion::V26_3.data_version(),
        _ => return None,
    })
}

/// Whether a datapack declaring `pack_format` (and optionally a
/// `supported_formats` `[min, max]` span, as `pack.mcmeta` allows since
/// 1.20.2) is loadable on a server running `target`.
///
/// The pack is compatible when the target's own `pack_format` equals the
/// declared one or falls inside the declared `supported_min..=supported_max`
/// span. `false` when `target` has no known `pack_format` (fail closed).
pub fn check_pack_format(
    target: McVersion,
    pack_format: u32,
    supported_min: Option<u32>,
    supported_max: Option<u32>,
) -> bool {
    let Some(target_pf) = data_pack_format_for(target) else {
        return false;
    };
    if target_pf == pack_format {
        return true;
    }
    if supported_min.is_none() && supported_max.is_none() {
        return false;
    }
    let lo = supported_min.unwrap_or(0);
    let hi = supported_max.unwrap_or(u32::MAX);
    lo <= target_pf && target_pf <= hi
}

/// Whether a datapack declaring `(major, minor)` format tuples — the
/// `pack.mcmeta` `min_format`/`max_format` schema the 26.x line uses — is
/// loadable on a server running `target`.
///
/// Same contract as [`check_pack_format`]: compatible when the target's own
/// format tuple equals `format` or falls inside the declared
/// `min_fmt..=max_fmt` span. Tuples compare lexicographically, so
/// `(121, 0) < (121, 1)`. `false` when `target` has no known format (fail
/// closed).
pub fn check_pack_format_tuple(
    target: McVersion,
    format: (u32, u32),
    min_fmt: Option<(u32, u32)>,
    max_fmt: Option<(u32, u32)>,
) -> bool {
    let Some(target_fmt) = data_pack_format_tuple_for(target) else {
        return false;
    };
    if target_fmt == format {
        return true;
    }
    if min_fmt.is_none() && max_fmt.is_none() {
        return false;
    }
    let lo = min_fmt.unwrap_or((0, 0));
    let hi = max_fmt.unwrap_or((u32::MAX, u32::MAX));
    lo <= target_fmt && target_fmt <= hi
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u16, minor: u16, patch: u16) -> McVersion {
        McVersion::new(major, minor, patch)
    }

    fn range(s: &str) -> VersionRange {
        VersionRange::parse(s).unwrap_or_else(|e| panic!("'{s}' failed to parse: {e}"))
    }

    #[test]
    fn mc_version_parse_and_ordering() {
        assert_eq!(McVersion::parse("1.21.1"), Some(v(1, 21, 1)));
        assert_eq!(McVersion::parse("1.21"), Some(v(1, 21, 0)));
        assert_eq!(McVersion::parse("1"), Some(v(1, 0, 0)));
        assert_eq!(McVersion::parse(""), None);
        assert_eq!(McVersion::parse("1.21.1.4"), None);
        assert_eq!(McVersion::parse("1.x"), None);
        assert_eq!(McVersion::parse("abc"), None);
        assert_eq!(McVersion::parse("1.65536.0"), None);
        assert!(v(1, 20, 6) < v(1, 21, 0) && v(1, 21, 1) < v(1, 21, 3));
        assert_eq!(McVersion::V1_21_1.to_string(), "1.21.1");
        assert_eq!(McVersion::parse("1.21").unwrap().to_string(), "1.21.0");
        // Year-based 26.x versions parse and order past the 1.x line.
        assert_eq!(McVersion::parse("26.3"), Some(McVersion::V26_3));
        assert_eq!(McVersion::parse("26.3.1"), Some(v(26, 3, 1)));
        assert_eq!(McVersion::V26_3.to_string(), "26.3.0");
        assert!(v(1, 21, 5) < v(26, 0, 0) && v(26, 3, 0) < v(26, 4, 0));
    }

    #[test]
    fn pack_format_and_data_version_table() {
        assert_eq!(data_pack_format_for(McVersion::V1_20_4), Some(26));
        assert_eq!(data_version_for(McVersion::V1_20_4), Some(3700));
        assert_eq!(data_pack_format_for(McVersion::V1_20_6), Some(41));
        assert_eq!(data_version_for(McVersion::V1_20_6), Some(3839));
        assert_eq!(data_pack_format_for(McVersion::V1_21_0), Some(48));
        assert_eq!(data_version_for(McVersion::V1_21_0), Some(3953));
        assert_eq!(data_pack_format_for(McVersion::V1_21_1), Some(48));
        assert_eq!(data_version_for(McVersion::V1_21_1), Some(3955));
        assert_eq!(data_pack_format_for(McVersion::V1_21_3), Some(57));
        assert_eq!(data_version_for(McVersion::V1_21_3), Some(4082));
        assert_eq!(data_pack_format_for(McVersion::V1_21_4), Some(61));
        assert_eq!(data_version_for(McVersion::V1_21_4), Some(4189));
        assert_eq!(data_pack_format_for(McVersion::V1_21_5), Some(71));
        assert_eq!(data_version_for(McVersion::V1_21_5), Some(4325));
        assert_eq!(data_pack_format_for(McVersion::V26_3), Some(121));
        assert_eq!(data_version_for(McVersion::V26_3), Some(5023));
        assert_eq!(data_pack_format_for(v(1, 19, 4)), None);
        assert_eq!(data_version_for(v(2, 0, 0)), None);
        // The (major, minor) tuple mirrors: legacy versions get a zero minor.
        assert_eq!(data_pack_format_tuple_for(McVersion::V1_21_1), Some((48, 0)));
        assert_eq!(data_pack_format_tuple_for(McVersion::V26_3), Some((121, 0)));
        assert_eq!(data_pack_format_tuple_for(v(1, 19, 4)), None);
    }

    #[test]
    fn check_pack_format_across_supported_targets() {
        // Exact-format pack built for 1.21.1 (pack_format 48).
        assert!(check_pack_format(McVersion::V1_21_1, 48, None, None));
        assert!(!check_pack_format(McVersion::V1_21_4, 48, None, None));
        assert!(!check_pack_format(McVersion::V1_20_4, 48, None, None));
        // Pack declaring a supported span covering 1.21.0-1.21.5.
        assert!(check_pack_format(
            McVersion::V1_21_1,
            48,
            Some(48),
            Some(71)
        ));
        assert!(check_pack_format(
            McVersion::V1_21_4,
            48,
            Some(48),
            Some(71)
        ));
        assert!(!check_pack_format(
            McVersion::V1_20_6,
            48,
            Some(48),
            Some(71)
        ));
        // Min-only / max-only bounds.
        assert!(check_pack_format(McVersion::V1_21_5, 48, Some(48), None));
        assert!(check_pack_format(McVersion::V1_20_4, 48, None, Some(71)));
        // Unknown target format fails closed.
        assert!(!check_pack_format(v(1, 19, 4), 26, Some(1), Some(99)));
    }

    #[test]
    fn check_pack_format_tuple_covers_26_3_min_max_format() {
        // A 26.3 datapack declaring min_format == max_format == [121, 0].
        assert!(check_pack_format_tuple(
            McVersion::V26_3,
            (121, 0),
            Some((121, 0)),
            Some((121, 0))
        ));
        assert!(!check_pack_format_tuple(
            McVersion::V1_21_1,
            (121, 0),
            Some((121, 0)),
            Some((121, 0))
        ));
        // A pack spanning both targets: min (48,0) - max (121,0).
        assert!(check_pack_format_tuple(
            McVersion::V1_21_1,
            (48, 0),
            Some((48, 0)),
            Some((121, 0))
        ));
        assert!(check_pack_format_tuple(
            McVersion::V26_3,
            (48, 0),
            Some((48, 0)),
            Some((121, 0))
        ));
        // Tuples order lexicographically: (121, 0) < (121, 1).
        assert!(!check_pack_format_tuple(
            McVersion::V26_3,
            (121, 1),
            Some((121, 1)),
            None
        ));
        assert!(check_pack_format_tuple(
            McVersion::V26_3,
            (121, 0),
            Some((120, 0)),
            None
        ));
        // Exact format match without a declared span.
        assert!(check_pack_format_tuple(McVersion::V26_3, (121, 0), None, None));
        assert!(!check_pack_format_tuple(McVersion::V1_21_4, (121, 0), None, None));
        // Unknown target format fails closed.
        assert!(!check_pack_format_tuple(v(9, 9, 9), (121, 0), Some((0, 0)), None));
    }

    #[test]
    fn year_based_26_x_versions_parse_and_range() {
        // `[26.3]`, `[26.3,26.4)`, `>=26.3`, `26.3.x` all behave like any
        // other numeric bound in both supported syntaxes.
        assert!(range("[26.3]").contains(McVersion::V26_3));
        assert!(!range("[26.3]").contains(v(26, 3, 1)));
        assert!(range("[26.3,26.4)").contains(v(26, 3, 5)));
        assert!(!range("[26.3,26.4)").contains(v(26, 4, 0)));
        assert!(range(">=26.3").contains(v(26, 9, 0)));
        assert!(range("26.3.x").contains(v(26, 3, 7)));
        assert!(!range("26.3.x").contains(v(26, 4, 0)));
        // A 1.21.1-only mod on a 26.3 server is a different release line.
        assert_eq!(
            check_mod_mc_version(McVersion::V26_3, "[1.21.1]"),
            VersionCompatVerdict::MajorMismatch {
                target: McVersion::V26_3,
                declared: "[1.21.1]".to_string()
            }
        );
    }

    #[test]
    fn maven_exact_pins_and_intervals() {
        // Exact pin `[v]` matches only v.
        let pin = range("[1.21.1]");
        assert!(pin.contains(McVersion::V1_21_1));
        assert!(!pin.contains(McVersion::V1_21_0));
        assert!(!pin.contains(McVersion::V1_21_3));

        let r = range("[1.21.1,1.21.5)");
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(McVersion::V1_21_3));
        assert!(r.contains(McVersion::V1_21_4));
        assert!(!r.contains(McVersion::V1_21_5));
        assert!(!r.contains(McVersion::V1_21_0));
        assert!(!r.contains(McVersion::V1_20_6));

        let r = range("(1.21.1,1.21.4]");
        assert!(!r.contains(McVersion::V1_21_1));
        assert!(r.contains(McVersion::V1_21_3));
        assert!(r.contains(McVersion::V1_21_4));
        assert!(!r.contains(McVersion::V1_21_5));

        // Unbounded both ways.
        assert!(range("[1.21.1,)").contains(McVersion::V1_21_5));
        assert!(!range("[1.21.1,)").contains(McVersion::V1_21_0));
        assert!(range("(,1.21.5)").contains(McVersion::V1_20_4));
        assert!(!range("(,1.21.5)").contains(McVersion::V1_21_5));
        assert!(range("[,1.21.5]").contains(McVersion::V1_21_5));

        // Two-component bounds: "1.21" is 1.21.0.
        let r = range("(1.21,1.22]");
        assert!(!r.contains(McVersion::V1_21_0));
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(v(1, 22, 0)));
        assert!(!r.contains(v(1, 22, 1)));

        // Maven union `(,1.20.5],[1.21.1,)`: a gap across the 1.20.6-1.21.0 line.
        let r = range("(,1.20.5],[1.21.1,)");
        assert!(r.contains(v(1, 20, 4)));
        assert!(r.contains(v(1, 20, 5)));
        assert!(!r.contains(v(1, 20, 6)));
        assert!(!r.contains(McVersion::V1_21_0));
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(McVersion::V1_21_5));

        // Bare exact version in the Maven ecosystem.
        let r = range("1.21.1");
        assert!(r.contains(McVersion::V1_21_1));
        assert!(!r.contains(McVersion::V1_21_3));
    }

    #[test]
    fn semver_comparators_tilde_caret_wildcards() {
        let r = range(">=1.21.1");
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(v(2, 0, 0)));
        assert!(!r.contains(McVersion::V1_21_0));

        let r = range(">=1.21.1 <1.21.5");
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(McVersion::V1_21_4));
        assert!(!r.contains(McVersion::V1_21_5));
        assert!(!r.contains(McVersion::V1_21_0));

        // `~1.21.1` == `>=1.21.1 <1.22.0`.
        let r = range("~1.21.1");
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(v(1, 21, 99)));
        assert!(!r.contains(v(1, 22, 0)));
        assert!(!r.contains(McVersion::V1_21_0));

        // `^1.21.1` == `>=1.21.1 <2.0.0`; `^0.x` keeps the minor band.
        let r = range("^1.21.1");
        assert!(r.contains(v(1, 99, 0)));
        assert!(!r.contains(v(2, 0, 0)));
        let r = range("^0.2.3");
        assert!(r.contains(v(0, 2, 9)));
        assert!(!r.contains(v(0, 3, 0)));
        assert!(!r.contains(v(0, 2, 2)));

        // Wildcards: `1.21.x` == `>=1.21.0 <1.22.0`; `1.x` == `>=1.0.0 <2.0.0`.
        let r = range("1.21.x");
        assert!(r.contains(McVersion::V1_21_0));
        assert!(r.contains(v(1, 21, 99)));
        assert!(!r.contains(v(1, 22, 0)));
        assert!(!r.contains(v(1, 20, 9)));
        let r = range("1.21.*");
        assert!(r.contains(McVersion::V1_21_4));
        assert!(range("1.x").contains(v(1, 2, 3)));
        assert!(!range("1.x").contains(v(2, 0, 0)));
        assert!(range("*").contains(v(9, 9, 9)));

        // `<`/`>`/`<=` on partial versions use the band edges.
        let r = range("<1.22");
        assert!(r.contains(v(1, 21, 9)));
        assert!(!r.contains(v(1, 22, 0)));
        // npm band semantics: a partial version in a comparator covers its
        // whole band, so `>1.21` means "after the 1.21.x line" (>=1.22.0).
        let r = range(">1.21");
        assert!(!r.contains(McVersion::V1_21_1));
        assert!(r.contains(v(1, 22, 0)));
        assert!(!r.contains(McVersion::V1_21_0));
        // `>1.21.0` is the spelling for "newer than 1.21.0" literally.
        assert!(range(">1.21.0").contains(McVersion::V1_21_1));
        let r = range("<=1.21.x");
        assert!(r.contains(v(1, 21, 99)));
        assert!(!r.contains(v(1, 22, 0)));

        // `=` pin and OR union.
        assert!(range("=1.21.1").contains(McVersion::V1_21_1));
        assert!(!range("=1.21.1").contains(McVersion::V1_21_3));
        let r = range("1.21.1 || 1.21.4");
        assert!(r.contains(McVersion::V1_21_1));
        assert!(r.contains(McVersion::V1_21_4));
        assert!(!r.contains(McVersion::V1_21_3));
        let r = range("1.20.x || >=1.21.1 <1.22");
        assert!(r.contains(v(1, 20, 1)));
        assert!(r.contains(McVersion::V1_21_5));
        assert!(!r.contains(McVersion::V1_21_0));
        assert!(!r.contains(v(1, 22, 0)));

        // Partial bare `1.21` covers the whole 1.21.x band.
        let r = range("1.21");
        assert!(r.contains(McVersion::V1_21_0));
        assert!(r.contains(v(1, 21, 9)));
        assert!(!r.contains(v(1, 22, 0)));
    }

    #[test]
    fn malformed_ranges_fail_closed_never_panic() {
        for bad in [
            "",
            "   ",
            "[",
            "(",
            "]",
            "[1.21.1",
            "[1.21.1,",
            "[1.21.1,1.21.5,1.21.9)",
            "[]",
            "(1.21.1)",
            "[1.21.1] extra",
            ">=",
            "~",
            "1.x.2",
            "1.2.3.4",
            "abc",
            ">=abc",
            "1.21.1 ||",
            "|| 1.21.1",
            "1.21.1 1.21.4 junk",
            "1.21.1-rc1",
            "<0.0.0 garbage",
            ",",
            "[1.21.1],garbage!",
        ] {
            assert!(
                VersionRange::parse(bad).is_err(),
                "'{bad}' must fail closed"
            );
        }
        // `<0.0.0` parses but matches nothing.
        assert!(!range("<0.0.0").contains(v(0, 0, 0)));
    }

    #[test]
    fn mod_version_verdict_distinguishes_minor_and_major() {
        let target = McVersion::V1_21_1;
        assert_eq!(
            check_mod_mc_version(target, "[1.21.1,1.22)"),
            VersionCompatVerdict::Compatible
        );
        assert_eq!(
            check_mod_mc_version(target, ">=1.20 <1.22"),
            VersionCompatVerdict::Compatible
        );
        // Same 1.21.x line, outside the declared window -> minor.
        assert_eq!(
            check_mod_mc_version(target, ">=1.21.3"),
            VersionCompatVerdict::MinorMismatch {
                target,
                declared: ">=1.21.3".to_string()
            }
        );
        assert_eq!(
            check_mod_mc_version(target, "[1.21.3,1.21.5)"),
            VersionCompatVerdict::MinorMismatch {
                target,
                declared: "[1.21.3,1.21.5)".to_string()
            }
        );
        // A different release line entirely -> major.
        assert_eq!(
            check_mod_mc_version(target, "[1.20.1,1.20.2)"),
            VersionCompatVerdict::MajorMismatch {
                target,
                declared: "[1.20.1,1.20.2)".to_string()
            }
        );
        assert_eq!(
            check_mod_mc_version(target, "1.20.x"),
            VersionCompatVerdict::MajorMismatch {
                target,
                declared: "1.20.x".to_string()
            }
        );
        // A union spanning both lines: still contains a 1.21.x version, so a
        // miss on 1.21.1 stays a minor mismatch.
        let r = range("1.20.x || >=1.21.3 <1.22");
        assert_eq!(
            check_mod_mc_version(target, r.declared()),
            VersionCompatVerdict::MinorMismatch {
                target,
                declared: r.declared().to_string()
            }
        );
        // Malformed input reports InvalidRange with the reason preserved.
        match check_mod_mc_version(target, "bogus!!") {
            VersionCompatVerdict::InvalidRange { declared, reason } => {
                assert_eq!(declared, "bogus!!");
                assert!(!reason.is_empty());
            }
            other => panic!("expected InvalidRange, got {other:?}"),
        }
        match check_mod_mc_version(target, "  ") {
            VersionCompatVerdict::InvalidRange { .. } => {}
            other => panic!("expected InvalidRange for blank range, got {other:?}"),
        }
    }

    #[test]
    fn target_rows_come_from_megu_core_meta() {
        // The two canonical target rows of the pack-format / DataVersion
        // table must equal `TargetVersion::meta()` exactly — this is what
        // makes the core crate the single source of truth.
        for target in TargetVersion::ALL {
            let meta = target.meta();
            let v = McVersion::from(target);
            assert_eq!(McVersion::parse(meta.id), Some(v));
            assert_eq!(data_pack_format_for(v), Some(meta.data_pack_format.0));
            assert_eq!(data_pack_format_tuple_for(v), Some(meta.data_pack_format));
            assert_eq!(data_version_for(v), Some(meta.data_version));
        }
    }

    #[test]
    fn target_version_mc_version_roundtrip_is_total_over_all() {
        // `TargetVersion -> McVersion -> TargetVersion` must be the identity
        // for every entry in the canonical target set.
        for target in TargetVersion::ALL {
            let v = McVersion::from(target);
            assert_eq!(TargetVersion::try_from(v), Ok(target));
        }
        assert_eq!(McVersion::from(TargetVersion::V1_21_1), McVersion::V1_21_1);
        assert_eq!(McVersion::from(TargetVersion::V26_3), McVersion::V26_3);
    }

    #[test]
    fn non_target_mc_versions_fail_try_from_cleanly() {
        for v in [
            McVersion::V1_20_4,
            McVersion::V1_20_6,
            McVersion::V1_21_0,
            McVersion::V1_21_3,
            McVersion::V1_21_4,
            McVersion::V1_21_5,
            v(1, 21, 2),
            v(26, 3, 1),
        ] {
            assert_eq!(
                TargetVersion::try_from(v),
                Err(NoTargetVersion(v)),
                "{v} must not map to a canonical target"
            );
        }
        // The error carries the offending version and formats usefully.
        let err = NoTargetVersion(v(9, 9, 9));
        assert_eq!(err.0, v(9, 9, 9));
        assert_eq!(
            err.to_string(),
            "9.9.9 is not a canonical megu target version"
        );
    }
}
