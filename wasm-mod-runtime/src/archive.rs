// SPDX-License-Identifier: LGPL-3.0-or-later

//! Archive loading: pull `META-INF/megu/mod.wasm` and `megu-mod.toml`
//! capability declarations out of `.mmod` or standard Modrinth `.jar`
//! bytes — fully in memory, zero disk extraction.
//!
//! Decompression and ZIP structural validation are delegated to
//! `megu_core::zip_archive::ZipSliceIndex`, the same bounded reader the
//! SDK uses.

use megu_core::zip_archive::ZipSliceIndex;

use crate::abi::{CapabilitySet, MANIFEST_ENTRY, MOD_WASM_ENTRY};
use crate::error::LoadError;
use crate::wasm_mod::{ModConfig, WasmMod};
use crate::abi::MeguHost;
use crate::error::ModFault;

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
    let mut in_caps = false;
    for raw_line in toml.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_caps = line == "[capabilities]";
            continue;
        }
        if !in_caps {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("malformed capability line `{line}`"));
        };
        let key = key.trim().trim_matches('"');
        let value = value.trim();
        let cap = crate::abi::Capability::from_manifest_key(key)
            .ok_or_else(|| format!("unknown capability `{key}`"))?;
        match value {
            "true" => caps = caps.granted(cap),
            "false" => {}
            _ => return Err(format!("capability `{key}` must be true or false")),
        }
    }
    Ok(caps)
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
