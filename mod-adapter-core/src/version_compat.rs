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
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() > 3 {
            return None;
        }
        let mut n = [0u16; 3];
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            n[i] = part.parse::<u16>().ok()?;
        }
        Some(Self::new(n[0], n[1], n[2]))
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

/// One semver prerelease identifier. Numeric identifiers order before
/// alphanumeric identifiers exactly like SemVer; text is ASCII-lowercased so
/// Maven qualifiers (`-SNAPSHOT`, `-RC.1`) compare case-insensitively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionIdentifier {
    Numeric(u64),
    Text(String),
}

impl Ord for VersionIdentifier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use VersionIdentifier::{Numeric, Text};
        match (self, other) {
            (Numeric(a), Numeric(b)) => a.cmp(b),
            // SemVer 11.4.4: numeric identifiers have lower precedence than
            // alphanumeric identifiers.
            (Numeric(_), Text(_)) => std::cmp::Ordering::Less,
            (Text(_), Numeric(_)) => std::cmp::Ordering::Greater,
            (Text(a), Text(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for VersionIdentifier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A general mod-version value: one to three dotted numeric components plus
/// an optional SemVer/Maven prerelease qualifier chain. Build metadata
/// (`+build`) is accepted and ignored for precedence, matching SemVer.
///
/// Precedence covers the qualifier families real Forge/NeoForge and Fabric
/// artifacts publish: `1.0.0-alpha.1 < 1.0.0-alpha.2 < 1.0.0-beta.1 <
/// 1.0.0-rc.1 < 1.0.0-SNAPSHOT < 1.0.0` for the usual spellings. Numeric
/// components are normalized to three positions, so `1.2 == 1.2.0` while
/// `1.2-alpha < 1.2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModVersion {
    numeric: [u64; 3],
    prerelease: Vec<VersionIdentifier>,
}

impl ModVersion {
    /// Parse a concrete version (`1.21.1`, `2.0.0-beta.2`, `1.0-SNAPSHOT`).
    /// Returns `None` for empty input, wildcards, non-numeric core components,
    /// more than three core components, empty prerelease identifiers, or bytes
    /// outside the SemVer identifier alphabet.
    pub fn parse(input: &str) -> Option<Self> {
        parse_version_value(input.trim()).ok()
    }

    /// Whether this version is a prerelease (`-alpha`, `-SNAPSHOT`, …).
    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }

    /// Numeric triple with omitted trailing components normalized to zero.
    pub fn numeric_components(&self) -> [u64; 3] {
        self.numeric
    }

    /// Dot-separated prerelease identifiers in their normalized form.
    pub fn prerelease(&self) -> &[VersionIdentifier] {
        &self.prerelease
    }
}

impl From<McVersion> for ModVersion {
    fn from(v: McVersion) -> Self {
        Self {
            numeric: [v.major as u64, v.minor as u64, v.patch as u64],
            prerelease: Vec::new(),
        }
    }
}

impl Ord for ModVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.numeric.cmp(&other.numeric).then_with(|| {
            match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                (true, true) => std::cmp::Ordering::Equal,
                // A release sorts after every prerelease of the same numeric
                // triple (SemVer 11.3; Maven qualifiers behave the same way).
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => {
                    let common = self.prerelease.len().min(other.prerelease.len());
                    for i in 0..common {
                        let ord = self.prerelease[i].cmp(&other.prerelease[i]);
                        if ord != std::cmp::Ordering::Equal {
                            return ord;
                        }
                    }
                    self.prerelease.len().cmp(&other.prerelease.len())
                }
            }
        })
    }
}

impl PartialOrd for ModVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ModVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.numeric[0])?;
        write!(f, ".{}", self.numeric[1])?;
        write!(f, ".{}", self.numeric[2])?;
        if !self.prerelease.is_empty() {
            f.write_str("-")?;
            for (i, ident) in self.prerelease.iter().enumerate() {
                if i > 0 {
                    f.write_str(".")?;
                }
                match ident {
                    VersionIdentifier::Numeric(n) => write!(f, "{n}")?,
                    VersionIdentifier::Text(t) => f.write_str(t)?,
                }
            }
        }
        Ok(())
    }
}

fn parse_version_value(s: &str) -> Result<ModVersion, String> {
    if s.is_empty() {
        return Err("empty version".to_string());
    }
    if s.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(format!("version '{s}' contains whitespace"));
    }
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    // Build metadata is valid but never participates in precedence.
    let without_build = s.split('+').next().unwrap_or("");
    if without_build.is_empty() {
        return Err(format!("version '{s}' has an empty core"));
    }
    let (core, prerelease_text) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3 {
        return Err(format!("version '{s}' has more than three components"));
    }
    let mut numeric = [0u64; 3];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("version '{s}' has non-numeric component '{part}'"));
        }
        numeric[i] = part
            .parse::<u64>()
            .map_err(|_| format!("version '{s}' component '{part}' out of range"))?;
    }
    let mut prerelease = Vec::new();
    if let Some(pre) = prerelease_text {
        if pre.is_empty() {
            return Err(format!("version '{s}' has an empty prerelease"));
        }
        for ident in pre.split('.') {
            if ident.is_empty() {
                return Err(format!("version '{s}' has an empty prerelease identifier"));
            }
            if !ident
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(format!(
                    "version '{s}' has invalid prerelease identifier '{ident}'"
                ));
            }
            if ident.bytes().all(|b| b.is_ascii_digit()) {
                prerelease.push(VersionIdentifier::Numeric(ident.parse::<u64>().map_err(
                    |_| format!("version '{s}' prerelease identifier '{ident}' out of range"),
                )?));
            } else {
                // Split simple Maven spellings such as `alpha1`/`rc2` into
                // text+number so they order consistently with `alpha.1`/`rc.2`.
                let digit_at = ident
                    .bytes()
                    .position(|b| b.is_ascii_digit())
                    .unwrap_or(ident.len());
                if digit_at > 0
                    && digit_at < ident.len()
                    && ident[..digit_at]
                        .bytes()
                        .all(|b| b.is_ascii_alphabetic() || b == b'-')
                    && ident[digit_at..]
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                {
                    prerelease.push(VersionIdentifier::Text(
                        ident[..digit_at].to_ascii_lowercase(),
                    ));
                    prerelease.push(VersionIdentifier::Numeric(
                        ident[digit_at..].parse::<u64>().map_err(|_| {
                            format!("version '{s}' prerelease identifier '{ident}' out of range")
                        })?,
                    ));
                } else {
                    prerelease.push(VersionIdentifier::Text(ident.to_ascii_lowercase()));
                }
            }
        }
    }
    Ok(ModVersion {
        numeric,
        prerelease,
    })
}

/// A version interval endpoint. `inclusive` is the Maven `[`/`]` distinction
/// and the semantic difference between `<`/`<=` in SemVer ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionEndpoint {
    version: ModVersion,
    inclusive: bool,
}

/// One continuous version interval; the full range is a union of these.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VersionInterval {
    lower: Option<VersionEndpoint>,
    upper: Option<VersionEndpoint>,
}

impl VersionInterval {
    fn unbounded() -> Self {
        Self {
            lower: None,
            upper: None,
        }
    }

    fn exact(version: ModVersion) -> Self {
        Self {
            lower: Some(VersionEndpoint {
                version: version.clone(),
                inclusive: true,
            }),
            upper: Some(VersionEndpoint {
                version,
                inclusive: true,
            }),
        }
    }

    fn contains(&self, version: &ModVersion) -> bool {
        if let Some(lower) = &self.lower {
            let ord = version.cmp(&lower.version);
            if ord == std::cmp::Ordering::Less
                || (ord == std::cmp::Ordering::Equal && !lower.inclusive)
            {
                return false;
            }
        }
        if let Some(upper) = &self.upper {
            let ord = version.cmp(&upper.version);
            if ord == std::cmp::Ordering::Greater
                || (ord == std::cmp::Ordering::Equal && !upper.inclusive)
            {
                return false;
            }
        }
        true
    }
}

fn endpoint_more_restrictive_lower(
    a: Option<VersionEndpoint>,
    b: Option<VersionEndpoint>,
) -> Option<VersionEndpoint> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(match a.version.cmp(&b.version) {
            std::cmp::Ordering::Greater => a,
            std::cmp::Ordering::Less => b,
            std::cmp::Ordering::Equal => VersionEndpoint {
                version: a.version,
                inclusive: a.inclusive && b.inclusive,
            },
        }),
    }
}

fn endpoint_more_restrictive_upper(
    a: Option<VersionEndpoint>,
    b: Option<VersionEndpoint>,
) -> Option<VersionEndpoint> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(match a.version.cmp(&b.version) {
            std::cmp::Ordering::Less => a,
            std::cmp::Ordering::Greater => b,
            std::cmp::Ordering::Equal => VersionEndpoint {
                version: a.version,
                inclusive: a.inclusive && b.inclusive,
            },
        }),
    }
}

fn intersect_intervals(a: &VersionInterval, b: &VersionInterval) -> VersionInterval {
    VersionInterval {
        lower: endpoint_more_restrictive_lower(a.lower.clone(), b.lower.clone()),
        upper: endpoint_more_restrictive_upper(a.upper.clone(), b.upper.clone()),
    }
}

fn upper_reaches_lower(upper: &VersionEndpoint, lower: &VersionEndpoint) -> bool {
    match upper.version.cmp(&lower.version) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => upper.inclusive && lower.inclusive,
    }
}

fn intervals_overlap(a: &VersionInterval, b: &VersionInterval) -> bool {
    if let (Some(au), Some(bl)) = (&a.upper, &b.lower) {
        if !upper_reaches_lower(au, bl) {
            return false;
        }
    }
    if let (Some(bu), Some(al)) = (&b.upper, &a.lower) {
        if !upper_reaches_lower(bu, al) {
            return false;
        }
    }
    true
}

/// A SemVer-ish version *spec*: dotted numeric components where a trailing
/// `x`/`X`/`*` wildcard (or an omitted component) widens the match to a whole
/// band. `specified` counts the leading numeric components: `1.21.x` -> 2,
/// `1.x`/`1` -> 1, `*` -> 0.
struct VersionSpec {
    version: ModVersion,
    specified: usize,
}

fn parse_spec(s: &str) -> Result<VersionSpec, String> {
    let original = s.trim();
    let s = original.strip_prefix(['v', 'V']).unwrap_or(original);
    let s = s.split('+').next().unwrap_or("");
    if s.is_empty() {
        return Err("empty version spec".to_string());
    }
    let (core, prerelease_text) = match s.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (s, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3 {
        return Err(format!("version spec '{original}' has more than three components"));
    }
    let mut numeric = [0u64; 3];
    let mut specified = parts.len();
    let mut saw_wildcard = false;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(format!("version spec '{original}' has an empty component"));
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
                "version spec '{original}' has a numeric component after a wildcard"
            ));
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "version spec '{original}' has non-numeric component '{part}'"
            ));
        }
        numeric[i] = part
            .parse()
            .map_err(|_| format!("version spec '{original}' component '{part}' out of range"))?;
    }
    if saw_wildcard && prerelease_text.is_some() {
        return Err(format!(
            "version spec '{original}' cannot combine a wildcard with a prerelease"
        ));
    }
    if specified < 3 && prerelease_text.is_some() {
        return Err(format!(
            "partial version spec '{original}' cannot carry a prerelease"
        ));
    }
    let mut version = ModVersion {
        numeric,
        prerelease: Vec::new(),
    };
    if prerelease_text.is_some() {
        version = parse_version_value(s)
            .map_err(|e| format!("version spec '{original}' is invalid: {e}"))?;
    }
    Ok(VersionSpec { version, specified })
}

fn bumped_version(mut v: ModVersion, component: usize) -> ModVersion {
    v.numeric[component] = v.numeric[component].saturating_add(1);
    for component in v.numeric.iter_mut().skip(component + 1) {
        *component = 0;
    }
    v.prerelease.clear();
    v
}

/// Exclusive upper bound of the band the spec covers: `1.21.1` -> `1.21.2`,
/// `1.21.x` -> `1.22.0`, `1.x` -> `2.0.0`, `*` -> unbounded.
fn spec_band_hi(spec: &VersionSpec) -> Option<ModVersion> {
    match spec.specified {
        0 => None,
        1 => Some(bumped_version(spec.version.clone(), 0)),
        2 => Some(bumped_version(spec.version.clone(), 1)),
        _ => Some(bumped_version(spec.version.clone(), 2)),
    }
}

fn lower_inclusive(version: ModVersion) -> Option<VersionEndpoint> {
    Some(VersionEndpoint {
        version,
        inclusive: true,
    })
}

fn lower_exclusive(version: ModVersion) -> Option<VersionEndpoint> {
    Some(VersionEndpoint {
        version,
        inclusive: false,
    })
}

fn upper_inclusive(version: ModVersion) -> Option<VersionEndpoint> {
    Some(VersionEndpoint {
        version,
        inclusive: true,
    })
}

fn upper_exclusive(version: ModVersion) -> Option<VersionEndpoint> {
    Some(VersionEndpoint {
        version,
        inclusive: false,
    })
}

/// One SemVer predicate token -> a continuous interval.
fn predicate_interval(tok: &str) -> Result<VersionInterval, String> {
    if matches!(tok, "*" | "x" | "X") {
        return Ok(VersionInterval::unbounded());
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
        return Ok(VersionInterval::unbounded());
    }
    let lo = spec.version.clone();
    let hi_band = spec_band_hi(&spec);
    Ok(match op {
        ">=" => VersionInterval {
            lower: lower_inclusive(lo),
            upper: None,
        },
        ">" => {
            if spec.specified == 3 {
                VersionInterval {
                    lower: lower_exclusive(lo),
                    upper: None,
                }
            } else {
                VersionInterval {
                    lower: hi_band.and_then(lower_inclusive),
                    upper: None,
                }
            }
        }
        "<=" => {
            if spec.specified == 3 {
                VersionInterval {
                    lower: None,
                    upper: upper_inclusive(lo),
                }
            } else {
                VersionInterval {
                    lower: None,
                    upper: hi_band.and_then(upper_exclusive),
                }
            }
        }
        "<" => VersionInterval {
            lower: None,
            upper: upper_exclusive(lo),
        },
        "=" => {
            if spec.specified == 3 {
                VersionInterval::exact(lo)
            } else {
                VersionInterval {
                    lower: lower_inclusive(lo),
                    upper: hi_band.and_then(upper_exclusive),
                }
            }
        }
        "~" => {
            // Tilde keeps the minor band: `~1.21.1` is `>=1.21.1 <1.22.0`,
            // `~1` is `>=1.0.0 <2.0.0`.
            let hi = if spec.specified >= 2 {
                bumped_version(lo.clone(), 1)
            } else {
                bumped_version(lo.clone(), 0)
            };
            VersionInterval {
                lower: lower_inclusive(lo),
                upper: upper_exclusive(hi),
            }
        }
        "^" => {
            // Caret allows everything below the leftmost non-zero component's
            // increment: `^1.21.1` <2.0.0, `^0.2.3` <0.3.0, `^0.0.3` <0.0.4,
            // `^0.0` <0.1.0, `^0` <1.0.0.
            let hi = if spec.version.numeric[0] > 0 {
                bumped_version(lo.clone(), 0)
            } else if spec.specified >= 2 && spec.version.numeric[1] > 0 {
                bumped_version(lo.clone(), 1)
            } else if spec.specified == 3 {
                bumped_version(lo.clone(), 2)
            } else if spec.specified == 2 {
                let mut v = lo.clone();
                v.numeric[1] = 0;
                v.numeric[2] = 0;
                bumped_version(v, 1)
            } else {
                bumped_version(lo.clone(), 0)
            };
            VersionInterval {
                lower: lower_inclusive(lo),
                upper: upper_exclusive(hi),
            }
        }
        // Bare spec: a full `1.21.1` pins exactly, a partial `1.21`/`1` covers
        // its whole band (`1.21.x`-style), matching SemVer range convention.
        _ => {
            if spec.specified == 3 {
                VersionInterval::exact(lo)
            } else {
                VersionInterval {
                    lower: lower_inclusive(lo),
                    upper: hi_band.and_then(upper_exclusive),
                }
            }
        }
    })
}

/// Parse one bracketed Maven interval member starting at `s[i]` (`[` or `(`);
/// returns its interval and the index just past the closer.
fn parse_maven_member(s: &str, i: usize) -> Result<(VersionInterval, usize), String> {
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
            VersionInterval::exact(parse_version_value(v)?)
        }
        2 => {
            let lo_s = parts[0].trim();
            let hi_s = parts[1].trim();
            let lower = if lo_s.is_empty() {
                None
            } else {
                Some(VersionEndpoint {
                    version: parse_version_value(lo_s)?,
                    inclusive: open_incl,
                })
            };
            let upper = if hi_s.is_empty() {
                None
            } else {
                Some(VersionEndpoint {
                    version: parse_version_value(hi_s)?,
                    inclusive: close_incl,
                })
            };
            VersionInterval { lower, upper }
        }
        _ => {
            return Err(format!(
                "interval '{inner}' in '{s}' has more than two bounds"
            ))
        }
    };
    Ok((interval, close + 1))
}

/// Maven / NeoForge range: a sequence of bracketed intervals (plus bare
/// exact pins). Real Maven union syntax concatenates bracketed members
/// (`(,1.20.5],[1.21.1,)`); a comma separator is accepted as a tolerant
/// extension because some hand-written manifests use it.
fn parse_maven_range(s: &str) -> Result<Vec<VersionInterval>, String> {
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
            let (interval, next) = parse_maven_member(s, i)?;
            intervals.push(interval);
            i = next;
        } else {
            // Bare member inside the list: exact pin (`(,1.0],1.2` style).
            let mut end = bytes.len();
            for (off, b) in bytes[i..].iter().enumerate() {
                if matches!(*b, b',' | b'[' | b'(') {
                    end = i + off;
                    break;
                }
            }
            let tok = s[i..end].trim();
            if tok.is_empty() {
                return Err(format!("empty list member in '{s}'"));
            }
            intervals.push(VersionInterval::exact(parse_version_value(tok)?));
            i = end;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if matches!(bytes[i], b'[' | b'(') {
            // Standard adjacent Maven union member.
            continue;
        }
        if bytes[i] != b',' {
            return Err(format!("expected ',' or an interval in '{s}'"));
        }
        i += 1;
        // A comma may just separate union members; loop validation catches a
        // trailing comma or an empty member fail-closed.
    }
    Ok(intervals)
}

/// SemVer range: `||`-separated union arms, each an AND of space-separated
/// comparator/tilde/caret/wildcard/bare predicates.
fn parse_semver_range(s: &str) -> Result<Vec<VersionInterval>, String> {
    let mut intervals = Vec::new();
    for arm in s.split("||") {
        let arm = arm.trim();
        if arm.is_empty() {
            return Err(format!("empty '||' arm in '{s}'"));
        }
        let mut interval = VersionInterval::unbounded();
        let mut tokens = arm.split_whitespace();
        while let Some(tok) = tokens.next() {
            // Tolerate `>= 1.21.1`: a lone operator binds the next token.
            let tok = if matches!(tok, "~" | "^" | ">=" | "<=" | ">" | "<" | "=") {
                match tokens.next() {
                    Some(next) => format!("{}{}", tok, next),
                    None => tok.to_string(),
                }
            } else {
                tok.to_string()
            };
            let next = predicate_interval(&tok)?;
            interval = intersect_intervals(&interval, &next);
        }
        intervals.push(interval);
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

/// A parsed mod/Minecraft version range: a union of continuous intervals
/// covering both the NeoForge/Forge Maven syntax and the Fabric SemVer
/// syntax. Unlike a name-only or numeric-only solver, endpoint qualifiers are
/// part of the ordering, so `[1.0-alpha.1,1.0]` really admits
/// `1.0-beta.2` and rejects `1.0.1-SNAPSHOT` exactly as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRange {
    declared: String,
    intervals: Vec<VersionInterval>,
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
        let version = ModVersion::from(v);
        self.contains_version(&version)
    }

    /// Whether a general parsed mod version falls inside this range.
    pub fn contains_version(&self, v: &ModVersion) -> bool {
        self.intervals.iter().any(|interval| interval.contains(v))
    }

    /// Parse `version` and test it against this range. Invalid concrete
    /// versions fail closed with an error rather than being coerced.
    pub fn contains_version_str(&self, version: &str) -> Result<bool, VersionRangeError> {
        let parsed = ModVersion::parse(version)
            .ok_or_else(|| VersionRangeError(format!("invalid concrete version '{version}'")))?;
        Ok(self.contains_version(&parsed))
    }

    /// Infallible convenience wrapper for callers that treat an unparsable
    /// candidate as simply not satisfying the range.
    pub fn matches_str(&self, version: &str) -> bool {
        ModVersion::parse(version).is_some_and(|v| self.contains_version(&v))
    }

    /// Whether the range admits at least one version on `v`'s `major.minor`
    /// line — distinguishes a minor mismatch (same `1.21.x` line, wrong patch
    /// window) from a major one (mod built for another release line).
    fn overlaps_minor_band(&self, v: McVersion) -> bool {
        let band = VersionInterval {
            lower: lower_inclusive(ModVersion::from(McVersion::new(v.major, v.minor, 0))),
            upper: upper_inclusive(ModVersion::from(McVersion::new(v.major, v.minor, u16::MAX))),
        };
        self.intervals
            .iter()
            .any(|interval| intervals_overlap(interval, &band))
    }
}

/// Convenience predicate used by dependency solvers: parse `declared_range`
/// and the concrete `version`, then perform real endpoint-aware satisfaction.
/// Any malformed input is an error — never silently treated as compatible.
pub fn version_satisfies(declared_range: &str, version: &str) -> Result<bool, VersionRangeError> {
    VersionRange::parse(declared_range)?.contains_version_str(version)
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
    fn maven_qualifiers_and_semver_prereleases_order_accurately() {
        let alpha = ModVersion::parse("1.0.0-alpha.1").unwrap();
        let alpha2 = ModVersion::parse("1.0.0-alpha.2").unwrap();
        let beta = ModVersion::parse("1.0.0-beta.2").unwrap();
        let rc = ModVersion::parse("1.0.0-rc.1").unwrap();
        let snapshot = ModVersion::parse("1.0.0-SNAPSHOT").unwrap();
        let release = ModVersion::parse("1.0.0").unwrap();
        assert!(alpha < alpha2 && alpha2 < beta && beta < rc);
        assert!(rc < snapshot && snapshot < release);
        assert_eq!(ModVersion::parse("1.0.0+build.7"), Some(release.clone()));

        // Maven endpoints carry qualifiers instead of truncating them.
        let r = range("[1.0.0-alpha.1,1.0.0]");
        assert!(r.contains_version(&alpha));
        assert!(r.contains_version(&beta));
        assert!(r.contains_version(&release));
        assert!(!r.contains_version(&ModVersion::parse("1.0.1-SNAPSHOT").unwrap()));

        // SemVer comparators use the same total ordering.
        let r = range(">=1.0.0-alpha.1 <1.0.0");
        assert!(r.matches_str("1.0.0-alpha.1"));
        assert!(r.matches_str("1.0.0-rc.1"));
        assert!(r.matches_str("1.0.0-SNAPSHOT"));
        assert!(!r.matches_str("1.0.0"));
        assert!(!r.matches_str("1.0.0-alpha.0"));
        assert!(version_satisfies("[2.0-beta.2,)", "2.0-rc.1").unwrap());
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
