// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

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
/// Maximum ZIP comment/record sizes we will parse (64 KiB EOCD search window,
/// matching the ZIP spec's maximum comment length).
const EOCD_SEARCH_WINDOW: usize = 65_557;

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
        let mut table = BTreeMap::new();
        let mut total: u64 = 0;
        for (k, v) in files {
            let key = canonical_key(k.as_ref())?;
            let bytes = v.as_ref();
            let len = bytes.len() as u64;
            if len > MAX_SHADER_FILE_BYTES {
                return Err(ShaderPackError::FileTooLarge {
                    path: key,
                    bytes: len,
                });
            }
            total += len;
            if total > MAX_PACK_UNPACKED_BYTES {
                return Err(ShaderPackError::PackTooLarge { bytes: total });
            }
            if table.len() >= MAX_PACK_FILES {
                return Err(ShaderPackError::TooManyFiles { count: table.len() + 1 });
            }
            table.insert(key, bytes.to_vec());
        }
        Ok(Self {
            files: table,
            origin: PackOrigin::InMemory,
        })
    }

    /// Open `path` as a directory or a `.zip` (extension-based dispatch).
    pub fn open(path: &Path) -> Result<Self, ShaderPackError> {
        let meta = fs::metadata(path).map_err(|source| ShaderPackError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if meta.is_dir() {
            Self::open_directory(path)
        } else if meta.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        {
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
        let canon_root = fs::canonicalize(root).map_err(|source| ShaderPackError::Io {
            path: root.display().to_string(),
            source,
        })?;
        if !canon_root.is_dir() {
            return Err(ShaderPackError::NotAPack {
                path: root.display().to_string(),
            });
        }
        let shaders = canon_root.join("shaders");
        let base = if shaders.is_dir() { shaders } else { canon_root };
        let mut files = BTreeMap::new();
        let mut total: u64 = 0;
        let mut pending = vec![base.clone()];
        while let Some(dir) = pending.pop() {
            let mut entries: Vec<_> = fs::read_dir(&dir)
                .map_err(|source| ShaderPackError::Io {
                    path: dir.display().to_string(),
                    source,
                })?
                .collect::<Result<_, _>>()
                .map_err(|source| ShaderPackError::Io {
                    path: dir.display().to_string(),
                    source,
                })?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let raw_path = entry.path();
                // Canonicalize *every* path: any symlink resolving outside the
                // shader root is a traversal escape and is rejected.
                let canon = fs::canonicalize(&raw_path).map_err(|source| {
                    ShaderPackError::Io {
                        path: raw_path.display().to_string(),
                        source,
                    }
                })?;
                if !canon.starts_with(&base) {
                    return Err(ShaderPackError::UnsafePath {
                        path: raw_path.display().to_string(),
                    });
                }
                let rel = canon
                    .strip_prefix(&base)
                    .map_err(|_| ShaderPackError::UnsafePath {
                        path: raw_path.display().to_string(),
                    })?;
                let rel_str = path_to_virtual(rel)?;
                if canon.is_dir() {
                    pending.push(canon);
                } else if canon.is_file() {
                    let meta = fs::metadata(&canon).map_err(|source| ShaderPackError::Io {
                        path: canon.display().to_string(),
                        source,
                    })?;
                    if meta.len() > MAX_SHADER_FILE_BYTES {
                        return Err(ShaderPackError::FileTooLarge {
                            path: rel_str,
                            bytes: meta.len(),
                        });
                    }
                    total += meta.len();
                    if total > MAX_PACK_UNPACKED_BYTES {
                        return Err(ShaderPackError::PackTooLarge { bytes: total });
                    }
                    if files.len() >= MAX_PACK_FILES {
                        return Err(ShaderPackError::TooManyFiles {
                            count: files.len() + 1,
                        });
                    }
                    let bytes = fs::read(&canon).map_err(|source| ShaderPackError::Io {
                        path: canon.display().to_string(),
                        source,
                    })?;
                    files.insert(rel_str, bytes);
                }
            }
        }
        Ok(Self {
            files,
            origin: PackOrigin::Directory,
        })
    }

    /// Load a pack from a `.zip` archive (stored or deflate entries).
    pub fn open_zip(path: &Path) -> Result<Self, ShaderPackError> {
        let meta = fs::metadata(path).map_err(|source| ShaderPackError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if meta.len() > MAX_ZIP_FILE_BYTES {
            return Err(ShaderPackError::PackTooLarge { bytes: meta.len() });
        }
        let bytes = fs::read(path).map_err(|source| ShaderPackError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let entries = read_zip_entries(&bytes)?;
        let mut files = BTreeMap::new();
        let mut total: u64 = 0;
        // Find the shader root prefix: prefer `shaders/`, else a single
        // wrapping top-level directory containing `shaders/`, else the
        // archive root itself.
        let prefix = shader_root_prefix(entries.iter().map(|(n, _)| n.as_str()));
        for (name, data) in entries {
            let rel = match name.strip_prefix(&prefix) {
                Some(r) => r,
                None => continue,
            };
            let key = canonical_key(rel)?;
            let len = data.len() as u64;
            if len > MAX_SHADER_FILE_BYTES {
                return Err(ShaderPackError::FileTooLarge {
                    path: key,
                    bytes: len,
                });
            }
            total += len;
            if total > MAX_PACK_UNPACKED_BYTES {
                return Err(ShaderPackError::PackTooLarge { bytes: total });
            }
            if files.len() >= MAX_PACK_FILES {
                return Err(ShaderPackError::TooManyFiles {
                    count: files.len() + 1,
                });
            }
            files.insert(key, data);
        }
        Ok(Self {
            files,
            origin: PackOrigin::ZipArchive,
        })
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
        let mut dims: Vec<i32> = self
            .files
            .keys()
            .filter_map(|k| k.split('/').next().and_then(parse_world_dir))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        dims.sort_unstable();
        dims
    }
}

/// Convert a relative `Path` into a `/`-separated virtual path.
fn path_to_virtual(rel: &Path) -> Result<String, ShaderPackError> {
    let mut parts = Vec::new();
    for comp in rel.components() {
        match comp {
            std::path::Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| ShaderPackError::UnsafePath {
                    path: rel.display().to_string(),
                })?;
                parts.push(s);
            }
            _ => {
                return Err(ShaderPackError::UnsafePath {
                    path: rel.display().to_string(),
                })
            }
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
    let mut tops = BTreeSet::new();
    for n in &names {
        if let Some((top, _)) = n.split_once('/') {
            tops.insert(top.to_owned());
        } else {
            tops.insert(String::new());
        }
    }
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
// Bounded ZIP reader + clean-room inflate (RFC 1950/1951)
// ---------------------------------------------------------------------------

fn u16le(data: &[u8], off: usize) -> Result<u16, ShaderPackError> {
    data.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| ShaderPackError::ZipMalformed {
            reason: "truncated record".to_owned(),
        })
}

fn u32le(data: &[u8], off: usize) -> Result<u32, ShaderPackError> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| ShaderPackError::ZipMalformed {
            reason: "truncated record".to_owned(),
        })
}

/// Parse the central directory of `zip` and decompress every regular file
/// entry. Returns `(name, uncompressed-bytes)` pairs in archive order.
fn read_zip_entries(zip: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ShaderPackError> {
    let malformed = |reason: &str| ShaderPackError::ZipMalformed {
        reason: reason.to_owned(),
    };
    // Locate EOCD by scanning the tail window backwards.
    let start = zip.len().saturating_sub(EOCD_SEARCH_WINDOW);
    let mut eocd = None;
    for i in (start..zip.len().saturating_sub(3)).rev() {
        if zip[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd.ok_or_else(|| malformed("no end-of-central-directory record"))?;
    let disk = u16le(zip, eocd + 4)?;
    let cd_disk = u16le(zip, eocd + 6)?;
    if disk != 0 || cd_disk != 0 {
        return Err(ShaderPackError::ZipUnsupported {
            reason: "multi-disk archive".to_owned(),
        });
    }
    let total_entries = u16le(zip, eocd + 10)? as usize;
    let cd_offset = u32le(zip, eocd + 16)? as usize;
    if total_entries == 0xFFFF || u32le(zip, eocd + 12)? == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        return Err(ShaderPackError::ZipUnsupported {
            reason: "ZIP64 archive".to_owned(),
        });
    }
    if total_entries > MAX_PACK_FILES {
        return Err(ShaderPackError::TooManyFiles {
            count: total_entries,
        });
    }
    let mut out = Vec::with_capacity(total_entries.min(1024));
    let mut off = cd_offset;
    for _ in 0..total_entries {
        if u32le(zip, off)? != 0x0201_4b50 {
            return Err(malformed("bad central-directory signature"));
        }
        let flags = u16le(zip, off + 8)?;
        let method = u16le(zip, off + 10)?;
        let csize = u32le(zip, off + 20)? as usize;
        let usize_ = u32le(zip, off + 24)? as u64;
        let name_len = u16le(zip, off + 28)? as usize;
        let extra_len = u16le(zip, off + 30)? as usize;
        let comment_len = u16le(zip, off + 32)? as usize;
        let lh_off = u32le(zip, off + 42)? as usize;
        if flags & 0x1 != 0 {
            return Err(ShaderPackError::ZipUnsupported {
                reason: "encrypted entry".to_owned(),
            });
        }
        if csize == 0xFFFF_FFFF || usize_ == u64::from(u32::MAX) {
            return Err(ShaderPackError::ZipUnsupported {
                reason: "ZIP64 entry".to_owned(),
            });
        }
        let name_bytes = zip
            .get(off + 46..off + 46 + name_len)
            .ok_or_else(|| malformed("truncated entry name"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| malformed("non-UTF-8 entry name"))?
            .to_owned();
        if name.ends_with('/') {
            off += 46 + name_len + extra_len + comment_len;
            continue;
        }
        // Validate *before* doing any work with the name.
        validate_virtual_path(&name)
            .map_err(|_| ShaderPackError::UnsafePath { path: name.clone() })?;
        if usize_ > MAX_SHADER_FILE_BYTES {
            return Err(ShaderPackError::FileTooLarge {
                path: name,
                bytes: usize_,
            });
        }
        // Local header: re-read name/extra lengths to find the data.
        if u32le(zip, lh_off)? != 0x0403_4b50 {
            return Err(malformed("bad local-file-header signature"));
        }
        let l_name_len = u16le(zip, lh_off + 26)? as usize;
        let l_extra_len = u16le(zip, lh_off + 28)? as usize;
        let data_start = lh_off
            .checked_add(30)
            .and_then(|v| v.checked_add(l_name_len))
            .and_then(|v| v.checked_add(l_extra_len))
            .ok_or_else(|| malformed("header offset overflow"))?;
        let data = zip
            .get(data_start..data_start + csize)
            .ok_or_else(|| malformed("truncated entry data"))?;
        let bytes = match method {
            0 => {
                if data.len() as u64 != usize_ {
                    return Err(malformed("stored entry size mismatch"));
                }
                data.to_vec()
            }
            8 => inflate(data, usize_ as usize)?,
            m => {
                return Err(ShaderPackError::ZipUnsupported {
                    reason: format!("compression method {m}"),
                })
            }
        };
        out.push((name, bytes));
        off += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// Inflate `input` (raw DEFLATE) into at most `limit` bytes.
///
/// Clean-room RFC 1951 implementation: stored blocks, fixed Huffman, and
/// dynamic Huffman with the full length/distance alphabet. Output is capped
/// at `limit` so a hostile stream cannot expand unboundedly.
fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, ShaderPackError> {
    let malformed = |reason: &str| ShaderPackError::ZipMalformed {
        reason: reason.to_owned(),
    };
    let mut br = BitReader::new(input);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align_byte();
                let len = br.aligned_u16()?;
                let nlen = br.aligned_u16()?;
                if len != !nlen {
                    return Err(malformed("deflate stored-block LEN/NLEN mismatch"));
                }
                for _ in 0..len {
                    let b = br.aligned_byte()?;
                    if out.len() >= limit {
                        return Err(malformed("deflate output exceeds declared size"));
                    }
                    out.push(b);
                }
            }
            1 | 2 => {
                let (lit, dist) = if btype == 1 {
                    (fixed_litlen_huffman(), fixed_dist_huffman())
                } else {
                    read_dynamic_tables(&mut br)?
                };
                loop {
                    let sym = lit.decode(&mut br)?;
                    match sym {
                        0..=255 => {
                            if out.len() >= limit {
                                return Err(malformed(
                                    "deflate output exceeds declared size",
                                ));
                            }
                            out.push(sym as u8);
                        }
                        256 => break,
                        257..=285 => {
                            let idx = (sym - 257) as usize;
                            let length = LENGTH_BASE[idx] as usize
                                + br.bits(LENGTH_EXTRA[idx])? as usize;
                            let dsym = dist.decode(&mut br)? as usize;
                            if dsym >= DIST_BASE.len() {
                                return Err(malformed("invalid distance symbol"));
                            }
                            let distance = DIST_BASE[dsym] as usize
                                + br.bits(DIST_EXTRA[dsym])? as usize;
                            if distance == 0 || distance > out.len() {
                                return Err(malformed("back-reference before output start"));
                            }
                            let start = out.len() - distance;
                            for i in 0..length {
                                if out.len() >= limit {
                                    return Err(malformed(
                                        "deflate output exceeds declared size",
                                    ));
                                }
                                let b = out[start + i];
                                out.push(b);
                            }
                        }
                        _ => return Err(malformed("invalid literal/length symbol")),
                    }
                }
            }
            _ => return Err(malformed("reserved deflate block type")),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// LSB-first bit reader over a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    len: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buf: 0,
            len: 0,
        }
    }

    fn bits(&mut self, n: u32) -> Result<u32, ShaderPackError> {
        debug_assert!(n <= 32);
        while self.len < n {
            let b = *self
                .data
                .get(self.pos)
                .ok_or_else(|| ShaderPackError::ZipMalformed {
                    reason: "deflate stream truncated".to_owned(),
                })?;
            self.pos += 1;
            self.buf |= u64::from(b) << self.len;
            self.len += 8;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.len -= n;
        Ok(v)
    }

    fn align_byte(&mut self) {
        let drop = self.len % 8;
        self.buf >>= drop;
        self.len -= drop;
    }

    fn aligned_byte(&mut self) -> Result<u8, ShaderPackError> {
        if self.len >= 8 {
            let b = self.buf as u8;
            self.buf >>= 8;
            self.len -= 8;
            Ok(b)
        } else {
            self.bits(8).map(|v| v as u8)
        }
    }

    fn aligned_u16(&mut self) -> Result<u16, ShaderPackError> {
        let lo = self.aligned_byte()?;
        let hi = self.aligned_byte()?;
        Ok(u16::from_le_bytes([lo, hi]))
    }
}

/// Canonical Huffman table decoded puff-style: `counts[n]` is how many
/// symbols have code length `n`, `symbols` lists them in code order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Self, ShaderPackError> {
        let malformed = || ShaderPackError::ZipMalformed {
            reason: "invalid huffman table".to_owned(),
        };
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err(malformed());
            }
            counts[l as usize] += 1;
        }
        if counts[0] as usize == lengths.len() {
            // All lengths zero: degenerate empty table.
            return Err(malformed());
        }
        // Check for an over-subscribed code.
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err(malformed());
            }
        }
        let mut offs = [0usize; 16];
        for i in 1..15 {
            offs[i + 1] = offs[i] + counts[i] as usize;
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize]] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16, ShaderPackError> {
        let malformed = || ShaderPackError::ZipMalformed {
            reason: "invalid huffman code".to_owned(),
        };
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16usize {
            code |= br.bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return self
                    .symbols
                    .get((index + (code - first)) as usize)
                    .copied()
                    .ok_or_else(malformed);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(malformed())
    }
}

fn fixed_litlen_huffman() -> Huffman {
    let mut lengths = [0u8; 288];
    for (i, l) in lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    // Lengths are compile-time constants; build cannot fail.
    match Huffman::build(&lengths) {
        Ok(h) => h,
        Err(_) => unreachable!("fixed deflate table is well-formed"),
    }
}

fn fixed_dist_huffman() -> Huffman {
    match Huffman::build(&[5u8; 30]) {
        Ok(h) => h,
        Err(_) => unreachable!("fixed deflate table is well-formed"),
    }
}

/// Read a dynamic-Huffman block header and build the literal/length and
/// distance tables.
fn read_dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman), ShaderPackError> {
    let malformed = |reason: &str| ShaderPackError::ZipMalformed {
        reason: reason.to_owned(),
    };
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(malformed("dynamic table size out of range"));
    }
    let mut clen_lengths = [0u8; 19];
    for &idx in CLEN_ORDER.iter().take(hclen) {
        clen_lengths[idx] = br.bits(3)? as u8;
    }
    let clen = Huffman::build(&clen_lengths)?;
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = clen.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(malformed("repeat with no previous length"));
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= lengths.len() {
                        return Err(malformed("repeat overflows table"));
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 | 18 => {
                let rep = if sym == 17 {
                    3 + br.bits(3)? as usize
                } else {
                    11 + br.bits(7)? as usize
                };
                i += rep;
                if i > lengths.len() {
                    return Err(malformed("zero-run overflows table"));
                }
            }
            _ => return Err(malformed("invalid code-length symbol")),
        }
    }
    let lit = Huffman::build(&lengths[..hlit])?;
    let dist = Huffman::build(&lengths[hlit..])?;
    Ok((lit, dist))
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
        if rest.starts_with(' ') || rest.starts_with('\t') {
            rest = rest.trim_start();
            if !(rest.starts_with('=') || rest.starts_with(':')) {
                // whitespace was the separator; the value follows directly.
            } else {
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
        const FILE: &str = "shaders.properties";
        let mut out = Self::default();
        for prop in parse_properties(text, FILE)? {
            out.raw.insert(prop.key.clone(), prop.value.clone());
            Self::apply(&mut out, &prop, FILE)?;
        }
        Ok(out)
    }

    fn apply(
        out: &mut Self,
        prop: &RawProperty,
        file: &str,
    ) -> Result<(), ShaderPackError> {
        let key = prop.key.as_str();
        let value = prop.value.as_str();
        let line = prop.line;
        if let Some(rest) = key.strip_prefix("bufferObject.") {
            if let Some(id_s) = rest.strip_suffix(".scale") {
                let id: u32 = id_s
                    .parse()
                    .map_err(|_| malformed_props(file, line, "bad bufferObject id"))?;
                let scale: f64 = value
                    .parse()
                    .map_err(|_| malformed_props(file, line, "bad bufferObject scale"))?;
                out.ssbos
                    .entry(id)
                    .or_insert(SsboSpec {
                        binding: id,
                        byte_size: 0,
                        scale: None,
                    })
                    .scale = Some(scale);
            } else {
                let id: u32 = rest
                    .parse()
                    .map_err(|_| malformed_props(file, line, "bad bufferObject id"))?;
                let size: u64 = value
                    .parse()
                    .map_err(|_| malformed_props(file, line, "bad bufferObject byte size"))?;
                out.ssbos
                    .entry(id)
                    .or_insert(SsboSpec {
                        binding: id,
                        byte_size: 0,
                        scale: None,
                    })
                    .byte_size = size;
            }
            return Ok(());
        }
        if let Some(name) = key.strip_prefix("image.") {
            let toks: Vec<&str> = value.split_whitespace().collect();
            if toks.len() < 7 {
                return Err(malformed_props(
                    file,
                    line,
                    "image needs: <sampler> <format> <internalFormat> <pixelType> <clear> <relative> <size...>",
                ));
            }
            let clear = parse_bool_token(toks[4])
                .ok_or_else(|| malformed_props(file, line, "bad image clear flag"))?;
            let relative = parse_bool_token(toks[5])
                .ok_or_else(|| malformed_props(file, line, "bad image relative flag"))?;
            let mut size = Vec::with_capacity(3);
            for t in &toks[6..] {
                size.push(
                    t.parse::<f64>()
                        .map_err(|_| malformed_props(file, line, "bad image size"))?,
                );
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
            return Ok(());
        }
        if let Some(rest) = key.strip_prefix("texture.") {
            let (stage, name) = match rest.split_once('.') {
                Some((s, n)) => (s.to_owned(), n.to_owned()),
                None => (String::new(), rest.to_owned()),
            };
            out.textures.insert(
                (stage.clone(), name.clone()),
                CustomTexture {
                    stage,
                    name,
                    path: value.to_owned(),
                },
            );
            return Ok(());
        }
        if let Some(name) = key.strip_prefix("size.buffer.") {
            let toks: Vec<&str> = value.split_whitespace().collect();
            if toks.len() != 2 {
                return Err(malformed_props(file, line, "size.buffer needs two ints"));
            }
            let w: u32 = toks[0]
                .parse()
                .map_err(|_| malformed_props(file, line, "bad buffer width"))?;
            let h: u32 = toks[1]
                .parse()
                .map_err(|_| malformed_props(file, line, "bad buffer height"))?;
            out.buffer_sizes.insert(name.to_owned(), (w, h));
            return Ok(());
        }
        if let Some(pass) = key.strip_prefix("scale.") {
            let mut vals = Vec::new();
            for t in value.split_whitespace() {
                vals.push(
                    t.parse::<f64>()
                        .map_err(|_| malformed_props(file, line, "bad scale value"))?,
                );
            }
            if vals.is_empty() || vals.len() > 2 {
                return Err(malformed_props(file, line, "scale needs 1-2 values"));
            }
            out.pass_scales.insert(pass.to_owned(), vals);
            return Ok(());
        }
        if let Some(rest) = key.strip_prefix("flip.") {
            let (pass, buf) = rest
                .split_once('.')
                .ok_or_else(|| malformed_props(file, line, "flip needs <pass>.<buf>"))?;
            let v = parse_bool_token(value)
                .ok_or_else(|| malformed_props(file, line, "bad flip value"))?;
            out.flips.push((pass.to_owned(), buf.to_owned(), v));
            return Ok(());
        }
        if let Some(pass) = key.strip_prefix("alphaTest.") {
            let toks: Vec<&str> = value.split_whitespace().collect();
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
                    let v: f64 = toks[1]
                        .parse()
                        .map_err(|_| malformed_props(file, line, "bad alphaTest value"))?;
                    AlphaTest::Func { func: t, value: v }
                }
                _ => AlphaTest::Raw(value.to_owned()),
            };
            out.alpha_tests.insert(pass.to_owned(), test);
            return Ok(());
        }
        if let Some(rest) = key.strip_prefix("blend.") {
            let (pass, buf) = match rest.split_once('.') {
                Some((p, b)) => (p.to_owned(), b.to_owned()),
                None => (rest.to_owned(), String::new()),
            };
            out.blends.push(BlendDirective {
                pass,
                buffer: buf,
                tokens: value.split_whitespace().map(str::to_owned).collect(),
            });
            return Ok(());
        }
        if let Some(name) = key.strip_prefix("profile.") {
            out.profiles.insert(
                name.to_owned(),
                Profile {
                    name: name.to_owned(),
                    tokens: value.split_whitespace().map(str::to_owned).collect(),
                },
            );
            return Ok(());
        }
        match key {
            "shadow.enabled" => {
                out.shadow.enabled = Some(
                    parse_bool_token(value)
                        .ok_or_else(|| malformed_props(file, line, "bad shadow.enabled"))?,
                );
            }
            "shadowDistance" => {
                out.shadow.distance = Some(
                    value
                        .parse()
                        .map_err(|_| malformed_props(file, line, "bad shadowDistance"))?,
                );
            }
            "shadowMapResolution" => {
                out.shadow.resolution = Some(
                    value
                        .parse()
                        .map_err(|_| malformed_props(file, line, "bad shadowMapResolution"))?,
                );
            }
            "screen" => {
                out.screens.insert(
                    String::new(),
                    value.split_whitespace().map(str::to_owned).collect(),
                );
            }
            "sliders" => {
                out.sliders = value.split_whitespace().map(str::to_owned).collect();
            }
            "iris.features.required" => {
                out.required_features = value
                    .split_whitespace()
                    .map(IrisFeature::parse)
                    .collect();
            }
            "iris.features.optional" => {
                out.optional_features = value
                    .split_whitespace()
                    .map(IrisFeature::parse)
                    .collect();
            }
            _ => {
                if let Some(sub) = key.strip_prefix("screen.") {
                    out.screens.insert(
                        sub.to_owned(),
                        value.split_whitespace().map(str::to_owned).collect(),
                    );
                }
                // Unknown keys are preserved in `raw` for forward
                // compatibility; they are not errors.
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
    let parts: Vec<&str> = spec.split(':').collect();
    // Name portion = parts before the first part containing '='.
    let name_len = parts
        .iter()
        .position(|p| p.contains('='))
        .unwrap_or(parts.len());
    let name_parts = &parts[..name_len];
    let name = match name_parts {
        [n] => format!("minecraft:{n}"),
        [ns, n] => format!("{ns}:{n}"),
        _ => {
            return Err(malformed_props(
                file,
                line,
                format!("bad block/item specifier {spec:?}"),
            ))
        }
    };
    if name_parts.iter().any(|p| p.is_empty()) {
        return Err(malformed_props(
            file,
            line,
            format!("bad block/item specifier {spec:?}"),
        ));
    }
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
                from: self
                    .stack
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "<entry>".to_owned()),
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
            if let Some(include) = include {
                match include {
                    Ok(spec) => {
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
                    Err(reason) => {
                        return Err(ShaderPackError::MalformedShader {
                            path: path.to_owned(),
                            line: line_no,
                            reason,
                        });
                    }
                }
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        self.stack.pop();
        Ok(out)
    }

    /// Resolve an `#include` spec to a canonical key: `/x` from the shader
    /// root, `x` relative to the including file's dir then the root.
    fn resolve(&self, dir: &str, spec: &str) -> Result<String, ShaderPackError> {
        let from = self
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| "<entry>".to_owned());
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
            return Err(ShaderPackError::UnsafePath {
                path: spec.to_owned(),
            });
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

/// For each `\n`-separated line, whether it starts inside a `/* */` block
/// comment. Lines inside a block comment cannot contain directives.
fn lines_in_block_comment(src: &str) -> Vec<bool> {
    #[derive(PartialEq)]
    enum S {
        Code,
        Line,
        Block,
        Str,
    }
    let mut out = Vec::new();
    let mut state = S::Code;
    let mut chars = src.chars().peekable();
    // First line starts outside any comment.
    out.push(false);
    while let Some(c) = chars.next() {
        match state {
            S::Code => match c {
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    state = S::Line;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = S::Block;
                }
                '"' => state = S::Str,
                _ => {}
            },
            S::Line => {
                if c == '\n' {
                    state = S::Code;
                }
            }
            S::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = S::Code;
                }
            }
            S::Str => {
                if c == '\\' {
                    chars.next();
                } else if c == '"' {
                    state = S::Code;
                }
            }
        }
        if c == '\n' {
            out.push(state == S::Block);
        }
    }
    out
}

/// Remove `//` and `/* */` comments while preserving newlines (line numbers
/// stay stable for diagnostics). String literals are respected.
fn strip_comments(src: &str) -> String {
    #[derive(PartialEq)]
    enum S {
        Code,
        Line,
        Block,
        Str,
    }
    let mut out = String::with_capacity(src.len());
    let mut state = S::Code;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            S::Code => match c {
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    state = S::Line;
                    out.push(' ');
                    out.push(' ');
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    state = S::Block;
                    out.push(' ');
                    out.push(' ');
                }
                '"' => {
                    out.push(c);
                    state = S::Str;
                }
                _ => out.push(c),
            },
            S::Line => {
                if c == '\n' {
                    out.push('\n');
                    state = S::Code;
                } else {
                    out.push(' ');
                }
            }
            S::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    out.push(' ');
                    out.push(' ');
                    state = S::Code;
                } else if c == '\n' {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            S::Str => {
                out.push(c);
                if c == '\\' {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                } else if c == '"' {
                    state = S::Code;
                }
            }
        }
    }
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
                if let Some(list) = body.strip_prefix(marker) {
                    *slot = Some(parse_target_list(list));
                }
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

/// Parse `const ivec3 workGroups = ivec3(a, b, c)` from comment-stripped
/// source. Literal integers only; anything else yields `None`.
fn extract_work_groups(src: &str) -> Option<[u32; 3]> {
    let marker = "const ivec3 workGroups";
    let pos = src.find(marker)?;
    let after = src[pos + marker.len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix("ivec3")?.trim_start();
    let after = after.strip_prefix('(')?;
    let close = after.find(')')?;
    let args: Vec<&str> = after[..close].split(',').collect();
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
    let marker = "const vec2 workGroupsRender";
    let pos = src.find(marker)?;
    let after = src[pos + marker.len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix("vec2")?.trim_start();
    let after = after.strip_prefix('(')?;
    let close = after.find(')')?;
    let args: Vec<&str> = after[..close].split(',').collect();
    if args.len() != 2 {
        return None;
    }
    let x = args[0].trim().trim_end_matches(['f', 'F']).parse::<f64>().ok()?;
    let y = args[1].trim().trim_end_matches(['f', 'F']).parse::<f64>().ok()?;
    Some([x, y])
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
            let mut toks = v.trim_start().splitn(2, char::is_whitespace);
            let name = toks.next().unwrap_or_default().trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                continue;
            }
            let value = toks.next().map(|s| s.trim().to_owned());
            defines.insert(
                name.to_owned(),
                ShaderOption {
                    name: name.to_owned(),
                    value,
                    enabled: true,
                },
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
        let mut toks = v.trim_start().splitn(2, char::is_whitespace);
        let name = toks.next().unwrap_or_default().trim();
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || defines.contains_key(name)
        {
            continue;
        }
        let value = toks.next().map(|s| s.trim().to_owned());
        defines.insert(
            name.to_owned(),
            ShaderOption {
                name: name.to_owned(),
                value,
                enabled: false,
            },
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
    let group = if stage == PipelineStage::Shadow {
        if pass_name == "dh_shadow" {
            1
        } else if pass_name.starts_with("shadowcomp") {
            2
        } else {
            0
        }
    } else {
        0
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

    // --- discover program files ------------------------------------------
    let dim_dir = dimension.dir_name();
    // program name -> (kind -> virtual path)
    let mut programs: BTreeMap<String, (PipelineStage, BTreeMap<ShaderKind, String>)> =
        BTreeMap::new();
    for path in source.list() {
        let name = match path.rsplit_once('/') {
            Some((dir, file)) => {
                if dir == dim_dir {
                    file
                } else {
                    continue;
                }
            }
            None => path,
        };
        let (base, ext) = match name.rsplit_once('.') {
            Some((b, e)) => (b, e),
            None => continue,
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
    if programs.is_empty() {
        return Err(ShaderPackError::EmptyPack);
    }
    // --- preprocess + assemble passes ------------------------------------
    let mut passes = Vec::with_capacity(programs.len());
    for (name, (stage, files)) in programs {
        let has_compute = files.contains_key(&ShaderKind::Compute);
        let has_graphics = files
            .keys()
            .any(|k| matches!(k, ShaderKind::Vertex | ShaderKind::Fragment));
        if has_compute && files.len() > 1 {
            return Err(ShaderPackError::IncompleteProgram {
                name,
                reason: "compute program mixes .csh with graphics stages".to_owned(),
            });
        }
        if has_graphics
            && !(files.contains_key(&ShaderKind::Vertex)
                && files.contains_key(&ShaderKind::Fragment))
        {
            return Err(ShaderPackError::IncompleteProgram {
                name,
                reason: "graphics program needs both .vsh and .fsh".to_owned(),
            });
        }
        let mut shaders = BTreeMap::new();
        for (kind, path) in &files {
            let mut pp = preprocess_shader_source(source, path)?;
            if !resolved_overrides.is_empty() {
                apply_option_overrides_to_unit(&mut pp, &resolved_overrides);
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
        passes.push(ShaderPass {
            name,
            stage,
            kind: if has_compute {
                PassKind::Compute
            } else {
                PassKind::Graphics
            },
            shaders,
            render_targets,
            work_groups,
        });
    }
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
    let digits: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().unwrap_or(0)
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
            if targets.len() as u32 > caps.max_color_attachments {
                missing.push(format!(
                    "{}: {} color attachments required (max {})",
                    pass.name,
                    targets.len(),
                    caps.max_color_attachments
                ));
            }
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
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        ShaderCompatibilityReport::Compatible
    } else {
        ShaderCompatibilityReport::MissingCapabilities { missing }
    }
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

fn contains_ident(src: &str, ident: &str) -> bool {
    let bytes = src.as_bytes();
    let needle = ident.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(rel) = src[start..].find(ident) {
        let pos = start + rel;
        let before_ok = pos == 0 || !is_ident(bytes[pos - 1]);
        let after = pos + needle.len();
        let after_ok = after == bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = pos + 1;
    }
    false
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
            for idx in 0u8..16 {
                if contains_ident(s, &format!("colortex{idx}")) {
                    req.color_attachments.insert(idx);
                }
            }
            for &(alias, idx) in LEGACY_COLOR_ALIASES {
                if contains_ident(s, alias) {
                    req.color_attachments.insert(idx);
                }
            }
            for idx in 0u8..=2 {
                if contains_ident(s, &format!("depthtex{idx}")) {
                    req.depth_attachments.insert(idx);
                }
            }
            for idx in 0u8..=1 {
                if contains_ident(s, &format!("shadowtex{idx}")) {
                    req.shadow_depth_attachments.insert(idx);
                }
                if contains_ident(s, &format!("shadowcolor{idx}")) {
                    req.shadow_color_attachments.insert(idx);
                }
            }
            if contains_ident(s, "shadow") && contains_ident(s, "sampler2D") {
                req.shadow_depth_attachments.insert(0);
            }
            if contains_ident(s, "heldItemId") || contains_ident(s, "heldBlockLightValue") {
                req.uses_main_hand_light = true;
            }
            if contains_ident(s, "heldItemId2") || contains_ident(s, "heldBlockLightValue2") {
                req.uses_off_hand_light = true;
            }
            if contains_ident(s, "dhDepthTex0")
                || contains_ident(s, "dhDepthTex1")
                || contains_ident(s, "dhRenderDistance")
                || contains_ident(s, "dhNearPlane")
                || contains_ident(s, "dhFarPlane")
            {
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

fn apply_option_overrides_to_unit(
    unit: &mut PreprocessedShader,
    overrides: &BTreeMap<String, Option<String>>,
) {
    let mut new_lines = Vec::new();
    let mut changed = false;
    for line in unit.source.split('\n') {
        let trimmed = line.trim_start();
        let (was_commented, after_hash) = if let Some(rest) = trimmed.strip_prefix("//") {
            let r = rest.trim_start();
            if let Some(h) = r.strip_prefix('#') {
                (true, Some(h.trim_start()))
            } else {
                (false, None)
            }
        } else if let Some(h) = trimmed.strip_prefix('#') {
            (false, Some(h.trim_start()))
        } else {
            (false, None)
        };
        let mut replaced = None;
        if let Some(rest) = after_hash {
            if let Some(def_rest) = rest.strip_prefix("define") {
                if def_rest.starts_with([' ', '\t']) {
                    let body = def_rest.trim_start();
                    let name: String = body
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if let Some(override_val) = overrides.get(&name) {
                        changed = true;
                        let replacement = match override_val {
                            None => format!("// #define {name}"),
                            Some(v) if v.is_empty() => format!("#define {name}"),
                            Some(v) => format!("#define {name} {v}"),
                        };
                        let opt = unit.defines.entry(name.clone()).or_insert(ShaderOption {
                            name: name.clone(),
                            value: None,
                            enabled: !was_commented,
                        });
                        match override_val {
                            None => {
                                opt.enabled = false;
                            }
                            Some(v) => {
                                opt.enabled = true;
                                opt.value = if v.is_empty() { None } else { Some(v.clone()) };
                            }
                        }
                        replaced = Some(replacement);
                    }
                }
            }
        }
        match replaced {
            Some(r) => new_lines.push(r),
            None => new_lines.push(line.to_owned()),
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

    /// Look up the localized display name for an option, falling back to `name`.
    pub fn option_label<'a>(&'a self, name: &'a str) -> &'a str {
        self.entries
            .get(&format!("option.{name}"))
            .map(String::as_str)
            .unwrap_or(name)
    }

    /// Look up the localized tooltip/comment for an option, if present.
    pub fn option_comment(&self, name: &str) -> Option<&str> {
        self.entries
            .get(&format!("option.{name}.comment"))
            .map(String::as_str)
    }

    /// Look up a localized option value label (`value.<OPTION>.<VAL>`),
    /// falling back to `val`.
    pub fn value_label<'a>(&'a self, option: &str, val: &'a str) -> &'a str {
        self.entries
            .get(&format!("value.{option}.{val}"))
            .map(String::as_str)
            .unwrap_or(val)
    }

    /// Look up a localized profile name (`profile.<NAME>`), falling back to `name`.
    pub fn profile_label<'a>(&'a self, name: &'a str) -> &'a str {
        self.entries
            .get(&format!("profile.{name}"))
            .map(String::as_str)
            .unwrap_or(name)
    }

    /// Look up a localized screen title (`screen.<NAME>`), falling back to `name`.
    pub fn screen_label<'a>(&'a self, name: &'a str) -> &'a str {
        self.entries
            .get(&format!("screen.{name}"))
            .map(String::as_str)
            .unwrap_or(name)
    }
}

impl ShaderPackSource {
    /// Sorted list of locale names available under `lang/<locale>.lang`.
    pub fn available_locales(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        for path in self.list() {
            if let Some(rest) = path.strip_prefix("lang/") {
                if let Some(stem) = rest.strip_suffix(".lang") {
                    if !stem.contains('/') && !stem.is_empty() {
                        out.insert(stem.to_owned());
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// Load `lang/<locale>.lang` (case-insensitive locale match) layered over
    /// `lang/en_US.lang` so missing translations fall back cleanly to English
    /// and then to the raw key.
    pub fn load_lang(&self, locale: &str) -> ShaderPackLang {
        let find_file = |want: &str| -> Option<(String, String)> {
            let want_lc = want.to_ascii_lowercase();
            for path in self.list() {
                if let Some(rest) = path.strip_prefix("lang/") {
                    if let Some(stem) = rest.strip_suffix(".lang") {
                        if !stem.contains('/') && stem.to_ascii_lowercase() == want_lc {
                            if let Some(text) = self.read_text(path) {
                                return Some((stem.to_owned(), text));
                            }
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

/// Load and validate a Megu native WGSL pack (`megu-shader.toml` + `.wgsl`
/// files) against `caps`.
pub fn load_megu_wgsl_pack(
    source: &ShaderPackSource,
    caps: &GpuShaderCaps,
) -> Result<MeguWgslPack, ShaderPackError> {
    let text = source
        .read_text(MEGU_WGSL_MANIFEST)
        .ok_or(ShaderPackError::EmptyPack)?;
    let mut name = String::from("megu-wgsl-pack");
    let mut author = String::new();
    let mut version = String::from("0.1.0");
    let mut api_version = 1u32;
    let mut requires_compute = false;
    let mut requires_ssbo = false;
    let mut supports_lod = false;

    #[derive(Default)]
    struct RawPass {
        stage: Option<PipelineStage>,
        kind: Option<PassKind>,
        file: Option<String>,
        targets: Vec<String>,
        work_groups: Option<[u32; 3]>,
    }
    let mut section = String::new();
    let mut raw_passes: BTreeMap<String, RawPass> = BTreeMap::new();

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
                raw_passes.entry(pass_name.trim().to_owned()).or_default();
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(malformed_props(
                MEGU_WGSL_MANIFEST,
                line_no,
                "expected key = value",
            ));
        };
        let key = k.trim();
        let val = v.trim();
        if section.is_empty() || section == "pack" {
            match key {
                "name" => name = unquote_toml_str(val),
                "author" => author = unquote_toml_str(val),
                "version" => version = unquote_toml_str(val),
                "api" | "api_version" => {
                    api_version = val.parse().map_err(|_| {
                        malformed_props(MEGU_WGSL_MANIFEST, line_no, "invalid api_version")
                    })?;
                }
                "requires_compute" => requires_compute = val == "true",
                "requires_ssbo" => requires_ssbo = val == "true",
                "supports_lod" => supports_lod = val == "true",
                _ => {}
            }
        } else if let Some(pass_name) = section.strip_prefix("pass.") {
            let entry = raw_passes.entry(pass_name.trim().to_owned()).or_default();
            match key {
                "stage" => {
                    let s = unquote_toml_str(val);
                    entry.stage = Some(parse_megu_stage(&s).ok_or_else(|| {
                        malformed_props(MEGU_WGSL_MANIFEST, line_no, "unknown pipeline stage")
                    })?);
                }
                "kind" => {
                    let s = unquote_toml_str(val).to_ascii_lowercase();
                    entry.kind = Some(match s.as_str() {
                        "compute" => PassKind::Compute,
                        "graphics" => PassKind::Graphics,
                        _ => {
                            return Err(malformed_props(
                                MEGU_WGSL_MANIFEST,
                                line_no,
                                "kind must be graphics or compute",
                            ));
                        }
                    });
                }
                "file" => entry.file = Some(unquote_toml_str(val)),
                "targets" => {
                    let inner = val.trim_start_matches('[').trim_end_matches(']');
                    entry.targets = inner
                        .split(',')
                        .map(unquote_toml_str)
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "workgroups" => {
                    let inner = val.trim_start_matches('[').trim_end_matches(']');
                    let nums: Result<Vec<u32>, _> = inner
                        .split(',')
                        .map(|s| s.trim().parse::<u32>())
                        .collect();
                    let nums = nums.map_err(|_| {
                        malformed_props(MEGU_WGSL_MANIFEST, line_no, "bad workgroups")
                    })?;
                    if nums.len() != 3 || nums.contains(&0) {
                        return Err(malformed_props(
                            MEGU_WGSL_MANIFEST,
                            line_no,
                            "workgroups must be 3 positive integers",
                        ));
                    }
                    entry.work_groups = Some([nums[0], nums[1], nums[2]]);
                }
                _ => {}
            }
        }
    }

    if raw_passes.is_empty() {
        return Err(ShaderPackError::EmptyPack);
    }

    let mut passes = Vec::with_capacity(raw_passes.len());
    for (pass_name, rp) in raw_passes {
        let stage = rp.stage.unwrap_or(PipelineStage::Gbuffers);
        let kind = rp.kind.unwrap_or(PassKind::Graphics);
        let file = rp.file.ok_or_else(|| ShaderPackError::IncompleteProgram {
            name: pass_name.clone(),
            reason: "WGSL pass missing `file`".to_owned(),
        })?;
        validate_virtual_path(&file)?;
        let wgsl_source = source.read_text(&file).ok_or_else(|| {
            ShaderPackError::IncludeNotFound {
                include: file.clone(),
                from: MEGU_WGSL_MANIFEST.to_owned(),
            }
        })?;
        if stage == PipelineStage::DistantHorizons {
            supports_lod = true;
        }
        if kind == PassKind::Compute {
            requires_compute = true;
        }
        passes.push(MeguWgslPass {
            name: pass_name,
            stage,
            kind,
            file,
            wgsl_source,
            render_targets: rp.targets,
            work_groups: rp.work_groups,
        });
    }
    passes.sort_by(|a, b| a.stage.cmp(&b.stage).then_with(|| a.name.cmp(&b.name)));

    let mut missing = Vec::new();
    if api_version != 1 {
        missing.push(format!(
            "unsupported megu-shader API version {api_version} (supported: 1)"
        ));
    }
    if requires_compute && !caps.supports_compute {
        missing.push("compute shaders required by WGSL pack".to_owned());
    }
    if requires_ssbo && !caps.supports_ssbo {
        missing.push("shader storage buffers (SSBO) required by WGSL pack".to_owned());
    }
    for pass in &passes {
        if pass.render_targets.len() as u32 > caps.max_color_attachments {
            missing.push(format!(
                "{}: {} color attachments required (max {})",
                pass.name,
                pass.render_targets.len(),
                caps.max_color_attachments
            ));
        }
    }
    missing.sort();
    missing.dedup();
    let compatibility = if missing.is_empty() {
        ShaderCompatibilityReport::Compatible
    } else {
        ShaderCompatibilityReport::MissingCapabilities { missing }
    };

    Ok(MeguWgslPack {
        name,
        author,
        version,
        api_version,
        requires_compute,
        requires_ssbo,
        supports_lod,
        passes,
        compatibility,
    })
}
