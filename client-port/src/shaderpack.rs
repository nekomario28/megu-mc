// SPDX-License-Identifier: LGPL-3.0-or-later

//! Clean-room Iris 1.7+ / OptiFine shaderpack loader, preprocessor, render-pass
//! pipeline planner and GPU capability validator.
//!
//! This module loads **user-supplied external** shaderpacks only. No shader
//! code, and no part of any third-party pack, is bundled here: every byte a
//! plan is built from comes out of a directory under `shaderpacks/<dir>/` or a
//! `.zip` the user dropped there, read under strict bounds
//! ([`MAX_PACK_FILES`], [`MAX_PACK_UNPACKED_BYTES`],
//! [`MAX_SHADER_FILE_BYTES`]) with fail-closed rejection of path traversal,
//! corrupt archives, `#include` cycles and unsupported GPU requirements.
//!
//! The pipeline is:
//!
//! 1. [`ShaderPackSource`] reads a pack into a bounded in-memory file table
//!    (from a directory, a `.zip`, or an in-memory map for tests/tooling).
//! 2. [`ShaderProperties`] parses `shaders.properties` (SSBOs, custom images
//!    and textures, buffer scales, flips, alpha tests, blends, shadow settings,
//!    profiles/screens/sliders and `iris.features.*` flags) while [`IdMap`]
//!    parses `block.properties` / `item.properties` / `entity.properties` into
//!    deterministic material-ID lookup tables.
//! 3. [`preprocess_shader_source`] expands `#include` directives inside the
//!    pack root under [`MAX_INCLUDE_DEPTH`] with cycle detection, then extracts
//!    `#version`, `#define` options, `/* DRAWBUFFERS:.. */` /
//!    `/* RENDERTARGETS:.. */`, `const ivec3 workGroups` and
//!    `const vec2 workGroupsRender`.
//! 4. [`plan_shader_pipeline`] discovers every Iris 1.7+ program, orders the
//!    stages (`Begin -> Shadow -> Prepare -> Gbuffers -> DistantHorizons ->
//!    Deferred -> Composite -> Final`) and validates the whole plan plus the
//!    pack's required feature flags against a [`GpuShaderCaps`] profile,
//!    producing a [`ShaderCompatibilityReport`] a renderer can use to decide
//!    between the shader pipeline and the vanilla fallback.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Bounds (all fail-closed)
// ---------------------------------------------------------------------------

/// Maximum number of files a pack may contain.
pub const MAX_PACK_FILES: usize = 4096;
/// Maximum total uncompressed bytes across every file in a pack.
pub const MAX_PACK_UNPACKED_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum uncompressed bytes for a single file in a pack.
pub const MAX_SHADER_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum on-disk bytes of a `.zip` archive itself.
pub const MAX_ZIP_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum `#include` nesting depth.
pub const MAX_INCLUDE_DEPTH: usize = 16;
/// Maximum total number of `#include` expansions in one shader translation
/// unit. Bounds the blow-up of diamond include graphs.
pub const MAX_TOTAL_INCLUDES: usize = 1024;
/// Maximum bytes of any `.properties` file.
pub const MAX_PROPERTIES_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong while loading or planning a shaderpack.
///
/// The loader is fail-closed: malformed input always produces one of these
/// errors rather than a partial or guessed result.
#[derive(Debug)]
#[non_exhaustive]
pub enum ShaderPackError {
    /// Underlying filesystem failure.
    Io {
        /// Path that failed.
        path: String,
        /// OS error.
        source: std::io::Error,
    },
    /// The path is not a directory or a `.zip` archive.
    NotAPack {
        /// Path that was rejected.
        path: String,
    },
    /// A path (inside a zip, on disk, or in an `#include`) escapes the pack
    /// root or contains forbidden characters (`..`, `\`, NUL, leading `/`,
    /// drive letters).
    UnsafePath {
        /// The offending path.
        path: String,
    },
    /// Pack contains more than [`MAX_PACK_FILES`] files.
    TooManyFiles {
        /// Files seen.
        count: usize,
    },
    /// Pack uncompressed payload exceeds [`MAX_PACK_UNPACKED_BYTES`].
    PackTooLarge {
        /// Bytes seen so far.
        bytes: u64,
    },
    /// A single file exceeds [`MAX_SHADER_FILE_BYTES`].
    FileTooLarge {
        /// File path.
        path: String,
        /// File size in bytes.
        bytes: u64,
    },
    /// The `.zip` container is structurally corrupt.
    ZipMalformed {
        /// What was wrong.
        reason: String,
    },
    /// The `.zip` uses a feature we deliberately do not implement
    /// (multi-disk, ZIP64, encryption, compression methods other than
    /// stored/deflate).
    ZipUnsupported {
        /// What was unsupported.
        reason: String,
    },
    /// `#include` target does not exist inside the pack.
    IncludeNotFound {
        /// Include as written.
        include: String,
        /// File that requested it.
        from: String,
    },
    /// `#include` nesting exceeded [`MAX_INCLUDE_DEPTH`].
    IncludeDepthExceeded {
        /// File that crossed the limit.
        path: String,
    },
    /// `#include` cycle detected.
    IncludeCycle {
        /// File that closes the cycle.
        path: String,
    },
    /// More than [`MAX_TOTAL_INCLUDES`] expansions in one translation unit.
    TooManyIncludes {
        /// Expansions seen.
        count: usize,
    },
    /// Malformed `#include` or other shader directive.
    MalformedShader {
        /// File containing the line.
        path: String,
        /// 1-based line number.
        line: usize,
        /// What was wrong.
        reason: String,
    },
    /// Malformed `.properties` content.
    MalformedProperties {
        /// File name (`shaders.properties`, `block.properties`, ...).
        file: String,
        /// 1-based line number.
        line: usize,
        /// What was wrong.
        reason: String,
    },
    /// A graphics program is missing its vertex or fragment stage, or mixes
    /// compute and graphics stages under one name.
    IncompleteProgram {
        /// Program name (e.g. `composite1`).
        name: String,
        /// What was missing/conflicting.
        reason: String,
    },
    /// No render passes were discovered at all.
    EmptyPack,
}

impl fmt::Display for ShaderPackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "I/O error on {path}: {source}"),
            Self::NotAPack { path } => {
                write!(f, "{path} is not a shaderpack directory or .zip")
            }
            Self::UnsafePath { path } => write!(f, "unsafe pack path rejected: {path:?}"),
            Self::TooManyFiles { count } => {
                write!(f, "pack has {count} files (limit {MAX_PACK_FILES})")
            }
            Self::PackTooLarge { bytes } => {
                write!(
                    f,
                    "pack exceeds {MAX_PACK_UNPACKED_BYTES} uncompressed bytes ({bytes} seen)"
                )
            }
            Self::FileTooLarge { path, bytes } => write!(
                f,
                "pack file {path} is {bytes} bytes (limit {MAX_SHADER_FILE_BYTES})"
            ),
            Self::ZipMalformed { reason } => write!(f, "corrupt zip archive: {reason}"),
            Self::ZipUnsupported { reason } => write!(f, "unsupported zip feature: {reason}"),
            Self::IncludeNotFound { include, from } => {
                write!(f, "{from}: #include {include:?} not found in pack")
            }
            Self::IncludeDepthExceeded { path } => write!(
                f,
                "{path}: #include depth exceeds {MAX_INCLUDE_DEPTH}"
            ),
            Self::IncludeCycle { path } => write!(f, "#include cycle at {path}"),
            Self::TooManyIncludes { count } => {
                write!(f, "more than {MAX_TOTAL_INCLUDES} #includes ({count})")
            }
            Self::MalformedShader {
                path,
                line,
                reason,
            } => write!(f, "{path}:{line}: {reason}"),
            Self::MalformedProperties {
                file,
                line,
                reason,
            } => write!(f, "{file}:{line}: {reason}"),
            Self::IncompleteProgram { name, reason } => {
                write!(f, "program {name}: {reason}")
            }
            Self::EmptyPack => write!(f, "shaderpack contains no render passes"),
        }
    }
}

impl std::error::Error for ShaderPackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Virtual path handling
// ---------------------------------------------------------------------------

/// Validate a pack-internal virtual path (`/`-separated, relative to the
/// shader root). Fail-closed on anything that could escape or be
/// interpreted differently by a downstream consumer.
fn validate_virtual_path(path: &str) -> Result<(), ShaderPackError> {
    let bad = || ShaderPackError::UnsafePath {
        path: path.to_owned(),
    };
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.len() > 512
    {
        return Err(bad());
    }
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            return Err(bad());
        }
        // Drive-letter / UNC style tricks.
        if comp.contains(':') {
            return Err(bad());
        }
    }
    Ok(())
}

/// Normalize a *relative* include or lookup path: resolve `.`/`..`
/// lexically, failing closed if the result escapes the root.
fn normalize_relative(path: &str) -> Result<String, ShaderPackError> {
    let bad = || ShaderPackError::UnsafePath {
        path: path.to_owned(),
    };
    if path.is_empty() || path.contains('\\') || path.contains('\0') || path.contains(':') {
        return Err(bad());
    }
    let mut out: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(bad());
                }
            }
            c => out.push(c),
        }
    }
    if out.is_empty() {
        return Err(bad());
    }
    Ok(out.join("/"))
}

/// Convert an externally supplied pack-relative key (zip entry name or
/// in-memory map key) into a shader-root-relative canonical key.
///
/// * Leading `/` and `\` are rejected outright.
/// * A leading `shaders/` segment is stripped so both `composite.fsh` and
///   `shaders/composite.fsh` name the same file.
fn canonical_key(raw: &str) -> Result<String, ShaderPackError> {
    let trimmed = raw.trim_start_matches("./");
    let stripped = trimmed.strip_prefix("shaders/").unwrap_or(trimmed);
    validate_virtual_path(stripped)?;
    Ok(stripped.to_owned())
}

/// `UnsafePath` for any printable pack-relative path.
fn unsafe_path(path: impl std::fmt::Display) -> ShaderPackError {
    ShaderPackError::UnsafePath {
        path: path.to_string(),
    }
}

/// Map an OS failure on `path` onto the pack error surface.
fn io_error(path: &Path, source: std::io::Error) -> ShaderPackError {
    ShaderPackError::Io {
        path: path.display().to_string(),
        source,
    }
}

// ---------------------------------------------------------------------------
// ShaderPackSource
// ---------------------------------------------------------------------------

/// How the pack was supplied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackOrigin {
    /// In-memory map (tests, tooling).
    InMemory,
    /// Directory on disk (`shaderpacks/<dir>/`).
    Directory,
    /// `.zip` archive on disk (`shaderpacks/<pack>.zip`).
    ZipArchive,
}

/// Bounded, immutable view of a loaded shaderpack.
///
/// Keys are canonical shader-root-relative virtual paths
/// (`composite.fsh`, `world0/gbuffers_terrain.vsh`, `lib/util.glsl`, ...).
/// For directory and zip packs the `shaders/` directory inside the pack is
/// the root; a pack with no `shaders/` directory is treated as rooted at the
/// pack itself (lenient, still fully bounded).
#[derive(Clone, Debug)]
pub struct ShaderPackSource {
    files: BTreeMap<String, Vec<u8>>,
    origin: PackOrigin,
}

/// Bounded file-table accumulator shared by every pack loader, so the
/// per-file/per-pack/per-count limits are enforced in exactly one place and
/// in one order.
#[derive(Default)]
struct PackBuilder {
    files: BTreeMap<String, Vec<u8>>,
    total: u64,
}

impl PackBuilder {
    /// Charge `len` bytes for `path` against the file, pack and count limits.
    fn reserve(&mut self, path: &str, len: u64) -> Result<(), ShaderPackError> {
        if len > MAX_SHADER_FILE_BYTES {
            return Err(ShaderPackError::FileTooLarge {
                path: path.to_owned(),
                bytes: len,
            });
        }
        self.total += len;
        if self.total > MAX_PACK_UNPACKED_BYTES {
            return Err(ShaderPackError::PackTooLarge { bytes: self.total });
        }
        if self.files.len() >= MAX_PACK_FILES {
            return Err(ShaderPackError::TooManyFiles {
                count: self.files.len() + 1,
            });
        }
        Ok(())
    }

    /// Reserve and store one already-read file.
    fn insert(&mut self, path: String, bytes: Vec<u8>) -> Result<(), ShaderPackError> {
        self.reserve(&path, bytes.len() as u64)?;
        self.files.insert(path, bytes);
        Ok(())
    }

    /// Store a file whose size was already charged through [`Self::reserve`].
    fn push_reserved(&mut self, path: String, bytes: Vec<u8>) {
        self.files.insert(path, bytes);
    }

    fn into_source(self, origin: PackOrigin) -> ShaderPackSource {
        ShaderPackSource {
            files: self.files,
            origin,
        }
    }
}

impl ShaderPackSource {
    /// Build a source from an in-memory map. Keys may be shader-root-relative
    /// (`composite.fsh`) or pack-root-relative (`shaders/composite.fsh`);
    /// all are canonicalized and validated.
    pub fn from_memory<K, V, I>(files: I) -> Result<Self, ShaderPackError>
    where
        K: AsRef<str>,
        V: AsRef<[u8]>,
        I: IntoIterator<Item = (K, V)>,
    {
        let mut pack = PackBuilder::default();
        for (k, v) in files {
            pack.insert(canonical_key(k.as_ref())?, v.as_ref().to_vec())?;
        }
        Ok(pack.into_source(PackOrigin::InMemory))
    }

    /// Open `path` as a directory or a `.zip` (extension-based dispatch).
    pub fn open(path: &Path) -> Result<Self, ShaderPackError> {
        let meta = fs::metadata(path).map_err(|source| io_error(path, source))?;
        if meta.is_dir() {
            Self::open_directory(path)
        } else if meta.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
            Self::open_zip(path)
        } else {
            Err(ShaderPackError::NotAPack {
                path: path.display().to_string(),
            })
        }
    }

    /// Load a pack from a directory. The `shaders/` subdirectory, when
    /// present, is the shader root. Symlinks escaping the directory are
    /// rejected.
    pub fn open_directory(root: &Path) -> Result<Self, ShaderPackError> {
        let canon_root = fs::canonicalize(root).map_err(|source| io_error(root, source))?;
        if !canon_root.is_dir() {
            return Err(ShaderPackError::NotAPack {
                path: root.display().to_string(),
            });
        }
        let shaders = canon_root.join("shaders");
        let base = if shaders.is_dir() { shaders } else { canon_root };
        let mut pack = PackBuilder::default();
        let mut pending = vec![base.clone()];
        while let Some(dir) = pending.pop() {
            let mut entries: Vec<_> = fs::read_dir(&dir)
                .map_err(|source| io_error(&dir, source))?
                .collect::<Result<_, _>>()
                .map_err(|source| io_error(&dir, source))?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let raw_path = entry.path();
                // Canonicalize *every* path: any symlink resolving outside the
                // shader root is a traversal escape and is rejected.
                let canon = fs::canonicalize(&raw_path).map_err(|source| io_error(&raw_path, source))?;
                if !canon.starts_with(&base) {
                    return Err(unsafe_path(raw_path.display()));
                }
                let rel = canon
                    .strip_prefix(&base)
                    .map_err(|_| unsafe_path(raw_path.display()))?;
                let rel_str = path_to_virtual(rel)?;
                if canon.is_dir() {
                    pending.push(canon);
                } else if canon.is_file() {
                    let meta = fs::metadata(&canon).map_err(|source| io_error(&canon, source))?;
                    // Charge the size before reading so an oversized file is
                    // never pulled into memory.
                    pack.reserve(&rel_str, meta.len())?;
                    let bytes = fs::read(&canon).map_err(|source| io_error(&canon, source))?;
                    pack.push_reserved(rel_str, bytes);
                }
            }
        }
        Ok(pack.into_source(PackOrigin::Directory))
    }

    /// Load a pack from a `.zip` archive (stored or deflate entries).
    pub fn open_zip(path: &Path) -> Result<Self, ShaderPackError> {
        let meta = fs::metadata(path).map_err(|source| io_error(path, source))?;
        if meta.len() > MAX_ZIP_FILE_BYTES {
            return Err(ShaderPackError::PackTooLarge { bytes: meta.len() });
        }
        let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
        let entries = read_zip_entries(&bytes)?;
        let mut pack = PackBuilder::default();
        // Find the shader root prefix: prefer `shaders/`, else a single
        // wrapping top-level directory containing `shaders/`, else the
        // archive root itself.
        let prefix = shader_root_prefix(entries.iter().map(|(n, _)| n.as_str()));
        for (name, data) in entries {
            let Some(rel) = name.strip_prefix(&prefix) else {
                continue;
            };
            pack.insert(canonical_key(rel)?, data)?;
        }
        Ok(pack.into_source(PackOrigin::ZipArchive))
    }

    /// How this pack was supplied.
    pub fn origin(&self) -> PackOrigin {
        self.origin
    }

    /// Read a file by shader-root-relative virtual path.
    pub fn read(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    /// Read a file as lossy UTF-8 (shader sources and `.properties` files are
    /// expected to be text; lossless-ness is not required for parsing).
    pub fn read_text(&self, path: &str) -> Option<String> {
        self.read(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// Whether a file exists.
    pub fn contains(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Iterate all virtual paths in deterministic (sorted) order.
    pub fn list(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Number of files in the pack.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the pack is empty.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Dimension directories discovered (`world0`, `world-1`, `world1`,
    /// `world<N>`), sorted ascending.
    pub fn dimensions(&self) -> Vec<i32> {
        self.files
            .keys()
            .filter_map(|k| k.split('/').next().and_then(parse_world_dir))
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect()
    }
}

/// Convert a relative `Path` into a `/`-separated virtual path.
fn path_to_virtual(rel: &Path) -> Result<String, ShaderPackError> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| unsafe_path(rel.display()))?;
                parts.push(s);
            }
            _ => return Err(unsafe_path(rel.display())),
        }
    }
    let joined = parts.join("/");
    validate_virtual_path(&joined)?;
    Ok(joined)
}

/// `world0`, `world-1`, `world1`, `world<N>` -> `N`. Returns `None` for
/// anything else.
fn parse_world_dir(name: &str) -> Option<i32> {
    let rest = name.strip_prefix("world")?;
    if rest.is_empty() || rest.len() > 6 {
        return None;
    }
    let (sign, digits) = match rest.strip_prefix('-') {
        Some(d) => (-1i32, d),
        None => (1i32, rest),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i32>().ok().map(|n| sign * n)
}

/// Determine the shader-root prefix for a set of archive entry names:
/// `shaders/` if any entry lives there, else `<top>/shaders/` when every
/// entry shares one top-level dir that wraps a `shaders/` dir, else `""`.
fn shader_root_prefix<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.iter().any(|n| n.starts_with("shaders/")) {
        return "shaders/".to_owned();
    }
    let tops: BTreeSet<&str> = names
        .iter()
        .map(|n| n.split_once('/').map_or("", |(top, _)| top))
        .collect();
    if tops.len() == 1 {
        let top = tops.into_iter().next().unwrap_or_default();
        if !top.is_empty() {
            let prefix = format!("{top}/shaders/");
            if names.iter().any(|n| n.starts_with(&prefix)) {
                return prefix;
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Bounded ZIP reader (delegates to `megu_core::zip_archive::ZipSliceIndex`)
// ---------------------------------------------------------------------------

/// Map a canonical ZIP index failure onto the pack error surface.
fn zip_index_error(error: megu_core::zip_archive::ZipError) -> ShaderPackError {
    use megu_core::zip_archive::ZipError as Z;
    match error {
        Z::InvalidEntryName(path) => ShaderPackError::UnsafePath { path },
        Z::MultiDisk => ShaderPackError::ZipUnsupported {
            reason: "multi-disk archive".to_owned(),
        },
        Z::Encrypted(name) => ShaderPackError::ZipUnsupported {
            reason: format!("encrypted entry {name}"),
        },
        Z::UnsupportedMethod(method) => ShaderPackError::ZipUnsupported {
            reason: format!("compression method {method}"),
        },
        other => ShaderPackError::ZipMalformed {
            reason: other.to_string(),
        },
    }
}

/// Parse the central directory of `zip` and decompress every regular file
/// entry. EOCD/Central-Directory walking, name validation, ZIP64, multi-disk
/// and encryption rejection are delegated to the canonical
/// [`megu_core::zip_archive::ZipSliceIndex`]; returns `(name,
/// uncompressed-bytes)` pairs in archive order.
fn read_zip_entries(zip: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ShaderPackError> {
    let index = megu_core::zip_archive::ZipSliceIndex::open(zip).map_err(zip_index_error)?;
    let mut out = Vec::new();
    for meta in index.entries() {
        // Only top-level members are pack files; Jar-in-Jar children are
        // not part of the shaderpack layout.
        if meta.archive_id != 0 {
            continue;
        }
        let name = meta.name.clone();
        if meta.uncompressed_size > MAX_SHADER_FILE_BYTES {
            return Err(ShaderPackError::FileTooLarge {
                path: name,
                bytes: meta.uncompressed_size,
            });
        }
        let bytes = index
            .read_entry(&name, MAX_SHADER_FILE_BYTES as usize)
            .map_err(|e| match e {
                megu_core::zip_archive::ZipError::UnsupportedMethod(method) => {
                    ShaderPackError::ZipUnsupported {
                        reason: format!("compression method {method}"),
                    }
                }
                other => ShaderPackError::ZipMalformed {
                    reason: format!("{name}: {other}"),
                },
            })?;
        out.push((name, bytes));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// .properties parsing (shared by shaders.properties and the ID maps)
// ---------------------------------------------------------------------------

/// One parsed `key = value` record plus its 1-based logical line number.
struct RawProperty {
    key: String,
    value: String,
    line: usize,
}

impl RawProperty {
    /// Parse `tok` (a token from this line, or its value) as `T`.
    fn field<T: std::str::FromStr>(&self, tok: &str, what: &str) -> Result<T, ShaderPackError> {
        tok.parse()
            .map_err(|_| malformed_props(SHADERS_PROPERTIES, self.line, what))
    }

    /// Parse `tok` as a boolean property token (`true`/`on`/`yes`/`1`, ...).
    fn flag(&self, tok: &str, what: &str) -> Result<bool, ShaderPackError> {
        parse_bool_token(tok).ok_or_else(|| malformed_props(SHADERS_PROPERTIES, self.line, what))
    }
}

/// Parse Java-`.properties` text: `\` line continuations, `#`/`!` comments,
/// `key=value`, `key:value` or `key value` separators. Deterministic.
fn parse_properties(text: &str, file: &str) -> Result<Vec<RawProperty>, ShaderPackError> {
    // Join continuation lines: a trailing odd number of backslashes merges
    // with the next line.
    let mut logical: Vec<(usize, String)> = Vec::new();
    let mut buf = String::new();
    let mut start_line = 0usize;
    for (idx, phys) in text.split('\n').enumerate() {
        let line_no = idx + 1;
        if buf.is_empty() {
            start_line = line_no;
        }
        let stripped = phys.strip_suffix('\r').unwrap_or(phys);
        let bs_run = stripped.chars().rev().take_while(|&c| c == '\\').count();
        if bs_run % 2 == 1 {
            buf.push_str(&stripped[..stripped.len() - 1]);
        } else {
            buf.push_str(stripped);
            logical.push((start_line, std::mem::take(&mut buf)));
        }
    }
    if !buf.is_empty() {
        logical.push((start_line, buf));
    }
    let mut out = Vec::new();
    for (line_no, line) in logical {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        // Split on the first unescaped separator: '=' or ':' (optionally
        // surrounded by whitespace), or bare whitespace.
        let bytes = trimmed.as_bytes();
        let mut sep = None;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'=' || b == b':' || b == b' ' || b == b'\t' {
                sep = Some(i);
                break;
            }
        }
        let Some(sep) = sep else {
            // A bare key with no value: treat as empty value.
            out.push(RawProperty {
                key: trimmed.to_owned(),
                value: String::new(),
                line: line_no,
            });
            continue;
        };
        let key = trimmed[..sep].trim_end();
        let mut rest = &trimmed[sep..];
        if rest.starts_with([' ', '\t']) {
            // Whitespace separator, optionally followed by `=` or `:`.
            rest = rest.trim_start();
            if rest.starts_with(['=', ':']) {
                rest = &rest[1..];
            }
        } else {
            rest = &rest[1..];
        }
        let value = rest.trim().to_owned();
        if key.is_empty() {
            return Err(ShaderPackError::MalformedProperties {
                file: file.to_owned(),
                line: line_no,
                reason: "empty key".to_owned(),
            });
        }
        out.push(RawProperty {
            key: key.to_owned(),
            value,
            line: line_no,
        });
    }
    Ok(out)
}

fn malformed_props(file: &str, line: usize, reason: impl Into<String>) -> ShaderPackError {
    ShaderPackError::MalformedProperties {
        file: file.to_owned(),
        line,
        reason: reason.into(),
    }
}

fn parse_bool_token(tok: &str) -> Option<bool> {
    match tok {
        "true" | "on" | "yes" | "1" => Some(true),
        "false" | "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// The only file applied through [`ShaderProperties`]; `apply` blames its
/// line numbers on it.
const SHADERS_PROPERTIES: &str = "shaders.properties";

/// A whitespace-separated token list, as owned strings (option screens,
/// sliders, profiles and blend functions all use this shape).
fn words(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_owned).collect()
}

// ---------------------------------------------------------------------------
// Iris / OptiFine feature flags
// ---------------------------------------------------------------------------

/// Iris 1.7+ feature flags usable in `iris.features.required` /
/// `iris.features.optional`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IrisFeature {
    /// `COMPUTE_SHADERS` — `.csh` compute passes (begin/shadowcomp/prepare/
    /// deferred/composite).
    ComputeShaders,
    /// `SSBO` — `bufferObject.<N>` shader storage buffers.
    Ssbo,
    /// `CUSTOM_IMAGES` — `image.<name>` custom image units.
    CustomImages,
    /// `ENTITY_TRANSLUCENT` — translucent entity gbuffers programs.
    EntityTranslucent,
    /// `SEPARATE_HARDWARE_SAMPLERS` — distinct sampler hardware units.
    SeparateHardwareSamplers,
    /// `PER_BUFFER_BLENDING` — `blend.<pass>.<buf>` directives.
    PerBufferBlending,
    /// Any other flag, preserved verbatim for fail-closed reporting.
    Unknown(String),
}

impl IrisFeature {
    fn parse(tok: &str) -> Self {
        match tok.to_ascii_uppercase().as_str() {
            "COMPUTE_SHADERS" => Self::ComputeShaders,
            "SSBO" => Self::Ssbo,
            "CUSTOM_IMAGES" => Self::CustomImages,
            "ENTITY_TRANSLUCENT" => Self::EntityTranslucent,
            "SEPARATE_HARDWARE_SAMPLERS" => Self::SeparateHardwareSamplers,
            "PER_BUFFER_BLENDING" => Self::PerBufferBlending,
            other => Self::Unknown(other.to_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// shaders.properties model
// ---------------------------------------------------------------------------

/// `bufferObject.<id>` — a shader storage buffer declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct SsboSpec {
    /// Binding index from the property key.
    pub binding: u32,
    /// `bufferObject.<id> = <byte_size>` static size.
    pub byte_size: u64,
    /// Optional `bufferObject.<id>.scale` factor for relative sizing.
    pub scale: Option<f64>,
}

/// `image.<name>` — a custom image declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomImage {
    /// Image name (the part after `image.`).
    pub name: String,
    /// Sampler name shaders bind it as.
    pub sampler: String,
    /// Format token (e.g. `rgba8`).
    pub format: String,
    /// Internal-format token.
    pub internal_format: String,
    /// Pixel-type token (e.g. `ubyte`, `float`).
    pub pixel_type: String,
    /// Whether the image is cleared each frame.
    pub clear: bool,
    /// Whether `size` values are relative to the render resolution.
    pub relative: bool,
    /// 1-3 size components (pixels or scale factors when `relative`).
    pub size: Vec<f64>,
}

/// `texture.<stage>.<sampler> = <path>` — a custom texture binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomTexture {
    /// Program stage (`gbuffers`, `deferred`, `composite`, ...; empty for
    /// stage-less bindings like `texture.noise`).
    pub stage: String,
    /// Sampler name.
    pub name: String,
    /// Pack-relative texture path.
    pub path: String,
}

/// `alphaTest.<pass>` value.
#[derive(Clone, Debug, PartialEq)]
pub enum AlphaTest {
    /// `off` / `false`.
    Off,
    /// `always` / `on`.
    Always,
    /// `never`.
    Never,
    /// Comparison function + threshold (`GREATER 0.1`, `LESS 0.5`, ...).
    Func {
        /// `greater`, `less`, `equal`, `notequal`, `gequal`, `lequal`.
        func: String,
        /// Threshold.
        value: f64,
    },
    /// Anything else, preserved verbatim.
    Raw(String),
}

/// `blend.<pass>.<buf> = <tokens>` — per-buffer blending directive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlendDirective {
    /// Program name.
    pub pass: String,
    /// Color attachment the blend applies to.
    pub buffer: String,
    /// Blend function tokens (e.g. `src_alpha one_minus_src_alpha`).
    pub tokens: Vec<String>,
}

/// Shadow-map settings from `shaders.properties`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShadowSettings {
    /// `shadow.enabled`.
    pub enabled: Option<bool>,
    /// `shadowDistance`.
    pub distance: Option<f64>,
    /// `shadowMapResolution`.
    pub resolution: Option<u32>,
}

/// `profile.<NAME> = <tokens>` — a user-selectable option profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Profile name (the part after `profile.`).
    pub name: String,
    /// `key:value` / `program:<name>` tokens, in file order.
    pub tokens: Vec<String>,
}

/// Parsed `shaders.properties`.
#[derive(Clone, Debug, Default)]
pub struct ShaderProperties {
    /// `bufferObject.<id>` SSBOs, by binding.
    pub ssbos: BTreeMap<u32, SsboSpec>,
    /// `image.<name>` custom images, by name.
    pub images: BTreeMap<String, CustomImage>,
    /// `texture.<stage>.<sampler>` bindings, by (stage, sampler).
    pub textures: BTreeMap<(String, String), CustomTexture>,
    /// `size.buffer.<name>` explicit buffer sizes.
    pub buffer_sizes: BTreeMap<String, (u32, u32)>,
    /// `scale.<pass>` scale factors (1-2 components).
    pub pass_scales: BTreeMap<String, Vec<f64>>,
    /// `flip.<pass>.<buffer>` directives `(pass, buffer, flip)`.
    pub flips: Vec<(String, String, bool)>,
    /// `alphaTest.<pass>` directives.
    pub alpha_tests: BTreeMap<String, AlphaTest>,
    /// `blend.<pass>.<buf>` directives.
    pub blends: Vec<BlendDirective>,
    /// Shadow settings.
    pub shadow: ShadowSettings,
    /// `profile.<NAME>` option profiles.
    pub profiles: BTreeMap<String, Profile>,
    /// `screen` / `screen.<SUB>` option-screen layouts (`""` = main screen).
    pub screens: BTreeMap<String, Vec<String>>,
    /// `sliders` list.
    pub sliders: Vec<String>,
    /// `iris.features.required`.
    pub required_features: BTreeSet<IrisFeature>,
    /// `iris.features.optional`.
    pub optional_features: BTreeSet<IrisFeature>,
    /// All raw key/value pairs, last-one-wins, for forward-compatible access.
    pub raw: BTreeMap<String, String>,
}

impl ShaderProperties {
    /// Parse `shaders.properties` text.
    pub fn parse(text: &str) -> Result<Self, ShaderPackError> {
        let mut out = Self::default();
        for prop in parse_properties(text, SHADERS_PROPERTIES)? {
            out.raw.insert(prop.key.clone(), prop.value.clone());
            Self::apply(&mut out, &prop)?;
        }
        Ok(out)
    }

    /// Apply one parsed property line. Unknown keys are kept in `raw` for
    /// forward compatibility; they are not errors.
    fn apply(out: &mut Self, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let key = prop.key.as_str();
        if let Some(rest) = key.strip_prefix("bufferObject.") {
            Self::apply_buffer_object(out, rest, prop)?;
        } else if let Some(name) = key.strip_prefix("image.") {
            Self::apply_image(out, name, prop)?;
        } else if let Some(rest) = key.strip_prefix("texture.") {
            Self::apply_texture(out, rest, prop);
        } else if let Some(name) = key.strip_prefix("size.buffer.") {
            Self::apply_buffer_size(out, name, prop)?;
        } else if let Some(pass) = key.strip_prefix("scale.") {
            Self::apply_pass_scale(out, pass, prop)?;
        } else if let Some(rest) = key.strip_prefix("flip.") {
            Self::apply_flip(out, rest, prop)?;
        } else if let Some(pass) = key.strip_prefix("alphaTest.") {
            Self::apply_alpha_test(out, pass, prop)?;
        } else if let Some(rest) = key.strip_prefix("blend.") {
            Self::apply_blend(out, rest, prop);
        } else if let Some(name) = key.strip_prefix("profile.") {
            out.profiles.insert(
                name.to_owned(),
                Profile {
                    name: name.to_owned(),
                    tokens: words(&prop.value),
                },
            );
        } else {
            Self::apply_plain_key(out, key, prop)?;
        }
        Ok(())
    }

    /// `bufferObject.<id> = <byte_size>` / `bufferObject.<id>.scale = <f64>`.
    fn apply_buffer_object(out: &mut Self, rest: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let (id, scaled) = match rest.strip_suffix(".scale") {
            Some(id) => (id, true),
            None => (rest, false),
        };
        let binding: u32 = prop.field(id, "bad bufferObject id")?;
        let spec = out
            .ssbos
            .entry(binding)
            .or_insert(SsboSpec { binding, byte_size: 0, scale: None });
        if scaled {
            spec.scale = Some(prop.field(&prop.value, "bad bufferObject scale")?);
        } else {
            spec.byte_size = prop.field(&prop.value, "bad bufferObject byte size")?;
        }
        Ok(())
    }

    /// `image.<name> = <sampler> <format> <internalFormat> <pixelType>
    /// <clear> <relative> <size...>`.
    fn apply_image(out: &mut Self, name: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let toks: Vec<&str> = prop.value.split_whitespace().collect();
        if toks.len() < 7 {
            return Err(malformed_props(
                SHADERS_PROPERTIES,
                prop.line,
                "image needs: <sampler> <format> <internalFormat> <pixelType> <clear> <relative> <size...>",
            ));
        }
        let clear = prop.flag(toks[4], "bad image clear flag")?;
        let relative = prop.flag(toks[5], "bad image relative flag")?;
        let mut size = Vec::with_capacity(3);
        for t in &toks[6..] {
            size.push(prop.field(t, "bad image size")?);
        }
        out.images.insert(
            name.to_owned(),
            CustomImage {
                name: name.to_owned(),
                sampler: toks[0].to_owned(),
                format: toks[1].to_owned(),
                internal_format: toks[2].to_owned(),
                pixel_type: toks[3].to_owned(),
                clear,
                relative,
                size,
            },
        );
        Ok(())
    }

    /// `texture.<stage>.<sampler> = <path>` (stage-less `texture.<name>` keeps
    /// an empty stage).
    fn apply_texture(out: &mut Self, rest: &str, prop: &RawProperty) {
        let (stage, name) = match rest.split_once('.') {
            Some((s, n)) => (s.to_owned(), n.to_owned()),
            None => (String::new(), rest.to_owned()),
        };
        out.textures.insert(
            (stage.clone(), name.clone()),
            CustomTexture {
                stage,
                name,
                path: prop.value.clone(),
            },
        );
    }

    /// `size.buffer.<name> = <width> <height>`.
    fn apply_buffer_size(out: &mut Self, name: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let toks: Vec<&str> = prop.value.split_whitespace().collect();
        if toks.len() != 2 {
            return Err(malformed_props(SHADERS_PROPERTIES, prop.line, "size.buffer needs two ints"));
        }
        let w: u32 = prop.field(toks[0], "bad buffer width")?;
        let h: u32 = prop.field(toks[1], "bad buffer height")?;
        out.buffer_sizes.insert(name.to_owned(), (w, h));
        Ok(())
    }

    /// `scale.<pass> = <sx> [<sy>]`.
    fn apply_pass_scale(out: &mut Self, pass: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let mut vals = Vec::new();
        for t in prop.value.split_whitespace() {
            vals.push(prop.field(t, "bad scale value")?);
        }
        if vals.is_empty() || vals.len() > 2 {
            return Err(malformed_props(SHADERS_PROPERTIES, prop.line, "scale needs 1-2 values"));
        }
        out.pass_scales.insert(pass.to_owned(), vals);
        Ok(())
    }

    /// `flip.<pass>.<buffer> = <bool>`.
    fn apply_flip(out: &mut Self, rest: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let (pass, buf) = rest
            .split_once('.')
            .ok_or_else(|| malformed_props(SHADERS_PROPERTIES, prop.line, "flip needs <pass>.<buf>"))?;
        let flip = prop.flag(&prop.value, "bad flip value")?;
        out.flips.push((pass.to_owned(), buf.to_owned(), flip));
        Ok(())
    }

    /// `alphaTest.<pass> = off|always|never|<func> <threshold>|...`.
    fn apply_alpha_test(out: &mut Self, pass: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        let toks: Vec<&str> = prop.value.split_whitespace().collect();
        let test = match toks.first().map(|t| t.to_ascii_lowercase()) {
            Some(t) if t == "off" || t == "false" => AlphaTest::Off,
            Some(t) if t == "always" || t == "on" || t == "true" => AlphaTest::Always,
            Some(t) if t == "never" => AlphaTest::Never,
            Some(t)
                if matches!(
                    t.as_str(),
                    "greater" | "less" | "equal" | "notequal" | "gequal" | "lequal"
                ) && toks.len() == 2 =>
            {
                let value = prop.field(toks[1], "bad alphaTest value")?;
                AlphaTest::Func { func: t, value }
            }
            _ => AlphaTest::Raw(prop.value.clone()),
        };
        out.alpha_tests.insert(pass.to_owned(), test);
        Ok(())
    }

    /// `blend.<pass>[.<buffer>] = <function tokens>`.
    fn apply_blend(out: &mut Self, rest: &str, prop: &RawProperty) {
        let (pass, buffer) = match rest.split_once('.') {
            Some((p, b)) => (p.to_owned(), b.to_owned()),
            None => (rest.to_owned(), String::new()),
        };
        out.blends.push(BlendDirective {
            pass,
            buffer,
            tokens: words(&prop.value),
        });
    }

    /// Shadow settings, option screens/sliders and `iris.features.*` — the
    /// keys that are not `<namespace>.<...>` directives.
    fn apply_plain_key(out: &mut Self, key: &str, prop: &RawProperty) -> Result<(), ShaderPackError> {
        match key {
            "shadow.enabled" => {
                out.shadow.enabled = Some(prop.flag(&prop.value, "bad shadow.enabled")?);
            }
            "shadowDistance" => {
                out.shadow.distance = Some(prop.field(&prop.value, "bad shadowDistance")?);
            }
            "shadowMapResolution" => {
                out.shadow.resolution = Some(prop.field(&prop.value, "bad shadowMapResolution")?);
            }
            "screen" => {
                out.screens.insert(String::new(), words(&prop.value));
            }
            "sliders" => {
                out.sliders = words(&prop.value);
            }
            "iris.features.required" => {
                out.required_features = prop.value.split_whitespace().map(IrisFeature::parse).collect();
            }
            "iris.features.optional" => {
                out.optional_features = prop.value.split_whitespace().map(IrisFeature::parse).collect();
            }
            _ => {
                if let Some(sub) = key.strip_prefix("screen.") {
                    out.screens.insert(sub.to_owned(), words(&prop.value));
                }
            }
        }
        Ok(())
    }

    /// Convenience: read and parse `shaders.properties` from a pack.
    pub fn load(source: &ShaderPackSource) -> Result<Option<Self>, ShaderPackError> {
        match source.read_text("shaders.properties") {
            Some(text) => {
                if text.len() as u64 > MAX_PROPERTIES_BYTES {
                    return Err(ShaderPackError::FileTooLarge {
                        path: "shaders.properties".to_owned(),
                        bytes: text.len() as u64,
                    });
                }
                Self::parse(&text).map(Some)
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// block/item/entity.properties material-ID maps
// ---------------------------------------------------------------------------

/// One block/item/entity specifier: a normalized `namespace:name` plus
/// optional `prop=value` constraints (`:prop=val` suffixes and `nbt.*` keys).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdMatcher {
    /// Normalized `namespace:name` (bare names become `minecraft:name`).
    pub name: String,
    /// Property constraints; a matcher applies when every constraint is
    /// satisfied by the queried state.
    pub properties: BTreeMap<String, String>,
}

/// Deterministic material-ID table built from `block.properties`,
/// `item.properties` or `entity.properties`.
///
/// Grammar: `block.<id> = <ns:name[:prop=val...]> <ns:name...>`, with `\`
/// line continuations handled by the shared `.properties` reader. Lookups
/// scan IDs ascending, so the lowest declared ID wins for a name matched by
/// several entries.
#[derive(Clone, Debug, Default)]
pub struct IdMap {
    /// `(id, matchers)` sorted by id then declaration order.
    entries: Vec<(u16, Vec<IdMatcher>)>,
}

impl IdMap {
    fn parse(text: &str, file: &str, prefix: &str) -> Result<Self, ShaderPackError> {
        let mut by_id: BTreeMap<u16, Vec<IdMatcher>> = BTreeMap::new();
        for prop in parse_properties(text, file)? {
            let Some(id_s) = prop.key.strip_prefix(prefix) else {
                continue;
            };
            let id: u16 = id_s
                .parse()
                .map_err(|_| malformed_props(file, prop.line, "bad numeric id key"))?;
            let matchers = by_id.entry(id).or_default();
            for spec in prop.value.split_whitespace() {
                matchers.push(parse_id_matcher(spec, file, prop.line)?);
            }
        }
        Ok(Self {
            entries: by_id.into_iter().collect(),
        })
    }

    /// Parse `block.properties`.
    pub fn from_block_properties(text: &str) -> Result<Self, ShaderPackError> {
        Self::parse(text, "block.properties", "block.")
    }

    /// Parse `item.properties`.
    pub fn from_item_properties(text: &str) -> Result<Self, ShaderPackError> {
        Self::parse(text, "item.properties", "item.")
    }

    /// Parse `entity.properties`.
    pub fn from_entity_properties(text: &str) -> Result<Self, ShaderPackError> {
        Self::parse(text, "entity.properties", "entity.")
    }

    /// Look up a `name` (`ns:name` or bare `name`, which becomes
    /// `minecraft:name`) with state `properties`. Returns the lowest ID whose
    /// matcher name equals `name` and whose property constraints are a subset
    /// of `properties` (a constraint-free matcher always fits).
    pub fn lookup(
        &self,
        name: &str,
        properties: &BTreeMap<String, String>,
    ) -> Option<u16> {
        let norm = normalize_resource_name(name);
        for (id, matchers) in &self.entries {
            for m in matchers {
                if m.name == norm
                    && m.properties
                        .iter()
                        .all(|(k, v)| properties.get(k) == Some(v))
                {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Look up by name only (no state properties).
    pub fn lookup_name(&self, name: &str) -> Option<u16> {
        self.lookup(name, &BTreeMap::new())
    }

    /// Iterate `(id, matchers)` in deterministic order.
    pub fn entries(&self) -> &[(u16, Vec<IdMatcher>)] {
        &self.entries
    }
}

/// `stone` -> `minecraft:stone`; `ns:name` stays.
fn normalize_resource_name(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

fn parse_id_matcher(spec: &str, file: &str, line: usize) -> Result<IdMatcher, ShaderPackError> {
    let bad_spec = || malformed_props(file, line, format!("bad block/item specifier {spec:?}"));
    let parts: Vec<&str> = spec.split(':').collect();
    // Name portion = parts before the first part containing '='.
    let name_len = parts
        .iter()
        .position(|p| p.contains('='))
        .unwrap_or(parts.len());
    let name_parts = &parts[..name_len];
    if name_parts.iter().any(|p| p.is_empty()) {
        return Err(bad_spec());
    }
    let name = match name_parts {
        [n] => format!("minecraft:{n}"),
        [ns, n] => format!("{ns}:{n}"),
        _ => return Err(bad_spec()),
    };
    let mut properties = BTreeMap::new();
    for p in &parts[name_len..] {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| malformed_props(file, line, format!("bad property {p:?}")))?;
        properties.insert(k.to_owned(), v.to_owned());
    }
    Ok(IdMatcher { name, properties })
}

// ---------------------------------------------------------------------------
// #include preprocessor + directive extraction
// ---------------------------------------------------------------------------

/// A `#define` user option discovered in a shader source.
#[derive(Clone, Debug, PartialEq)]
pub struct ShaderOption {
    /// Option name.
    pub name: String,
    /// Value after the name (`#define FOG 0.5`), when present.
    pub value: Option<String>,
    /// `false` for `// #define FOG` (disabled but user-toggleable).
    pub enabled: bool,
}

/// Fully preprocessed shader translation unit plus extracted directives.
#[derive(Clone, Debug)]
pub struct PreprocessedShader {
    /// Canonical pack path this unit was compiled from.
    pub path: String,
    /// Source with all `#include`s expanded.
    pub source: String,
    /// `#version` number (e.g. `460`), when declared.
    pub glsl_version: Option<u32>,
    /// `#version` profile word (`core`, `compatibility`, ...).
    pub version_profile: Option<String>,
    /// `#define`/`// #define` options, by name (enabled wins over commented).
    pub defines: BTreeMap<String, ShaderOption>,
    /// Render-target token list from `/* RENDERTARGETS:... */` (preferred)
    /// or `/* DRAWBUFFERS:... */`.
    pub render_targets: Option<Vec<String>>,
    /// `const ivec3 workGroups = ivec3(x, y, z)`.
    pub work_groups: Option<[u32; 3]>,
    /// `const vec2 workGroupsRender = vec2(x, y)`.
    pub work_groups_render: Option<[f64; 2]>,
    /// Pack files that were `#include`d, sorted and deduplicated.
    pub included: Vec<String>,
}

/// Preprocess the shader at `path` (shader-root-relative, e.g.
/// `composite.fsh` or `world0/composite.fsh`): expand every `#include`
/// inside the pack root under [`MAX_INCLUDE_DEPTH`] with cycle detection and
/// a global expansion cap, then extract compiler directives.
///
/// `#include "file.glsl"` resolves against the including file's directory
/// first, then the shader root; `#include "/abs.glsl"` resolves from the
/// shader root. Any attempt to escape the root, any cycle, and any missing
/// file is a hard error.
pub fn preprocess_shader_source(
    source: &ShaderPackSource,
    path: &str,
) -> Result<PreprocessedShader, ShaderPackError> {
    validate_virtual_path(path)?;
    let mut state = PreprocessState {
        source,
        stack: Vec::new(),
        included: BTreeSet::new(),
        expansions: 0,
    };
    let text = state.expand(path, 0)?;
    let mut pp = extract_directives(&text);
    pp.path = path.to_owned();
    pp.source = text;
    pp.included = state.included.iter().cloned().collect();
    Ok(pp)
}

struct PreprocessState<'a> {
    source: &'a ShaderPackSource,
    stack: Vec<String>,
    included: BTreeSet<String>,
    expansions: usize,
}

impl<'a> PreprocessState<'a> {
    /// The file that pulled the current one in (`<entry>` at the root), used
    /// for `IncludeNotFound` diagnostics.
    fn including(&self) -> String {
        self.stack
            .last()
            .cloned()
            .unwrap_or_else(|| "<entry>".to_owned())
    }

    /// Expand `path` (canonical key) at `depth`; returns the expanded text.
    fn expand(&mut self, path: &str, depth: usize) -> Result<String, ShaderPackError> {
        if depth > MAX_INCLUDE_DEPTH {
            return Err(ShaderPackError::IncludeDepthExceeded {
                path: path.to_owned(),
            });
        }
        if self.stack.iter().any(|p| p == path) {
            return Err(ShaderPackError::IncludeCycle {
                path: path.to_owned(),
            });
        }
        let raw = self
            .source
            .read(path)
            .ok_or_else(|| ShaderPackError::IncludeNotFound {
                include: path.to_owned(),
                from: self.including(),
            })?;
        let text = String::from_utf8_lossy(raw).into_owned();
        let dir = dir_of(path);
        let commented = lines_in_block_comment(&text);
        self.stack.push(path.to_owned());
        let mut out = String::with_capacity(text.len());
        for (idx, line) in text.split('\n').enumerate() {
            let line_no = idx + 1;
            let include = if commented.get(idx).copied().unwrap_or(false) {
                None
            } else {
                parse_include_line(line)
            };
            match include {
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
                Some(Ok(spec)) => {
                    self.expansions += 1;
                    if self.expansions > MAX_TOTAL_INCLUDES {
                        return Err(ShaderPackError::TooManyIncludes {
                            count: self.expansions,
                        });
                    }
                    let resolved = self.resolve(&dir, &spec)?;
                    out.push_str(&self.expand(&resolved, depth + 1)?);
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    self.included.insert(resolved);
                }
                Some(Err(reason)) => {
                    return Err(ShaderPackError::MalformedShader {
                        path: path.to_owned(),
                        line: line_no,
                        reason,
                    });
                }
            }
        }
        self.stack.pop();
        Ok(out)
    }

    /// Resolve an `#include` spec to a canonical key: `/x` from the shader
    /// root, `x` relative to the including file's dir then the root.
    fn resolve(&self, dir: &str, spec: &str) -> Result<String, ShaderPackError> {
        let from = self.including();
        let candidates: Vec<String> = if let Some(abs) = spec.strip_prefix('/') {
            vec![normalize_relative(abs)?]
        } else {
            let mut c = Vec::with_capacity(2);
            if !dir.is_empty() {
                if let Ok(joined) = normalize_relative(&format!("{dir}/{spec}")) {
                    c.push(joined);
                }
            }
            if let Ok(root) = normalize_relative(spec) {
                c.push(root);
            }
            c
        };
        for cand in &candidates {
            if self.source.contains(cand) {
                return Ok(cand.clone());
            }
        }
        // Reject traversal explicitly even when the file simply does not
        // exist, so escapes are always reported as unsafe.
        if spec.contains("..") {
            return Err(unsafe_path(spec));
        }
        Err(ShaderPackError::IncludeNotFound {
            include: spec.to_owned(),
            from,
        })
    }
}

/// Directory part of a virtual path (`a/b/c.fsh` -> `a/b`, `c.fsh` -> ``).
fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_owned(),
        None => String::new(),
    }
}

/// If `line` is an `#include` directive, return `Some(Ok(spec))` or
/// `Some(Err(reason))` for a malformed one. `None` for everything else.
fn parse_include_line(line: &str) -> Option<Result<String, String>> {
    let t = line.trim_start();
    let rest = t.strip_prefix('#')?.trim_start();
    let rest = rest.strip_prefix("include")?;
    // `include` must be a full token (not `includefoo`).
    if !rest.is_empty() && !rest.starts_with([' ', '\t', '"', '<']) {
        return None;
    }
    let rest = rest.trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        match r.find('"') {
            Some(end) => {
                let spec = &r[..end];
                if r[end + 1..].trim().is_empty() {
                    Some(Ok(spec.to_owned()))
                } else {
                    Some(Err("trailing tokens after #include".to_owned()))
                }
            }
            None => Some(Err("unterminated #include string".to_owned())),
        }
    } else if rest.starts_with('<') {
        Some(Err("#include <...> is not supported".to_owned()))
    } else if rest.is_empty() {
        Some(Err("empty #include".to_owned()))
    } else {
        Some(Err("malformed #include".to_owned()))
    }
}

/// Lexical class of one emitted char.
#[derive(Clone, Copy, PartialEq)]
enum LexClass {
    /// Ordinary source text.
    Code,
    /// String-literal text, where comment markers are not special.
    Str,
    /// Second char of a string escape (never starts a new line).
    Escaped,
    /// Comment text, including the `//` and `/*` openers, but not a position
    /// inside an already-open block (so it cannot begin a comment line).
    Comment,
    /// Inside an already-open `/* */` block, its closing `*/` included.
    Block,
}

/// Walk `src`, reporting the [`LexClass`] of every char in order, and return
/// the class of the state left at end of input. Comment delimiters are
/// classed as part of the comment they open and the closing `*/` as part of
/// the block it closes, so a consumer can mask comments by offset.
fn scan_comments(src: &str, mut emit: impl FnMut(char, LexClass)) -> LexClass {
    let mut state = LexClass::Code;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            LexClass::Code => match c {
                '/' if chars.peek() == Some(&'/') => {
                    emit(c, LexClass::Comment);
                    emit(chars.next().unwrap_or('/'), LexClass::Comment);
                    state = LexClass::Comment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    emit(c, LexClass::Comment);
                    emit(chars.next().unwrap_or('*'), LexClass::Comment);
                    state = LexClass::Block;
                }
                '"' => {
                    emit(c, LexClass::Str);
                    state = LexClass::Str;
                }
                _ => emit(c, LexClass::Code),
            },
            LexClass::Comment => {
                if c == '\n' {
                    emit(c, LexClass::Code);
                    state = LexClass::Code;
                } else {
                    emit(c, LexClass::Comment);
                }
            }
            LexClass::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    emit(c, LexClass::Block);
                    emit(chars.next().unwrap_or('/'), LexClass::Block);
                    state = LexClass::Code;
                } else {
                    emit(c, LexClass::Block);
                }
            }
            _ => {
                emit(c, LexClass::Str);
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        emit(escaped, LexClass::Escaped);
                    }
                } else if c == '"' {
                    state = LexClass::Code;
                }
            }
        }
    }
    state
}

/// For each `\n`-separated line, whether it starts inside a `/* */` block
/// comment. Lines inside a block comment cannot contain directives.
fn lines_in_block_comment(src: &str) -> Vec<bool> {
    let mut out = vec![false];
    let mut at_line_start = false;
    let end = scan_comments(src, |c, class| {
        if std::mem::take(&mut at_line_start) {
            out.push(class == LexClass::Block);
        }
        if c == '\n' && class != LexClass::Escaped {
            at_line_start = true;
        }
    });
    if at_line_start {
        out.push(end == LexClass::Block);
    }
    out
}

/// Remove `//` and `/* */` comments while preserving newlines (line numbers
/// stay stable for diagnostics). String literals are respected.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    scan_comments(src, |c, class| {
        let comment = matches!(class, LexClass::Comment | LexClass::Block);
        out.push(if comment && c != '\n' { ' ' } else { c });
    });
    out
}

/// Scan raw source for `/* DRAWBUFFERS:tokens */` / `/* RENDERTARGETS:tokens */`
/// markers. RENDERTARGETS wins when both exist.
fn extract_render_targets(src: &str) -> Option<Vec<String>> {
    let mut drawbuffers = None;
    let mut rendertargets = None;
    let mut rest = src;
    while let Some(start) = rest.find("/*") {
        let body_start = start + 2;
        let Some(end) = rest[body_start..].find("*/") else {
            break;
        };
        let body = &rest[body_start..body_start + end];
        let body = body.trim_start();
        for (marker, slot) in [
            ("RENDERTARGETS:", &mut rendertargets),
            ("DRAWBUFFERS:", &mut drawbuffers),
        ] {
            if slot.is_none() {
                *slot = body.strip_prefix(marker).map(parse_target_list);
            }
        }
        rest = &rest[body_start + end + 2..];
    }
    rendertargets.or(drawbuffers)
}

/// `0,1,2` -> ["0","1","2"]; `012` -> ["0","1","2"]. Tokens are alphanumeric.
fn parse_target_list(list: &str) -> Vec<String> {
    let list = list.trim();
    if list.contains(',') {
        list.split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(str::to_owned)
            .collect()
    } else {
        list.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_string())
            .collect()
    }
}

/// Split the argument list of a `const <ty> <name> = <ty>(...)` declaration
/// from comment-stripped source. `None` unless the marker, `=`, constructor
/// and parentheses all appear.
fn work_group_args<'a>(src: &'a str, marker: &str, ty: &str) -> Option<Vec<&'a str>> {
    let pos = src.find(marker)?;
    let after = src[pos + marker.len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix(ty)?.trim_start();
    let after = after.strip_prefix('(')?;
    let close = after.find(')')?;
    Some(after[..close].split(',').collect())
}

/// Parse `const ivec3 workGroups = ivec3(a, b, c)` from comment-stripped
/// source. Literal integers only; anything else yields `None`.
fn extract_work_groups(src: &str) -> Option<[u32; 3]> {
    let args = work_group_args(src, "const ivec3 workGroups", "ivec3")?;
    if args.len() != 3 {
        return None;
    }
    let mut out = [0u32; 3];
    for (slot, a) in out.iter_mut().zip(args.iter()) {
        let t = a.trim().trim_end_matches('u').trim_end_matches('U');
        *slot = t.parse::<u32>().ok()?;
    }
    Some(out)
}

/// Parse `const vec2 workGroupsRender = vec2(x, y)`.
fn extract_work_groups_render(src: &str) -> Option<[f64; 2]> {
    let args = work_group_args(src, "const vec2 workGroupsRender", "vec2")?;
    if args.len() != 2 {
        return None;
    }
    let x = args[0].trim().trim_end_matches(['f', 'F']).parse::<f64>().ok()?;
    let y = args[1].trim().trim_end_matches(['f', 'F']).parse::<f64>().ok()?;
    Some([x, y])
}

/// Parse the body of a `#define` directive into `(name, value)`. `None` when
/// the name is empty or not an identifier, so malformed defines are ignored
/// exactly like non-defines.
fn parse_define_body(body: &str) -> Option<(String, Option<String>)> {
    let mut toks = body.trim_start().splitn(2, char::is_whitespace);
    let name = toks.next().unwrap_or_default().trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let value = toks.next().map(|s| s.trim().to_owned());
    Some((name.to_owned(), value))
}

/// Extract `#version`, `#define` options and the geometry/workgroup
/// constants from expanded source. `raw` is the expanded source; comments
/// are stripped for directive parsing but `// #define` options are picked up
/// from the raw text.
fn extract_directives(raw: &str) -> PreprocessedShader {
    let stripped = strip_comments(raw);
    let mut glsl_version = None;
    let mut version_profile = None;
    let mut defines: BTreeMap<String, ShaderOption> = BTreeMap::new();
    for line in stripped.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(v) = rest.strip_prefix("version") {
            if glsl_version.is_none() {
                let mut toks = v.split_whitespace();
                if let Some(n) = toks.next().and_then(|t| t.parse::<u32>().ok()) {
                    glsl_version = Some(n);
                    version_profile = toks.next().map(str::to_owned);
                }
            }
        } else if let Some(v) = rest.strip_prefix("define") {
            if !v.is_empty() && !v.starts_with([' ', '\t']) {
                continue;
            }
            let Some((name, value)) = parse_define_body(v) else {
                continue;
            };
            defines.insert(
                name.clone(),
                ShaderOption { name, value, enabled: true },
            );
        }
    }
    // `// #define OPTION [value]` — disabled options still surface in the UI.
    for line in raw.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("//") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('#') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(v) = rest.strip_prefix("define") else {
            continue;
        };
        if !v.is_empty() && !v.starts_with([' ', '\t']) {
            continue;
        }
        let Some((name, value)) = parse_define_body(v) else {
            continue;
        };
        if defines.contains_key(&name) {
            continue;
        }
        defines.insert(
            name.clone(),
            ShaderOption { name, value, enabled: false },
        );
    }
    PreprocessedShader {
        path: String::new(),
        source: String::new(),
        glsl_version,
        version_profile,
        defines,
        render_targets: extract_render_targets(raw),
        work_groups: extract_work_groups(&stripped),
        work_groups_render: extract_work_groups_render(&stripped),
        included: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Pipeline planner + capability validation
// ---------------------------------------------------------------------------

/// GLSL shader stage kinds recognized in program filenames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShaderKind {
    /// `.vsh`
    Vertex,
    /// `.tcs`
    TessControl,
    /// `.tes`
    TessEval,
    /// `.gsh`
    Geometry,
    /// `.fsh`
    Fragment,
    /// `.csh`
    Compute,
}

impl ShaderKind {
    fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext {
            "vsh" => Self::Vertex,
            "tcs" => Self::TessControl,
            "tes" => Self::TessEval,
            "gsh" => Self::Geometry,
            "fsh" => Self::Fragment,
            "csh" => Self::Compute,
            _ => return None,
        })
    }
}

/// Render pipeline stages in Iris 1.7+ execution order.
///
/// `DistantHorizons` `dh_*` programs render after the vanilla gbuffers and
/// before the deferred/composite chain (the DH pass draws into the main
/// framebuffer the composite passes then process).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PipelineStage {
    /// `begin*.csh` — frame setup compute.
    Begin,
    /// `shadow*` (shadow-map render) then `shadowcomp*` passes.
    Shadow,
    /// `prepare*` passes.
    Prepare,
    /// `gbuffers_*` geometry passes.
    Gbuffers,
    /// `dh_*` DistantHorizons terrain/water passes.
    DistantHorizons,
    /// `deferred*` full-screen passes.
    Deferred,
    /// `composite*` full-screen passes.
    Composite,
    /// `final` full-screen pass.
    Final,
}

impl PipelineStage {
    /// All stages in execution order.
    pub const ALL: [PipelineStage; 8] = [
        Self::Begin,
        Self::Shadow,
        Self::Prepare,
        Self::Gbuffers,
        Self::DistantHorizons,
        Self::Deferred,
        Self::Composite,
        Self::Final,
    ];
}

/// How a pass executes on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassKind {
    /// `.csh` compute dispatch.
    Compute,
    /// vertex + fragment (+optional geometry/tessellation) draw.
    Graphics,
}

/// Which dimension's `world<N>` subdirectory to read programs from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackDimension {
    /// `world0` — the Overworld.
    Overworld,
    /// `world-1` — the Nether.
    Nether,
    /// `world1` — the End.
    End,
    /// `world<N>` — any other Iris dimension id.
    Custom(i32),
}

impl PackDimension {
    /// Directory name inside the pack (`world0`, `world-1`, ...).
    pub fn dir_name(self) -> String {
        match self {
            Self::Overworld => "world0".to_owned(),
            Self::Nether => "world-1".to_owned(),
            Self::End => "world1".to_owned(),
            Self::Custom(n) => format!("world{n}"),
        }
    }
}

/// One ordered render pass (a program name plus its shader stages).
#[derive(Clone, Debug)]
pub struct ShaderPass {
    /// Program name (`gbuffers_terrain`, `composite1`, `begin`, `final`, ...).
    pub name: String,
    /// Pipeline stage this pass belongs to.
    pub stage: PipelineStage,
    /// Compute or graphics.
    pub kind: PassKind,
    /// Preprocessed shader units by stage kind.
    pub shaders: BTreeMap<ShaderKind, PreprocessedShader>,
    /// Render-target list from the fragment/compute shader's
    /// `RENDERTARGETS`/`DRAWBUFFERS` marker.
    pub render_targets: Option<Vec<String>>,
    /// Compute workgroup counts from `const ivec3 workGroups`.
    pub work_groups: Option<[u32; 3]>,
}

/// Report produced by [`plan_shader_pipeline`] describing whether the GPU
/// capability profile can run the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderCompatibilityReport {
    /// Every requirement is satisfied.
    Compatible,
    /// One or more requirements are unmet; `missing` lists human-readable
    /// reasons in deterministic order.
    MissingCapabilities {
        /// Reason strings (e.g. `compute shaders required by begin.csh`,
        /// `unknown Iris feature: FOO`).
        missing: Vec<String>,
    },
}

impl ShaderCompatibilityReport {
    /// Whether the pack can run on this GPU profile.
    pub fn is_compatible(&self) -> bool {
        matches!(self, Self::Compatible)
    }

    /// Reason strings, or empty when compatible.
    pub fn missing(&self) -> &[String] {
        match self {
            Self::Compatible => &[],
            Self::MissingCapabilities { missing } => missing,
        }
    }
}

/// GPU capability profile the pack is validated against.
#[derive(Clone, Debug, PartialEq)]
pub struct GpuShaderCaps {
    /// Maximum GLSL `#version` the backend accepts (e.g. `460`, `410`).
    pub max_glsl_version: u32,
    /// Compute dispatch support (`supports_compute`).
    pub supports_compute: bool,
    /// Shader storage buffer (`bufferObject.*`) support.
    pub supports_ssbo: bool,
    /// `image.*` custom image unit support.
    pub supports_custom_images: bool,
    /// Largest single `bufferObject` byte size.
    pub max_ssbo_bytes: u64,
    /// Largest `RENDERTARGETS`/`DRAWBUFFERS` list length.
    pub max_color_attachments: u32,
}

impl GpuShaderCaps {
    /// Vulkan/wgpu-class backend: GLSL 4.60, compute + SSBO + custom images,
    /// 128 MiB storage buffers, 8 color attachments.
    pub fn modern_compute() -> Self {
        Self {
            max_glsl_version: 460,
            supports_compute: true,
            supports_ssbo: true,
            supports_custom_images: true,
            max_ssbo_bytes: 128 * 1024 * 1024,
            max_color_attachments: 8,
        }
    }

    /// Legacy OpenGL 4.1-class low-spec profile: GLSL 4.10, no compute, no
    /// SSBO, no custom images — packs requiring those fail closed.
    pub fn legacy_opengl() -> Self {
        Self {
            max_glsl_version: 410,
            supports_compute: false,
            supports_ssbo: false,
            supports_custom_images: false,
            max_ssbo_bytes: 0,
            max_color_attachments: 4,
        }
    }
}

/// A fully-planned shaderpack: ordered passes, parsed properties, material
/// ID maps, uniform/buffer requirements, pack family classification, and
/// the GPU compatibility verdict.
#[derive(Clone, Debug)]
pub struct ShaderPipelinePlan {
    /// Dimension the plan was built for.
    pub dimension: PackDimension,
    /// Broad shaderpack architecture family detected from passes and properties.
    pub family: ShaderPackFamily,
    /// Passes in execution order (stage, then program order).
    pub passes: Vec<ShaderPass>,
    /// Parsed `shaders.properties`, when present.
    pub properties: Option<ShaderProperties>,
    /// `block.properties` material IDs.
    pub block_ids: Option<IdMap>,
    /// `item.properties` material IDs.
    pub item_ids: Option<IdMap>,
    /// `entity.properties` material IDs.
    pub entity_ids: Option<IdMap>,
    /// Uniform, buffer, dual-hand lighting and DistantHorizons LOD requirements
    /// discovered across all preprocessed passes.
    pub uniforms: ShaderUniformRequirements,
    /// Compatibility verdict against the supplied caps.
    pub compatibility: ShaderCompatibilityReport,
}

impl ShaderPipelinePlan {
    /// Passes belonging to `stage`, in order.
    pub fn passes_in_stage(
        &self,
        stage: PipelineStage,
    ) -> impl Iterator<Item = &ShaderPass> {
        self.passes.iter().filter(move |p| p.stage == stage)
    }

    /// Stages that contain at least one pass, in execution order.
    pub fn stages(&self) -> Vec<PipelineStage> {
        self.passes
            .iter()
            .map(|p| p.stage)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Whether the plan is compatible with the GPU profile it was checked
    /// against.
    pub fn is_compatible(&self) -> bool {
        self.compatibility.is_compatible()
    }

    /// Whether this pack defines any DistantHorizons/LOD passes
    /// (`dh_terrain`, `dh_water`, `dh_shadow`, ...).
    pub fn has_dh_lod_passes(&self) -> bool {
        self.passes.iter().any(|p| p.name.starts_with("dh_"))
    }
}

/// Canonical OptiFine/Iris gbuffers program order: known programs sort here
/// first, unknown `gbuffers_*` names sort after, alphabetically.
const GBUFFERS_ORDER: &[&str] = &[
    "gbuffers_basic",
    "gbuffers_line",
    "gbuffers_textured",
    "gbuffers_textured_lit",
    "gbuffers_skybasic",
    "gbuffers_skytextured",
    "gbuffers_clouds",
    "gbuffers_terrain",
    "gbuffers_damagedblock",
    "gbuffers_block",
    "gbuffers_beaconbeam",
    "gbuffers_item",
    "gbuffers_entities",
    "gbuffers_entities_glowing",
    "gbuffers_armor_glint",
    "gbuffers_spidereyes",
    "gbuffers_hand",
    "gbuffers_weather",
    "gbuffers_particles",
    "gbuffers_lightning",
    "gbuffers_hand_water",
    "gbuffers_water",
];

/// `name` == `prefix` or `name` == `prefix` + decimal digits.
fn prefixed_digits(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

/// Map a program basename to a pipeline stage.
fn program_stage(name: &str) -> Option<PipelineStage> {
    if name == "final" {
        Some(PipelineStage::Final)
    } else if prefixed_digits(name, "composite") {
        Some(PipelineStage::Composite)
    } else if prefixed_digits(name, "deferred") {
        Some(PipelineStage::Deferred)
    } else if prefixed_digits(name, "prepare") {
        Some(PipelineStage::Prepare)
    } else if prefixed_digits(name, "begin") {
        Some(PipelineStage::Begin)
    } else if prefixed_digits(name, "shadowcomp")
        || prefixed_digits(name, "shadow")
        || name == "dh_shadow"
    {
        Some(PipelineStage::Shadow)
    } else if name.starts_with("gbuffers_") {
        Some(PipelineStage::Gbuffers)
    } else if name.starts_with("dh_") {
        Some(PipelineStage::DistantHorizons)
    } else {
        None
    }
}

/// Sort key within a stage: shadow-map programs (`shadow*` -> `dh_shadow` ->
/// `shadowcomp*`), known gbuffers programs in canonical order, `dh_terrain`
/// before `dh_water`, numbered suffixes numerically, then by name.
fn pass_sort_key(pass_name: &str, stage: PipelineStage) -> (u32, u32) {
    let group = match (stage, pass_name) {
        (PipelineStage::Shadow, "dh_shadow") => 1,
        (PipelineStage::Shadow, name) if name.starts_with("shadowcomp") => 2,
        _ => 0,
    };
    let order = if stage == PipelineStage::Gbuffers {
        GBUFFERS_ORDER
            .iter()
            .position(|&n| n == pass_name)
            .map(|i| i as u32)
            .unwrap_or(u32::MAX)
    } else if stage == PipelineStage::DistantHorizons {
        match pass_name {
            "dh_terrain" => 0,
            "dh_water" => 1,
            _ => u32::MAX,
        }
    } else {
        u32::MAX
    };
    (group, order)
}

/// Discover every program, order the stages, preprocess every shader file
/// and validate the result plus `iris.features.required` against `caps`.
///
/// Files inside `world<dim>/` override same-named root files for the chosen
/// `dimension`; `shaders.properties` and the `*.properties` ID maps are read
/// from the shader root only.
pub fn plan_shader_pipeline(
    source: &ShaderPackSource,
    dimension: PackDimension,
    caps: &GpuShaderCaps,
) -> Result<ShaderPipelinePlan, ShaderPackError> {
    plan_shader_pipeline_with_options(
        source,
        dimension,
        caps,
        &ShaderOptionOverrides::default(),
    )
}

/// Discovered shaders for one program: its pipeline stage plus the file that
/// provides each shader stage kind.
type ProgramFiles = (PipelineStage, BTreeMap<ShaderKind, String>);

/// Map every program basename visible to `dim_dir` onto its stage and shader
/// files. Root-level programs are always included; same-named files inside
/// `world<dim>/` replace them.
fn discover_programs(source: &ShaderPackSource, dim_dir: &str) -> BTreeMap<String, ProgramFiles> {
    let mut programs: BTreeMap<String, ProgramFiles> = BTreeMap::new();
    for path in source.list() {
        let name = match path.rsplit_once('/') {
            Some((dir, file)) if dir == dim_dir => file,
            Some(_) => continue,
            None => path,
        };
        let Some((base, ext)) = name.rsplit_once('.') else {
            continue;
        };
        let Some(kind) = ShaderKind::from_extension(ext) else {
            continue;
        };
        let Some(stage) = program_stage(base) else {
            continue;
        };
        programs
            .entry(base.to_owned())
            .or_insert_with(|| (stage, BTreeMap::new()))
            .1
            .insert(kind, path.to_owned());
    }
    programs
}

/// Validate that `files` forms a complete program: a compute program is a
/// lone `.csh`, a graphics program has both `.vsh` and `.fsh`.
fn check_program_shape(
    name: &str,
    files: &BTreeMap<ShaderKind, String>,
) -> Result<(), ShaderPackError> {
    let incomplete = |reason: &str| ShaderPackError::IncompleteProgram {
        name: name.to_owned(),
        reason: reason.to_owned(),
    };
    let has_compute = files.contains_key(&ShaderKind::Compute);
    let has_graphics = files
        .keys()
        .any(|k| matches!(k, ShaderKind::Vertex | ShaderKind::Fragment));
    if has_compute && files.len() > 1 {
        return Err(incomplete("compute program mixes .csh with graphics stages"));
    }
    let complete = files.contains_key(&ShaderKind::Vertex)
        && files.contains_key(&ShaderKind::Fragment);
    if has_graphics && !complete {
        return Err(incomplete("graphics program needs both .vsh and .fsh"));
    }
    Ok(())
}

/// Preprocess every shader file of every program and assemble the passes
/// (the caller orders them).
fn build_passes(
    source: &ShaderPackSource,
    programs: BTreeMap<String, ProgramFiles>,
    overrides: &BTreeMap<String, Option<String>>,
) -> Result<Vec<ShaderPass>, ShaderPackError> {
    let mut passes = Vec::with_capacity(programs.len());
    for (name, (stage, files)) in programs {
        check_program_shape(&name, &files)?;
        let mut shaders = BTreeMap::new();
        for (kind, path) in &files {
            let mut pp = preprocess_shader_source(source, path)?;
            if !overrides.is_empty() {
                apply_option_overrides_to_unit(&mut pp, overrides);
            }
            shaders.insert(*kind, pp);
        }
        let render_targets = shaders
            .get(&ShaderKind::Fragment)
            .or_else(|| shaders.get(&ShaderKind::Compute))
            .and_then(|s| s.render_targets.clone());
        let work_groups = shaders
            .get(&ShaderKind::Compute)
            .and_then(|s| s.work_groups);
        let kind = if files.contains_key(&ShaderKind::Compute) {
            PassKind::Compute
        } else {
            PassKind::Graphics
        };
        passes.push(ShaderPass {
            name,
            stage,
            kind,
            shaders,
            render_targets,
            work_groups,
        });
    }
    Ok(passes)
}

/// Like [`plan_shader_pipeline`], additionally applying a named
/// `profile.<NAME>` (from `shaders.properties`) and explicit per-option
/// overrides to every `#define`/`// #define` option in the preprocessed
/// shader sources before capability and uniform inspection.
pub fn plan_shader_pipeline_with_options(
    source: &ShaderPackSource,
    dimension: PackDimension,
    caps: &GpuShaderCaps,
    overrides: &ShaderOptionOverrides,
) -> Result<ShaderPipelinePlan, ShaderPackError> {
    // --- pack-level properties first so profile.<NAME> can drive options --
    let properties = ShaderProperties::load(source)?;
    let resolved_overrides = resolve_option_overrides(properties.as_ref(), overrides);

    // --- discover, preprocess and order every program ---------------------
    let programs = discover_programs(source, &dimension.dir_name());
    if programs.is_empty() {
        return Err(ShaderPackError::EmptyPack);
    }
    let mut passes = build_passes(source, programs, &resolved_overrides)?;
    // --- deterministic ordering ------------------------------------------
    passes.sort_by(|a, b| {
        a.stage
            .cmp(&b.stage)
            .then_with(|| pass_sort_key(&a.name, a.stage).cmp(&pass_sort_key(&b.name, b.stage)))
            .then_with(|| program_num(&a.name).cmp(&program_num(&b.name)))
            .then_with(|| a.name.cmp(&b.name))
    });
    let load_map = |file: &str, f: fn(&str) -> Result<IdMap, ShaderPackError>| {
        match source.read_text(file) {
            Some(t) => f(&t).map(Some),
            None => Ok(None),
        }
    };
    let block_ids = load_map("block.properties", IdMap::from_block_properties)?;
    let item_ids = load_map("item.properties", IdMap::from_item_properties)?;
    let entity_ids = load_map("entity.properties", IdMap::from_entity_properties)?;
    let uniforms = analyze_uniform_requirements(&passes);
    let family = classify_pack_family(&passes, properties.as_ref(), &uniforms);
    let compatibility = validate_capabilities(&passes, properties.as_ref(), caps);
    Ok(ShaderPipelinePlan {
        dimension,
        family,
        passes,
        properties,
        block_ids,
        item_ids,
        entity_ids,
        uniforms,
        compatibility,
    })
}

/// Trailing digits of a numbered program name (`composite12` -> 12).
fn program_num(name: &str) -> u32 {
    name.rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(0)
}

/// Deterministic compatibility verdict from the accumulated reasons.
fn report_from_missing(mut missing: Vec<String>) -> ShaderCompatibilityReport {
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        ShaderCompatibilityReport::Compatible
    } else {
        ShaderCompatibilityReport::MissingCapabilities { missing }
    }
}

/// Record a color-attachment list that exceeds the profile's limit.
fn check_color_attachments(
    pass: &str,
    count: usize,
    caps: &GpuShaderCaps,
    missing: &mut Vec<String>,
) {
    if count as u32 > caps.max_color_attachments {
        missing.push(format!(
            "{pass}: {count} color attachments required (max {})",
            caps.max_color_attachments
        ));
    }
}

/// Validate passes + declared properties + required features against caps.
fn validate_capabilities(
    passes: &[ShaderPass],
    properties: Option<&ShaderProperties>,
    caps: &GpuShaderCaps,
) -> ShaderCompatibilityReport {
    let mut missing = Vec::new();
    let mut need_compute = false;
    let mut need_ssbo = false;
    let mut need_images = false;
    if let Some(props) = properties {
        for feat in &props.required_features {
            match feat {
                IrisFeature::ComputeShaders => need_compute = true,
                IrisFeature::Ssbo => need_ssbo = true,
                IrisFeature::CustomImages => need_images = true,
                // Engine-side features this renderer implements; they impose
                // no GPU capability beyond what the passes already require.
                IrisFeature::EntityTranslucent
                | IrisFeature::SeparateHardwareSamplers
                | IrisFeature::PerBufferBlending => {}
                IrisFeature::Unknown(name) => {
                    missing.push(format!("unknown Iris feature required: {name}"));
                }
            }
        }
        for spec in props.ssbos.values() {
            need_ssbo = true;
            if spec.byte_size > caps.max_ssbo_bytes {
                missing.push(format!(
                    "bufferObject.{} needs {} bytes (limit {})",
                    spec.binding, spec.byte_size, caps.max_ssbo_bytes
                ));
            }
        }
        if !props.images.is_empty() {
            need_images = true;
        }
    }
    for pass in passes {
        if pass.kind == PassKind::Compute {
            need_compute = true;
        }
        for shader in pass.shaders.values() {
            if let Some(v) = shader.glsl_version {
                if v > caps.max_glsl_version {
                    missing.push(format!(
                        "{}: GLSL {} required (max {})",
                        shader.path, v, caps.max_glsl_version
                    ));
                }
            }
        }
        if let Some(targets) = &pass.render_targets {
            check_color_attachments(&pass.name, targets.len(), caps, &mut missing);
        }
    }
    if need_compute && !caps.supports_compute {
        missing.push("compute shaders required".to_owned());
    }
    if need_ssbo && !caps.supports_ssbo {
        missing.push("shader storage buffers (SSBO) required".to_owned());
    }
    if need_images && !caps.supports_custom_images {
        missing.push("custom image units required".to_owned());
    }
    report_from_missing(missing)
}

// ---------------------------------------------------------------------------
// Broad pack family classification, uniform analysis & lang/*.lang UI strings
// ---------------------------------------------------------------------------

/// High-level shaderpack family inferred from passes, GLSL versions, and
/// pack manifest. Used for diagnostics and pipeline routing — never restricts
/// a pack by brand name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShaderPackFamily {
    /// Classic OptiFine-style pack (GLSL 120..330, `gbuffers_*`/`composite*`/
    /// `final`, `colortex`/`depthtex`/`shadowtex`, `DRAWBUFFERS`, no compute,
    /// no SSBO, no custom images).
    ClassicOptiFine,
    /// Next-gen Iris 1.7..1.11+ pack (GLSL 430..460, `.csh` compute passes,
    /// `bufferObject.*` SSBOs, `image.*` custom images, `begin`/`shadowcomp`/
    /// `prepare`/`deferred`, or `RENDERTARGETS`).
    IrisModern,
    /// OptiFine/Iris pack that additionally supplies DistantHorizons LOD
    /// passes (`dh_terrain`, `dh_water`, `dh_shadow`) for native Megu LOD
    /// integration.
    DistantHorizonsHybrid,
    /// Megu native WGSL pack (`megu-shader.toml` + `.wgsl` modules) with zero
    /// GLSL translation overhead on Vulkan/Metal/DX12.
    MeguNativeWgsl,
}

impl ShaderPackFamily {
    /// Stable lowercase identifier for logs and JSON reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClassicOptiFine => "classic-optifine",
            Self::IrisModern => "iris-modern",
            Self::DistantHorizonsHybrid => "distant-horizons-hybrid",
            Self::MeguNativeWgsl => "megu-native-wgsl",
        }
    }
}

/// Standard uniform, attachment, dual-hand lighting and LOD inputs referenced
/// across a shaderpack's preprocessed passes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderUniformRequirements {
    /// Referenced color attachments (`colortex0`..`colortex15`, normalized
    /// from legacy `gcolor`/`gdepth`/`gnormal`/`composite`/`gaux1..4` aliases).
    pub color_attachments: BTreeSet<u8>,
    /// Referenced depth attachments (`depthtex0`..`depthtex2`).
    pub depth_attachments: BTreeSet<u8>,
    /// Referenced shadow depth maps (`shadowtex0`, `shadowtex1`, `shadow`).
    pub shadow_depth_attachments: BTreeSet<u8>,
    /// Referenced shadow color buffers (`shadowcolor0`, `shadowcolor1`).
    pub shadow_color_attachments: BTreeSet<u8>,
    /// Whether any pass reads main-hand held light (`heldItemId` /
    /// `heldBlockLightValue`).
    pub uses_main_hand_light: bool,
    /// Whether any pass reads off-hand held light (`heldItemId2` /
    /// `heldBlockLightValue2`).
    pub uses_off_hand_light: bool,
    /// Whether any pass reads DistantHorizons/LOD uniforms (`dhDepthTex0/1`,
    /// `dhRenderDistance`, `dhNearPlane`, `dhFarPlane`) or defines a `dh_*`
    /// program.
    pub uses_dh_lod: bool,
    /// Highest GLSL `#version` observed across all passes (`None` if undeclared).
    pub max_glsl_version: Option<u32>,
    /// Lowest GLSL `#version` observed across all passes (`None` if undeclared).
    pub min_glsl_version: Option<u32>,
}

impl ShaderUniformRequirements {
    /// True when the pack uses both main-hand and off-hand dynamic light
    /// uniforms (Iris 1.7+ dual-hand lighting).
    pub const fn uses_dual_hand_light(&self) -> bool {
        self.uses_main_hand_light && self.uses_off_hand_light
    }
}

/// Whether `src` references `ident` as a whole GLSL identifier (so `gcolor`
/// does not match inside `gcolor2`).
fn contains_ident(src: &str, ident: &str) -> bool {
    !ident.is_empty()
        && src
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|tok| tok == ident)
}

/// Insert the index of every `prefix<idx>` identifier referenced in `src`.
fn collect_ident_indices(
    src: &str,
    prefix: &str,
    range: std::ops::RangeInclusive<u8>,
    out: &mut BTreeSet<u8>,
) {
    for idx in range {
        if contains_ident(src, &format!("{prefix}{idx}")) {
            out.insert(idx);
        }
    }
}

fn analyze_uniform_requirements(passes: &[ShaderPass]) -> ShaderUniformRequirements {
    const LEGACY_COLOR_ALIASES: &[(&str, u8)] = &[
        ("gcolor", 0),
        ("gdepth", 1),
        ("gnormal", 2),
        ("composite", 3),
        ("gaux1", 4),
        ("gaux2", 5),
        ("gaux3", 6),
        ("gaux4", 7),
    ];
    const MAIN_HAND_LIGHT: &[&str] = &["heldItemId", "heldBlockLightValue"];
    const OFF_HAND_LIGHT: &[&str] = &["heldItemId2", "heldBlockLightValue2"];
    const DH_LOD_IDENTS: &[&str] = &[
        "dhDepthTex0",
        "dhDepthTex1",
        "dhRenderDistance",
        "dhNearPlane",
        "dhFarPlane",
    ];
    let mut req = ShaderUniformRequirements::default();
    for pass in passes {
        if pass.name.starts_with("dh_") {
            req.uses_dh_lod = true;
        }
        for shader in pass.shaders.values() {
            if let Some(v) = shader.glsl_version {
                req.max_glsl_version = Some(req.max_glsl_version.map_or(v, |m| m.max(v)));
                req.min_glsl_version = Some(req.min_glsl_version.map_or(v, |m| m.min(v)));
            }
            let s = shader.source.as_str();
            collect_ident_indices(s, "colortex", 0..=15, &mut req.color_attachments);
            for &(alias, idx) in LEGACY_COLOR_ALIASES {
                if contains_ident(s, alias) {
                    req.color_attachments.insert(idx);
                }
            }
            collect_ident_indices(s, "depthtex", 0..=2, &mut req.depth_attachments);
            collect_ident_indices(s, "shadowtex", 0..=1, &mut req.shadow_depth_attachments);
            collect_ident_indices(s, "shadowcolor", 0..=1, &mut req.shadow_color_attachments);
            if contains_ident(s, "shadow") && contains_ident(s, "sampler2D") {
                req.shadow_depth_attachments.insert(0);
            }
            if MAIN_HAND_LIGHT.iter().any(|id| contains_ident(s, id)) {
                req.uses_main_hand_light = true;
            }
            if OFF_HAND_LIGHT.iter().any(|id| contains_ident(s, id)) {
                req.uses_off_hand_light = true;
            }
            if DH_LOD_IDENTS.iter().any(|id| contains_ident(s, id)) {
                req.uses_dh_lod = true;
            }
        }
    }
    req
}

fn classify_pack_family(
    passes: &[ShaderPass],
    properties: Option<&ShaderProperties>,
    uniforms: &ShaderUniformRequirements,
) -> ShaderPackFamily {
    if passes.iter().any(|p| p.name.starts_with("dh_")) || uniforms.uses_dh_lod {
        return ShaderPackFamily::DistantHorizonsHybrid;
    }
    let has_modern_stage = passes.iter().any(|p| {
        p.kind == PassKind::Compute
            || matches!(p.stage, PipelineStage::Begin | PipelineStage::Prepare)
            || p.name.starts_with("shadowcomp")
    });
    let has_modern_props = properties.is_some_and(|p| {
        !p.ssbos.is_empty() || !p.images.is_empty() || !p.required_features.is_empty()
    });
    let has_modern_glsl = uniforms.max_glsl_version.is_some_and(|v| v >= 430);
    if has_modern_stage || has_modern_props || has_modern_glsl {
        ShaderPackFamily::IrisModern
    } else {
        ShaderPackFamily::ClassicOptiFine
    }
}

/// Option & profile overrides applied before shader preprocessing completes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderOptionOverrides {
    /// Optional `profile.<NAME>` from `shaders.properties` to activate first.
    pub profile: Option<String>,
    /// Explicit per-option overrides (`Some(val)` sets `#define NAME val`,
    /// `Some("")` sets `#define NAME`, `None` disables to `// #define NAME`).
    pub options: BTreeMap<String, Option<String>>,
}

fn resolve_option_overrides(
    props: Option<&ShaderProperties>,
    overrides: &ShaderOptionOverrides,
) -> BTreeMap<String, Option<String>> {
    let mut resolved = BTreeMap::new();
    if let (Some(props), Some(profile_name)) = (props, overrides.profile.as_deref()) {
        if let Some(profile) = props.profiles.get(profile_name) {
            for tok in &profile.tokens {
                if tok.starts_with("program:") {
                    continue;
                }
                if let Some(stripped) = tok.strip_prefix('!') {
                    if !stripped.is_empty() {
                        resolved.insert(stripped.to_owned(), None);
                    }
                } else if let Some((k, v)) = tok.split_once('=') {
                    resolved.insert(k.trim().to_owned(), Some(v.trim().to_owned()));
                } else if !tok.is_empty() {
                    resolved.insert(tok.clone(), Some(String::new()));
                }
            }
        }
    }
    for (k, v) in &overrides.options {
        resolved.insert(k.clone(), v.clone());
    }
    resolved
}

/// Split a `#define ...` or `// #define ...` line into `(commented, body)`,
/// where `body` is the text after the directive keyword. `None` for every
/// other line.
fn define_line(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    let (commented, rest) = match trimmed.strip_prefix("//") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };
    let body = rest.strip_prefix('#')?.trim_start().strip_prefix("define")?;
    body.starts_with([' ', '\t']).then(|| (commented, body.trim_start()))
}

fn apply_option_overrides_to_unit(
    unit: &mut PreprocessedShader,
    overrides: &BTreeMap<String, Option<String>>,
) {
    let mut new_lines = Vec::new();
    let mut changed = false;
    for line in unit.source.split('\n') {
        let Some((was_commented, body)) = define_line(line) else {
            new_lines.push(line.to_owned());
            continue;
        };
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let Some(override_val) = overrides.get(&name) else {
            new_lines.push(line.to_owned());
            continue;
        };
        changed = true;
        new_lines.push(match override_val {
            None => format!("// #define {name}"),
            Some(v) if v.is_empty() => format!("#define {name}"),
            Some(v) => format!("#define {name} {v}"),
        });
        let opt = unit.defines.entry(name.clone()).or_insert(ShaderOption {
            name: name.clone(),
            value: None,
            enabled: !was_commented,
        });
        match override_val {
            None => opt.enabled = false,
            Some(v) => {
                opt.enabled = true;
                opt.value = if v.is_empty() { None } else { Some(v.clone()) };
            }
        }
    }
    if changed {
        unit.source = new_lines.join("\n");
    }
}

/// Parsed `lang/<locale>.lang` localization table for shaderpack option UIs
/// (`option.<NAME>`, `value.<NAME>.<VAL>`, `profile.<NAME>`, `screen.<NAME>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShaderPackLang {
    /// Resolved locale tag (e.g. `ja_JP`, `en_US`), or `""` when empty.
    pub locale: String,
    /// `key -> localized text`, merged with `en_US` fallback when present.
    pub entries: BTreeMap<String, String>,
}

impl ShaderPackLang {
    /// Parse a single `.lang` file (`key=value` lines, `#` comments).
    pub fn parse(locale: impl Into<String>, text: &str) -> Self {
        let mut entries = BTreeMap::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                if !key.is_empty() {
                    entries.insert(key.to_owned(), v.trim().to_owned());
                }
            }
        }
        Self {
            locale: locale.into(),
            entries,
        }
    }

    /// Look up `key`, falling back to `default` when it has no translation.
    fn resolved<'a>(&'a self, key: String, default: &'a str) -> &'a str {
        self.entries.get(&key).map(String::as_str).unwrap_or(default)
    }

    /// Look up the localized display name for an option, falling back to `name`.
    pub fn option_label<'a>(&'a self, name: &'a str) -> &'a str {
        self.resolved(format!("option.{name}"), name)
    }

    /// Look up the localized tooltip/comment for an option, if present.
    pub fn option_comment(&self, name: &str) -> Option<&str> {
        self.entries.get(&format!("option.{name}.comment")).map(String::as_str)
    }

    /// Look up a localized option value label (`value.<OPTION>.<VAL>`),
    /// falling back to `val`.
    pub fn value_label<'a>(&'a self, option: &str, val: &'a str) -> &'a str {
        self.resolved(format!("value.{option}.{val}"), val)
    }

    /// Look up a localized profile name (`profile.<NAME>`), falling back to `name`.
    pub fn profile_label<'a>(&'a self, name: &'a str) -> &'a str {
        self.resolved(format!("profile.{name}"), name)
    }

    /// Look up a localized screen title (`screen.<NAME>`), falling back to `name`.
    pub fn screen_label<'a>(&'a self, name: &'a str) -> &'a str {
        self.resolved(format!("screen.{name}"), name)
    }
}

/// The locale stem of a `lang/<locale>.lang` path, `None` for any other path.
fn lang_stem(path: &str) -> Option<&str> {
    let stem = path.strip_prefix("lang/")?.strip_suffix(".lang")?;
    (!stem.is_empty() && !stem.contains('/')).then_some(stem)
}

impl ShaderPackSource {
    /// Sorted list of locale names available under `lang/<locale>.lang`.
    pub fn available_locales(&self) -> Vec<String> {
        self.list()
            .filter_map(lang_stem)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Load `lang/<locale>.lang` (case-insensitive locale match) layered over
    /// `lang/en_US.lang` so missing translations fall back cleanly to English
    /// and then to the raw key.
    pub fn load_lang(&self, locale: &str) -> ShaderPackLang {
        let find_file = |want: &str| -> Option<(String, String)> {
            for path in self.list() {
                if let Some(stem) = lang_stem(path) {
                    if stem.eq_ignore_ascii_case(want) {
                        if let Some(text) = self.read_text(path) {
                            return Some((stem.to_owned(), text));
                        }
                    }
                }
            }
            None
        };
        let mut merged = match find_file("en_US") {
            Some((loc, text)) => ShaderPackLang::parse(loc, &text),
            None => ShaderPackLang::default(),
        };
        if !locale.eq_ignore_ascii_case("en_US") {
            if let Some((loc, text)) = find_file(locale) {
                let overlay = ShaderPackLang::parse(loc.clone(), &text);
                merged.locale = loc;
                for (k, v) in overlay.entries {
                    merged.entries.insert(k, v);
                }
            }
        }
        merged
    }
}

// ---------------------------------------------------------------------------
// Megu Native WGSL Shaderpack (`megu-shader.toml` + `.wgsl` passes)
// ---------------------------------------------------------------------------

/// `MalformedProperties` for the `megu-shader.toml` manifest at `line`.
fn megu_bad(line: usize, reason: impl Into<String>) -> ShaderPackError {
    malformed_props(MEGU_WGSL_MANIFEST, line, reason)
}

/// Default manifest path inside a Megu native WGSL shaderpack.
pub const MEGU_WGSL_MANIFEST: &str = "megu-shader.toml";

/// One pass declared inside a Megu native WGSL shaderpack (`[pass.<name>]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeguWgslPass {
    /// Pass name (e.g. `terrain_opaque`, `lod_terrain`, `composite0`, `final`).
    pub name: String,
    /// Pipeline stage this pass executes in.
    pub stage: PipelineStage,
    /// Graphics or compute pass kind.
    pub kind: PassKind,
    /// Pack-relative `.wgsl` file path.
    pub file: String,
    /// Raw WGSL source text read from [`ShaderPackSource`].
    pub wgsl_source: String,
    /// Declared color attachment targets, if any.
    pub render_targets: Vec<String>,
    /// Compute workgroup dispatch count `[x, y, z]`, when `kind == Compute`.
    pub work_groups: Option<[u32; 3]>,
}

/// Loaded Megu native WGSL shaderpack — zero GLSL translation overhead on
/// Vulkan/Metal/DX12 backends, sharing the same bounded [`ShaderPackSource`]
/// loader and [`GpuShaderCaps`] validation as external OptiFine/Iris packs.
#[derive(Clone, Debug)]
pub struct MeguWgslPack {
    /// Human-readable pack display name.
    pub name: String,
    /// Pack author (optional).
    pub author: String,
    /// Pack version string (optional).
    pub version: String,
    /// Manifest API version (currently `1`).
    pub api_version: u32,
    /// Whether the pack explicitly requires GPU compute shaders.
    pub requires_compute: bool,
    /// Whether the pack explicitly requires SSBO storage buffers.
    pub requires_ssbo: bool,
    /// Whether the pack provides native Megu LOD shading passes.
    pub supports_lod: bool,
    /// Ordered WGSL passes.
    pub passes: Vec<MeguWgslPass>,
    /// GPU capability verdict against the supplied [`GpuShaderCaps`].
    pub compatibility: ShaderCompatibilityReport,
}

impl MeguWgslPack {
    /// Whether this WGSL pack can execute on the validated GPU capabilities.
    pub fn is_compatible(&self) -> bool {
        self.compatibility.is_compatible()
    }
}

fn parse_megu_stage(tok: &str) -> Option<PipelineStage> {
    match tok.trim().to_ascii_lowercase().as_str() {
        "begin" => Some(PipelineStage::Begin),
        "shadow" => Some(PipelineStage::Shadow),
        "prepare" => Some(PipelineStage::Prepare),
        "gbuffers" | "terrain" | "opaque" | "translucent" => Some(PipelineStage::Gbuffers),
        "lod" | "distant_horizons" | "dh" => Some(PipelineStage::DistantHorizons),
        "deferred" => Some(PipelineStage::Deferred),
        "composite" => Some(PipelineStage::Composite),
        "final" => Some(PipelineStage::Final),
        _ => None,
    }
}

fn unquote_toml_str(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].to_owned()
    } else {
        t.to_owned()
    }
}

/// Detect whether `source` is a Megu native WGSL pack (`megu-shader.toml`).
pub fn is_megu_wgsl_pack(source: &ShaderPackSource) -> bool {
    source.read(MEGU_WGSL_MANIFEST).is_some()
}

/// One `[pass.<name>]` section of a `megu-shader.toml` manifest, before its
/// WGSL file is read.
#[derive(Default)]
struct MeguRawPass {
    stage: Option<PipelineStage>,
    kind: Option<PassKind>,
    file: Option<String>,
    targets: Vec<String>,
    work_groups: Option<[u32; 3]>,
}

/// A parsed `megu-shader.toml`: pack metadata plus its raw pass sections.
#[derive(Default)]
struct MeguManifest {
    name: String,
    author: String,
    version: String,
    api_version: u32,
    requires_compute: bool,
    requires_ssbo: bool,
    supports_lod: bool,
    passes: BTreeMap<String, MeguRawPass>,
}

impl MeguManifest {
    /// Parse the manifest (a flat TOML subset: `[section]` headers and
    /// `key = value` lines, `#` comments).
    fn parse(text: &str) -> Result<Self, ShaderPackError> {
        let mut out = Self {
            name: "megu-wgsl-pack".to_owned(),
            version: "0.1.0".to_owned(),
            api_version: 1,
            ..Self::default()
        };
        let mut section = String::new();
        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = match raw_line.find('#') {
                Some(c) => raw_line[..c].trim(),
                None => raw_line.trim(),
            };
            if line.is_empty() {
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = header.trim().to_owned();
                if let Some(pass_name) = section.strip_prefix("pass.") {
                    out.passes.entry(pass_name.trim().to_owned()).or_default();
                }
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return Err(megu_bad(line_no, "expected key = value"));
            };
            let (key, val) = (k.trim(), v.trim());
            if section.is_empty() || section == "pack" {
                out.apply_pack_entry(key, val, line_no)?;
            } else if let Some(pass_name) = section.strip_prefix("pass.") {
                let entry = out.passes.entry(pass_name.trim().to_owned()).or_default();
                apply_pass_entry(entry, key, val, line_no)?;
            }
        }
        if out.passes.is_empty() {
            return Err(ShaderPackError::EmptyPack);
        }
        Ok(out)
    }

    /// Apply one `key = value` line of the `[pack]` section.
    fn apply_pack_entry(&mut self, key: &str, val: &str, line: usize) -> Result<(), ShaderPackError> {
        match key {
            "name" => self.name = unquote_toml_str(val),
            "author" => self.author = unquote_toml_str(val),
            "version" => self.version = unquote_toml_str(val),
            "api" | "api_version" => {
                self.api_version =
                    val.parse()
                        .map_err(|_| megu_bad(line, "invalid api_version"))?;
            }
            "requires_compute" => self.requires_compute = val == "true",
            "requires_ssbo" => self.requires_ssbo = val == "true",
            "supports_lod" => self.supports_lod = val == "true",
            _ => {}
        }
        Ok(())
    }
}

/// Apply one `key = value` line of a `[pass.<name>]` section.
fn apply_pass_entry(
    entry: &mut MeguRawPass,
    key: &str,
    val: &str,
    line: usize,
) -> Result<(), ShaderPackError> {
    match key {
        "stage" => {
            let stage = unquote_toml_str(val);
            entry.stage =
                Some(parse_megu_stage(&stage).ok_or_else(|| megu_bad(line, "unknown pipeline stage"))?);
        }
        "kind" => {
            let kind = unquote_toml_str(val).to_ascii_lowercase();
            entry.kind = Some(match kind.as_str() {
                "compute" => PassKind::Compute,
                "graphics" => PassKind::Graphics,
                _ => return Err(megu_bad(line, "kind must be graphics or compute")),
            });
        }
        "file" => entry.file = Some(unquote_toml_str(val)),
        "targets" => {
            let list = toml_list(val);
            entry.targets = list.split(',').map(unquote_toml_str).filter(|s| !s.is_empty()).collect();
        }
        "workgroups" => {
            let nums = toml_list(val)
                .split(',')
                .map(|s| s.trim().parse::<u32>().map_err(|_| megu_bad(line, "bad workgroups")))
                .collect::<Result<Vec<u32>, _>>()?;
            let [x, y, z] = nums[..] else {
                return Err(megu_bad(line, "workgroups must be 3 positive integers"));
            };
            if x == 0 || y == 0 || z == 0 {
                return Err(megu_bad(line, "workgroups must be 3 positive integers"));
            }
            entry.work_groups = Some([x, y, z]);
        }
        _ => {}
    }
    Ok(())
}

/// Strip the brackets around an inline TOML array value.
fn toml_list(val: &str) -> &str {
    val.trim_start_matches('[').trim_end_matches(']')
}

/// Read every declared pass file into [`MeguWgslPass`]es, sorted by stage and
/// name. Also reports whether the passes themselves imply compute/LOD usage.
fn build_megu_passes(
    source: &ShaderPackSource,
    raw: BTreeMap<String, MeguRawPass>,
) -> Result<(Vec<MeguWgslPass>, bool, bool), ShaderPackError> {
    let mut passes = Vec::with_capacity(raw.len());
    let mut requires_compute = false;
    let mut supports_lod = false;
    for (name, rp) in raw {
        let stage = rp.stage.unwrap_or(PipelineStage::Gbuffers);
        let kind = rp.kind.unwrap_or(PassKind::Graphics);
        let file = rp.file.ok_or_else(|| ShaderPackError::IncompleteProgram {
            name: name.clone(),
            reason: "WGSL pass missing `file`".to_owned(),
        })?;
        validate_virtual_path(&file)?;
        let wgsl_source = source
            .read_text(&file)
            .ok_or_else(|| ShaderPackError::IncludeNotFound {
                include: file.clone(),
                from: MEGU_WGSL_MANIFEST.to_owned(),
            })?;
        supports_lod |= stage == PipelineStage::DistantHorizons;
        requires_compute |= kind == PassKind::Compute;
        passes.push(MeguWgslPass {
            name,
            stage,
            kind,
            file,
            wgsl_source,
            render_targets: rp.targets,
            work_groups: rp.work_groups,
        });
    }
    passes.sort_by(|a, b| a.stage.cmp(&b.stage).then_with(|| a.name.cmp(&b.name)));
    Ok((passes, requires_compute, supports_lod))
}

/// Load and validate a Megu native WGSL pack (`megu-shader.toml` + `.wgsl`
/// files) against `caps`.
pub fn load_megu_wgsl_pack(
    source: &ShaderPackSource,
    caps: &GpuShaderCaps,
) -> Result<MeguWgslPack, ShaderPackError> {
    let text = source
        .read_text(MEGU_WGSL_MANIFEST)
        .ok_or(ShaderPackError::EmptyPack)?;
    let manifest = MeguManifest::parse(&text)?;
    let (passes, uses_compute, uses_lod) = build_megu_passes(source, manifest.passes)?;
    let requires_compute = manifest.requires_compute || uses_compute;
    let requires_ssbo = manifest.requires_ssbo;
    let mut missing = Vec::new();
    if manifest.api_version != 1 {
        missing.push(format!(
            "unsupported megu-shader API version {} (supported: 1)",
            manifest.api_version
        ));
    }
    if requires_compute && !caps.supports_compute {
        missing.push("compute shaders required by WGSL pack".to_owned());
    }
    if requires_ssbo && !caps.supports_ssbo {
        missing.push("shader storage buffers (SSBO) required by WGSL pack".to_owned());
    }
    for pass in &passes {
        check_color_attachments(&pass.name, pass.render_targets.len(), caps, &mut missing);
    }
    Ok(MeguWgslPack {
        name: manifest.name,
        author: manifest.author,
        version: manifest.version,
        api_version: manifest.api_version,
        requires_compute,
        requires_ssbo,
        supports_lod: manifest.supports_lod || uses_lod,
        passes,
        compatibility: report_from_missing(missing),
    })
}
