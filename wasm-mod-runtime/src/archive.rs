// SPDX-License-Identifier: LGPL-3.0-or-later

//! Archive loading: pull `META-INF/megu/mod.wasm` and `megu-mod.toml`
//! capability declarations out of `.mmod` or standard Modrinth `.jar`
//! bytes — fully in memory, zero disk extraction.
//!
//! Decompression and ZIP structural validation are delegated to
//! `megu_core::zip_archive::ZipSliceIndex`, the same bounded reader the
//! SDK uses.

use std::collections::BTreeSet;

use megu_core::zip_archive::ZipSliceIndex;

use crate::abi::{Capability, CapabilitySet, MeguHost, MANIFEST_ENTRY, MOD_WASM_ENTRY};
use crate::error::{LoadError, ModFault};
use crate::wasm_mod::{ModConfig, WasmMod};

/// The extracted mod payload: guest bytes plus the capability set the
/// embedded manifest declares.
pub struct ArchiveMod {
    pub wasm: Vec<u8>,
    pub capabilities: CapabilitySet,
    /// Raw `megu-mod.toml` text when present.
    pub manifest: Option<String>,
}

/// Read the megu payload from `.mmod`/`.jar` archive bytes.
///
/// Fails closed when `META-INF/megu/mod.wasm` is absent, the archive is
/// malformed, the embedded manifest's `[capabilities]` table names an
/// unknown capability, or any entry exceeds its budget.
pub fn inspect_archive(bytes: &[u8], config: &ModConfig) -> Result<ArchiveMod, LoadError> {
    let zip = ZipSliceIndex::open_filtered(bytes, |name| {
        name == MOD_WASM_ENTRY || name == MANIFEST_ENTRY
    })
    .map_err(|e| LoadError::Archive(format!("zip index: {e}")))?;

    let wasm = zip
        .read_entry(MOD_WASM_ENTRY, config.max_module_bytes)
        .map_err(|e| {
            LoadError::Archive(format!("missing `{MOD_WASM_ENTRY}`: {e}"))
        })?;

    let manifest = match zip.read_entry(MANIFEST_ENTRY, config.max_manifest_bytes) {
        Ok(bytes) => Some(
            String::from_utf8(bytes)
                .map_err(|_| LoadError::Archive("manifest not UTF-8".into()))?,
        ),
        // Absent manifest = zero granted capabilities; any other read
        // failure (oversized, corrupt) fails closed.
        Err(megu_core::zip_archive::ZipError::EntryNotFound(_)) => None,
        Err(e) => {
            return Err(LoadError::Archive(format!("manifest `{MANIFEST_ENTRY}`: {e}")));
        }
    };

    let capabilities = match &manifest {
        Some(text) => parse_capabilities(text).map_err(LoadError::Archive)?,
        None => CapabilitySet::NONE,
    };

    Ok(ArchiveMod {
        wasm,
        capabilities,
        manifest,
    })
}

/// Minimal, strict `[capabilities]` table scanner for `megu-mod.toml`.
/// Recognizes `key = true`/`false` lines inside `[capabilities]` only;
/// unknown capability names fail closed. `= true` grants, `= false` or
/// absence denies.
fn parse_capabilities(toml: &str) -> Result<CapabilitySet, String> {
    let mut caps = CapabilitySet::NONE;
    let mut declared = BTreeSet::new();
    let mut seen_table = false;
    let mut in_caps = false;
    let mut multiline = None;
    for raw_line in toml.lines() {
        let Some(line) = manifest_line(raw_line, &mut multiline) else {
            continue;
        };
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_caps = line
                .strip_prefix('[')
                .and_then(|header| header.strip_suffix(']'))
                .is_some_and(|header| unquote(header.trim()) == "capabilities");
            if in_caps && std::mem::replace(&mut seen_table, true) {
                return Err("duplicate [capabilities] table".into());
            }
            continue;
        }
        if !in_caps {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("malformed capability line `{line}`"));
        };
        let key = unquote(key.trim());
        let value = value.trim();
        let cap = Capability::from_manifest_key(key)
            .ok_or_else(|| format!("unknown capability `{key}`"))?;
        if !declared.insert(cap) {
            return Err(format!("duplicate capability `{key}`"));
        }
        match value {
            "true" => caps = caps.granted(cap),
            "false" => {}
            _ => return Err(format!("capability `{key}` must be true or false")),
        }
    }
    if declared.contains(&Capability::BlockRegistry)
        && (declared.contains(&Capability::ReadBlocks) || declared.contains(&Capability::WriteBlocks))
    {
        return Err("block_registry overlaps read_blocks/write_blocks declarations".into());
    }
    Ok(caps)
}

fn unquote(key: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = key.strip_prefix(quote).and_then(|key| key.strip_suffix(quote)) {
            return inner;
        }
    }
    key
}

/// Remove comments outside strings and skip continuation lines of unrelated
/// multiline TOML strings, which can themselves contain table-like text.
fn manifest_line<'a>(line: &'a str, multiline: &mut Option<u8>) -> Option<&'a str> {
    let continuation = multiline.is_some();
    let mut quoted = None;
    let mut end = line.len();
    let bytes = line.as_bytes();
    let mut offset = 0;
    while let Some(&byte) = bytes.get(offset) {
        if let Some(quote) = multiline.or(quoted) {
            if byte == b'\\' && quote == b'"' {
                offset += 2;
                continue;
            }
            if byte == quote {
                if multiline.is_some() && bytes.get(offset..offset + 3) == Some(&[quote; 3]) {
                    *multiline = None;
                    offset += 3;
                    continue;
                }
                quoted = None;
            }
        } else if matches!(byte, b'"' | b'\'') {
            if bytes.get(offset..offset + 3) == Some(&[byte; 3]) {
                *multiline = Some(byte);
                offset += 3;
                continue;
            }
            quoted = Some(byte);
        } else if byte == b'#' {
            end = offset;
            break;
        }
        offset += 1;
    }
    (!continuation).then(|| line[..end].trim())
}

impl WasmMod {
    /// Load a mod directly from `.mmod`/`.jar` archive bytes. The granted
    /// capability set is exactly what the embedded `megu-mod.toml`
    /// declares — an undeclared host call fails closed at call time.
    pub fn from_archive(
        bytes: &[u8],
        host: Box<dyn MeguHost>,
        config: ModConfig,
    ) -> Result<Self, ModFault> {
        let payload = inspect_archive(bytes, &config)?;
        WasmMod::from_wasm(&payload.wasm, host, payload.capabilities, config)
    }
}
