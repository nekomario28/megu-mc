// SPDX-License-Identifier: LGPL-3.0-or-later
//! Transactional mod-profile generations, activation diffs, and rollback.

use megu_core::crypto::{hex_encode, Sha256};
use megu_core::hash::crc32_ieee;
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_core::zip_archive::{validate_entry_name, LazyZipIndex};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const GENERATION_MANIFEST_FILE: &str = "generation_manifest.json";
pub const ACTIVE_GENERATION_FILE: &str = "active_generation.json";
pub const KNOWN_GOOD_GENERATION_FILE: &str = "known_good_generation.json";
pub const MAX_GENERATION_MODS: usize = 64;
pub const MAX_JAR_ENTRIES: usize = 4_096;
pub const MAX_JAR_ENTRY_UNCOMPRESSED_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_JAR_TOTAL_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_JAR_BYTES: u64 = 128 * 1024 * 1024;
pub const SOURCE_AUTOFETCH_DEPS: &str = "autofetch_deps";
pub const SOURCE_SERVER_SYNC: &str = "server_sync";
pub const SOURCE_USER_EDIT: &str = "user_edit";

static GENERATION_COUNTER: AtomicU64 = AtomicU64::new(0);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationModEntry {
    pub mod_id: String,
    pub version: String,
    pub filename: String,
    pub sha512: String,
    pub sha256: Option<String>,
    pub source_reason: String,
    pub modrinth_project_id: Option<String>,
    pub modrinth_version_id: Option<String>,
    pub requested_by: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationManifest {
    pub generation_id: String,
    pub created_unix_ms: u64,
    pub source_reason: String,
    pub mods: Vec<GenerationModEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationSummary {
    pub generation_id: String,
    pub created_unix_ms: u64,
    pub mod_count: usize,
    pub active: bool,
    pub known_good: bool,
    pub source_reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileModSummary {
    pub mod_id: String,
    pub version: String,
    pub filename: String,
    pub sha512: String,
    pub source_reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModVersionChange {
    pub mod_id: String,
    pub from_version: String,
    pub to_version: String,
    pub from_filename: String,
    pub to_filename: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ProfileActivationDiff {
    pub from_generation_id: Option<String>,
    pub to_generation_id: String,
    pub added_mods: Vec<ProfileModSummary>,
    pub removed_mods: Vec<ProfileModSummary>,
    pub updated_mods: Vec<ModVersionChange>,
    pub auto_fetched_deps: Vec<ProfileModSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRollbackResult {
    pub rolled_back_generation_id: Option<String>,
    pub restored_generation_id: String,
    pub diff: ProfileActivationDiff,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JarArchiveReport {
    pub entries: usize,
    pub uncompressed_bytes: u64,
}

#[derive(Debug)]
pub struct StagedGeneration {
    pub generation_id: String,
    pub generation_dir: PathBuf,
    complete: bool,
}

impl StagedGeneration {
    pub fn mark_published(&mut self) {
        self.complete = true;
    }
}

impl Drop for StagedGeneration {
    fn drop(&mut self) {
        if !self.complete {
            let _ = fs::remove_dir_all(&self.generation_dir);
        }
    }
}

pub struct ModProfileStore {
    profile_root: PathBuf,
}

impl ModProfileStore {
    pub fn open(profile_root: impl AsRef<Path>) -> Result<Self, String> {
        let profile_root = profile_root.as_ref().to_path_buf();
        reject_symlink_path(&profile_root)?;
        fs::create_dir_all(profile_root.join("generations"))
            .map_err(|e| format!("create generations directory: {e}"))?;
        reject_symlink_path(&profile_root.join("generations"))?;
        Ok(Self { profile_root })
    }

    pub fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    pub fn generations_root(&self) -> PathBuf {
        self.profile_root.join("generations")
    }

    pub fn stage_generation(&self, source_reason: &str) -> Result<StagedGeneration, String> {
        validate_source_reason(source_reason)?;
        for _ in 0..128 {
            let millis = unix_millis()?;
            let counter = GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let generation_id = format!("gen-{millis:x}-{}-{counter:x}", std::process::id());
            validate_generation_id(&generation_id)?;
            let generation_dir = self.generations_root().join(&generation_id);
            match fs::create_dir(&generation_dir) {
                Ok(()) => {
                    reject_symlink_path(&generation_dir)?;
                    fsync_dir(&self.generations_root())?;
                    return Ok(StagedGeneration {
                        generation_id,
                        generation_dir,
                        complete: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("create staged generation: {e}")),
            }
        }
        Err("could not allocate a unique generation id".into())
    }

    pub fn publish_staged_generation(
        &self,
        staged: &mut StagedGeneration,
        source_reason: &str,
        mods: Vec<GenerationModEntry>,
    ) -> Result<ProfileActivationDiff, String> {
        validate_source_reason(source_reason)?;
        if mods.len() > MAX_GENERATION_MODS {
            return Err(format!("generation exceeds {MAX_GENERATION_MODS} mods"));
        }
        let manifest = GenerationManifest {
            generation_id: staged.generation_id.clone(),
            created_unix_ms: unix_millis()?,
            source_reason: source_reason.to_owned(),
            mods,
        };
        verify_generation_files(&staged.generation_dir, &manifest)?;
        write_manifest(&staged.generation_dir, &manifest)?;
        fsync_dir(&staged.generation_dir)?;
        staged.mark_published();
        self.activate_generation_atomic(&staged.generation_id)
    }

    pub fn activate_generation_atomic(
        &self,
        generation_id: &str,
    ) -> Result<ProfileActivationDiff, String> {
        validate_generation_id(generation_id)?;
        let generation_dir = self.generations_root().join(generation_id);
        reject_symlink_path(&generation_dir)?;
        let target = read_manifest(&generation_dir)?;
        if target.generation_id != generation_id {
            return Err("generation manifest id does not match directory".into());
        }
        verify_generation_files(&generation_dir, &target)?;
        let previous = self.active_generation_id().ok().flatten();
        let previous_manifest = previous
            .as_deref()
            .map(|id| read_manifest(&self.generations_root().join(id)))
            .transpose()?;
        let active = self.profile_root.join(ACTIVE_GENERATION_FILE);
        reject_symlink_path(&active)?;
        let body = JsonValue::Object(vec![(
            "generation_id".to_owned(),
            JsonValue::String(generation_id.to_owned()),
        )])
        .to_json_string();
        atomic_write(&active, body.as_bytes())?;
        fsync_dir(&self.profile_root)?;
        Ok(diff_manifests(previous_manifest.as_ref(), &target))
    }

    pub fn record_known_good_generation(&self, generation_id: &str) -> Result<(), String> {
        validate_generation_id(generation_id)?;
        let manifest = read_manifest(&self.generations_root().join(generation_id))?;
        verify_generation_files(&self.generations_root().join(generation_id), &manifest)?;
        let path = self.profile_root.join(KNOWN_GOOD_GENERATION_FILE);
        reject_symlink_path(&path)?;
        let body = JsonValue::Object(vec![(
            "generation_id".to_owned(),
            JsonValue::String(generation_id.to_owned()),
        )])
        .to_json_string();
        atomic_write(&path, body.as_bytes())?;
        fsync_dir(&self.profile_root)
    }

    pub fn rollback_to_last_good_generation(&self) -> Result<ProfileRollbackResult, String> {
        let known_good = self
            .known_good_generation_id()?
            .ok_or("no known-good mod generation has been recorded")?;
        let current = self.active_generation_id().ok().flatten();
        let diff = self.activate_generation_atomic(&known_good)?;
        Ok(ProfileRollbackResult {
            rolled_back_generation_id: current.filter(|id| id != &known_good),
            restored_generation_id: known_good,
            diff,
        })
    }

    pub fn list_generations(&self) -> Result<Vec<GenerationSummary>, String> {
        let active = self.active_generation_id().ok().flatten();
        let known_good = self.known_good_generation_id().ok().flatten();
        let mut out = Vec::new();
        let root = self.generations_root();
        for entry in fs::read_dir(&root).map_err(|e| format!("read generations: {e}"))? {
            let entry = entry.map_err(|e| format!("read generation entry: {e}"))?;
            let path = entry.path();
            if !path.is_dir() || !path.join(GENERATION_MANIFEST_FILE).is_file() {
                continue;
            }
            let manifest = read_manifest(&path)?;
            out.push(GenerationSummary {
                generation_id: manifest.generation_id.clone(),
                created_unix_ms: manifest.created_unix_ms,
                mod_count: manifest.mods.len(),
                active: active.as_deref() == Some(manifest.generation_id.as_str()),
                known_good: known_good.as_deref() == Some(manifest.generation_id.as_str()),
                source_reason: manifest.source_reason,
            });
        }
        out.sort_by(|a, b| a.generation_id.cmp(&b.generation_id));
        Ok(out)
    }

    pub fn diff_generations(
        &self,
        from_id: &str,
        to_id: &str,
    ) -> Result<ProfileActivationDiff, String> {
        validate_generation_id(from_id)?;
        validate_generation_id(to_id)?;
        let from = read_manifest(&self.generations_root().join(from_id))?;
        let to = read_manifest(&self.generations_root().join(to_id))?;
        Ok(diff_manifests(Some(&from), &to))
    }

    pub fn active_generation_id(&self) -> Result<Option<String>, String> {
        read_generation_pointer(&self.profile_root.join(ACTIVE_GENERATION_FILE))
    }

    pub fn known_good_generation_id(&self) -> Result<Option<String>, String> {
        read_generation_pointer(&self.profile_root.join(KNOWN_GOOD_GENERATION_FILE))
    }

    pub fn active_generation_dir(&self) -> Result<Option<PathBuf>, String> {
        Ok(self
            .active_generation_id()?
            .map(|id| self.generations_root().join(id)))
    }

    pub fn manifest(&self, generation_id: &str) -> Result<GenerationManifest, String> {
        validate_generation_id(generation_id)?;
        read_manifest(&self.generations_root().join(generation_id))
    }
}

pub fn validate_member_filename(filename: &str) -> Result<(), String> {
    if filename.is_empty()
        || filename.contains("..")
        || filename
            .bytes()
            .any(|b| matches!(b, b'/' | b'\\' | b':' | 0))
        || !filename.to_ascii_lowercase().ends_with(".jar")
    {
        return Err("unsafe mod filename".into());
    }
    Ok(())
}

pub fn validate_generation_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id.len() <= 96
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(())
    } else {
        Err("invalid generation id".into())
    }
}

fn validate_source_reason(reason: &str) -> Result<(), String> {
    if matches!(
        reason,
        SOURCE_AUTOFETCH_DEPS | SOURCE_SERVER_SYNC | SOURCE_USER_EDIT
    ) {
        Ok(())
    } else {
        Err("unknown generation source reason".into())
    }
}

fn read_generation_pointer(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    reject_symlink_path(path)?;
    let bytes = fs::read(path).map_err(|e| format!("read generation pointer: {e}"))?;
    if bytes.len() > 4_096 {
        return Err("generation pointer is too large".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "generation pointer is not UTF-8")?;
    let json = parse_json_with_limits(text, small_json_limits(), true)
        .map_err(|e| format!("invalid generation pointer: {e}"))?;
    let id = json
        .get("generation_id")
        .and_then(JsonValue::as_str)
        .ok_or("generation pointer lacks generation_id")?;
    validate_generation_id(id)?;
    Ok(Some(id.to_owned()))
}

fn read_manifest(generation_dir: &Path) -> Result<GenerationManifest, String> {
    reject_symlink_path(generation_dir)?;
    let path = generation_dir.join(GENERATION_MANIFEST_FILE);
    reject_symlink_path(&path)?;
    let bytes = fs::read(&path).map_err(|e| format!("read generation manifest: {e}"))?;
    if bytes.len() > 1024 * 1024 {
        return Err("generation manifest is too large".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "generation manifest is not UTF-8")?;
    let json = parse_json_with_limits(text, small_json_limits(), true)
        .map_err(|e| format!("invalid generation manifest: {e}"))?;
    manifest_from_json(&json)
}

fn write_manifest(generation_dir: &Path, manifest: &GenerationManifest) -> Result<(), String> {
    let json = manifest_to_json(manifest).to_json_string();
    atomic_write(
        &generation_dir.join(GENERATION_MANIFEST_FILE),
        json.as_bytes(),
    )
}

fn small_json_limits() -> JsonLimits {
    JsonLimits {
        max_depth: 32,
        max_nodes: 20_000,
        max_bytes: 1024 * 1024,
    }
}

fn manifest_to_json(manifest: &GenerationManifest) -> JsonValue {
    JsonValue::Object(vec![
        (
            "generation_id".into(),
            JsonValue::String(manifest.generation_id.clone()),
        ),
        (
            "created_unix_ms".into(),
            JsonValue::Number(manifest.created_unix_ms as f64),
        ),
        (
            "source_reason".into(),
            JsonValue::String(manifest.source_reason.clone()),
        ),
        (
            "mods".into(),
            JsonValue::Array(manifest.mods.iter().map(mod_entry_to_json).collect()),
        ),
    ])
}

fn mod_entry_to_json(entry: &GenerationModEntry) -> JsonValue {
    let mut object = vec![
        ("mod_id".into(), JsonValue::String(entry.mod_id.clone())),
        ("version".into(), JsonValue::String(entry.version.clone())),
        ("filename".into(), JsonValue::String(entry.filename.clone())),
        ("sha512".into(), JsonValue::String(entry.sha512.clone())),
        (
            "source_reason".into(),
            JsonValue::String(entry.source_reason.clone()),
        ),
        (
            "requested_by".into(),
            JsonValue::Array(
                entry
                    .requested_by
                    .iter()
                    .cloned()
                    .map(JsonValue::String)
                    .collect(),
            ),
        ),
    ];
    if let Some(value) = &entry.sha256 {
        object.push(("sha256".into(), JsonValue::String(value.clone())));
    }
    if let Some(value) = &entry.modrinth_project_id {
        object.push((
            "modrinth_project_id".into(),
            JsonValue::String(value.clone()),
        ));
    }
    if let Some(value) = &entry.modrinth_version_id {
        object.push((
            "modrinth_version_id".into(),
            JsonValue::String(value.clone()),
        ));
    }
    JsonValue::Object(object)
}

fn manifest_from_json(json: &JsonValue) -> Result<GenerationManifest, String> {
    let generation_id = required_str(json, "generation_id")?.to_owned();
    validate_generation_id(&generation_id)?;
    let created_unix_ms = json
        .get("created_unix_ms")
        .and_then(JsonValue::as_u64)
        .ok_or("generation manifest lacks created_unix_ms")?;
    let source_reason = required_str(json, "source_reason")?.to_owned();
    validate_source_reason(&source_reason)?;
    let mods = json
        .get("mods")
        .and_then(JsonValue::as_array)
        .ok_or("generation manifest lacks mods")?;
    if mods.len() > MAX_GENERATION_MODS {
        return Err(format!("generation exceeds {MAX_GENERATION_MODS} mods"));
    }
    let mut entries = Vec::with_capacity(mods.len());
    for item in mods {
        let entry = GenerationModEntry {
            mod_id: required_str(item, "mod_id")?.to_owned(),
            version: required_str(item, "version")?.to_owned(),
            filename: required_str(item, "filename")?.to_owned(),
            sha512: required_str(item, "sha512")?.to_ascii_lowercase(),
            sha256: item
                .get("sha256")
                .and_then(JsonValue::as_str)
                .map(str::to_ascii_lowercase),
            source_reason: required_str(item, "source_reason")?.to_owned(),
            modrinth_project_id: item
                .get("modrinth_project_id")
                .and_then(JsonValue::as_str)
                .map(str::to_owned),
            modrinth_version_id: item
                .get("modrinth_version_id")
                .and_then(JsonValue::as_str)
                .map(str::to_owned),
            requested_by: item
                .get("requested_by")
                .and_then(JsonValue::as_array)
                .ok_or("generation mod lacks requested_by")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "requested_by entries must be strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        validate_member_filename(&entry.filename)?;
        validate_hex(&entry.sha512, 128, "manifest SHA-512")?;
        if let Some(sha256) = &entry.sha256 {
            validate_hex(sha256, 64, "manifest SHA-256")?;
        }
        validate_source_reason(&entry.source_reason)?;
        entries.push(entry);
    }
    Ok(GenerationManifest {
        generation_id,
        created_unix_ms,
        source_reason,
        mods: entries,
    })
}

fn required_str<'a>(value: &'a JsonValue, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("generation manifest lacks {key}"))
}

fn verify_generation_files(
    generation_dir: &Path,
    manifest: &GenerationManifest,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    let mut seen_mod_ids = BTreeSet::new();
    for entry in &manifest.mods {
        validate_member_filename(&entry.filename)?;
        if !seen.insert(entry.filename.clone()) {
            return Err("duplicate mod filename in generation".into());
        }
        if !seen_mod_ids.insert(entry.mod_id.clone()) {
            return Err("duplicate mod id in generation".into());
        }
        let path = generation_dir.join(&entry.filename);
        reject_symlink_path(&path)?;
        let metadata = fs::metadata(&path).map_err(|e| format!("read staged mod metadata: {e}"))?;
        if !metadata.is_file() {
            return Err("staged mod path is not a regular file".into());
        }
        if metadata.len() == 0 || metadata.len() > MAX_JAR_BYTES {
            return Err("staged mod size outside allowed bounds".into());
        }
        let hashes = hash_file_hex(&path)?;
        if !hashes.0.eq_ignore_ascii_case(&entry.sha512) {
            return Err(format!("{} SHA-512 changed after staging", entry.filename));
        }
        if let Some(expected) = &entry.sha256 {
            if !hashes.1.eq_ignore_ascii_case(expected) {
                return Err(format!("{} SHA-256 changed after staging", entry.filename));
            }
        }
        verify_jar_archive_safety(&path)?;
    }
    for entry in
        fs::read_dir(generation_dir).map_err(|e| format!("read generation directory: {e}"))?
    {
        let entry = entry.map_err(|e| format!("read generation file entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err("generation contains a non-UTF-8 filename".into());
        };
        if name == GENERATION_MANIFEST_FILE {
            continue;
        }
        if !seen.contains(name) {
            return Err(format!("generation contains unmanifested file {name}"));
        }
        let file_type = entry
            .file_type()
            .map_err(|e| format!("inspect generation entry: {e}"))?;
        if !file_type.is_file() {
            return Err("generation contains a non-regular file".into());
        }
    }
    Ok(())
}

pub fn verify_jar_archive_safety(path: &Path) -> Result<JarArchiveReport, String> {
    reject_symlink_path(path)?;
    let metadata = fs::metadata(path).map_err(|e| format!("read archive metadata: {e}"))?;
    if metadata.len() == 0 || metadata.len() > MAX_JAR_BYTES {
        return Err("archive size outside allowed bounds".into());
    }
    reject_unsafe_zip_central_names(path)?;
    let index =
        LazyZipIndex::open_all(path).map_err(|e| format!("invalid JAR/ZIP archive: {e}"))?;
    if index.entries().len() > MAX_JAR_ENTRIES {
        return Err(format!("JAR entry count exceeds {MAX_JAR_ENTRIES}"));
    }
    let mut total_uncompressed = 0u64;
    for entry in index.entries() {
        validate_entry_name(&entry.name).map_err(|e| format!("unsafe JAR entry: {e}"))?;
        total_uncompressed = total_uncompressed
            .checked_add(entry.uncompressed_size)
            .ok_or("JAR uncompressed size overflow")?;
        if total_uncompressed > MAX_JAR_TOTAL_UNCOMPRESSED_BYTES {
            return Err("JAR total uncompressed bytes exceed cap".into());
        }
        if entry.uncompressed_size > MAX_JAR_ENTRY_UNCOMPRESSED_BYTES as u64 {
            return Err("JAR entry uncompressed bytes exceed cap".into());
        }
        let payload = index
            .read_entry(&entry.name, MAX_JAR_ENTRY_UNCOMPRESSED_BYTES)
            .map_err(|e| format!("decode JAR entry {}: {e}", entry.name))?;
        if payload.len() as u64 != entry.uncompressed_size {
            return Err(format!("JAR entry {} length mismatch", entry.name));
        }
        if crc32_ieee(&payload) != entry.crc32 {
            return Err(format!("JAR entry {} CRC mismatch", entry.name));
        }
    }
    Ok(JarArchiveReport {
        entries: index.entries().len(),
        uncompressed_bytes: total_uncompressed,
    })
}

fn reject_unsafe_zip_central_names(path: &Path) -> Result<(), String> {
    let bytes =
        fs::read(path).map_err(|e| format!("read archive for central-directory scan: {e}"))?;
    let Some((cd_offset, cd_size)) = central_directory_location(&bytes) else {
        return Ok(());
    };
    let end = cd_offset
        .checked_add(cd_size)
        .filter(|end| *end <= bytes.len())
        .ok_or("ZIP central directory outside archive")?;
    let mut cursor = cd_offset;
    while cursor + 46 <= end {
        if u32_le(&bytes, cursor) != 0x0201_4b50 {
            break;
        }
        let name_len = u16_le(&bytes, cursor + 28) as usize;
        let extra_len = u16_le(&bytes, cursor + 30) as usize;
        let comment_len = u16_le(&bytes, cursor + 32) as usize;
        let name_start = cursor + 46;
        let name_end = name_start
            .checked_add(name_len)
            .filter(|value| *value <= end)
            .ok_or("ZIP entry name overruns central directory")?;
        if let Ok(name) = std::str::from_utf8(&bytes[name_start..name_end]) {
            validate_zip_member_or_directory_name(name)?;
        }
        cursor = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .filter(|value| *value <= end)
            .ok_or("ZIP central directory record overrun")?;
    }
    Ok(())
}

fn central_directory_location(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() < 22 {
        return None;
    }
    let start = bytes.len().saturating_sub(65_557);
    for eocd in (start..=bytes.len() - 22).rev() {
        if u32_le(bytes, eocd) != 0x0605_4b50 {
            continue;
        }
        let comment_len = u16_le(bytes, eocd + 20) as usize;
        if eocd + 22 + comment_len != bytes.len() {
            continue;
        }
        let count = u16_le(bytes, eocd + 10) as u64;
        let mut size = u32_le(bytes, eocd + 12) as u64;
        let mut offset = u32_le(bytes, eocd + 16) as u64;
        if count == u64::from(u16::MAX)
            || size == u64::from(u32::MAX)
            || offset == u64::from(u32::MAX)
        {
            if eocd < 20 || u32_le(bytes, eocd - 20) != 0x0706_4b50 {
                return None;
            }
            let z64 = u64::from(u32_le(bytes, eocd - 12)) as usize;
            if z64 + 56 > bytes.len() || u32_le(bytes, z64) != 0x0606_4b50 {
                return None;
            }
            size = u64_le(bytes, z64 + 40);
            offset = u64_le(bytes, z64 + 48);
        }
        return Some((usize::try_from(offset).ok()?, usize::try_from(size).ok()?));
    }
    None
}

fn validate_zip_member_or_directory_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty()
        || name.starts_with('/')
        || name.bytes().any(|b| b == b'\\' || b == b':' || b == 0)
        || trimmed
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(format!("ZIP archive contains unsafe member name {name:?}"));
    }
    Ok(())
}

fn u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn hash_file_hex(path: &Path) -> Result<(String, String), String> {
    let mut file = File::open(path).map_err(|e| format!("open staged mod: {e}"))?;
    let mut sha512 = Sha512::new();
    let mut sha256 = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|e| format!("read staged mod: {e}"))?;
        if read == 0 {
            break;
        }
        sha512.update(&chunk[..read]);
        sha256.update(&chunk[..read]);
    }
    Ok((
        hex_encode(&sha512.finalize()),
        hex_encode(&sha256.finalize()),
    ))
}

pub fn hash_bytes_sha512_hex(bytes: &[u8]) -> String {
    let mut hash = Sha512::new();
    hash.update(bytes);
    hex_encode(&hash.finalize())
}

fn diff_manifests(
    from: Option<&GenerationManifest>,
    to: &GenerationManifest,
) -> ProfileActivationDiff {
    let old = from.map(mod_map).unwrap_or_default();
    let new = mod_map(to);
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut updated = Vec::new();
    let mut auto = BTreeMap::new();
    for (mod_id, next) in &new {
        match old.get(mod_id) {
            None => {
                added.push(summary(next));
                if next.source_reason == SOURCE_AUTOFETCH_DEPS {
                    auto.insert(mod_id.clone(), summary(next));
                }
            }
            Some(previous)
                if previous.version != next.version
                    || previous.sha512 != next.sha512
                    || previous.filename != next.filename =>
            {
                updated.push(ModVersionChange {
                    mod_id: mod_id.clone(),
                    from_version: previous.version.clone(),
                    to_version: next.version.clone(),
                    from_filename: previous.filename.clone(),
                    to_filename: next.filename.clone(),
                });
                if next.source_reason == SOURCE_AUTOFETCH_DEPS
                    || previous.source_reason == SOURCE_AUTOFETCH_DEPS
                {
                    auto.insert(mod_id.clone(), summary(next));
                }
            }
            _ => {}
        }
    }
    for (mod_id, previous) in &old {
        if !new.contains_key(mod_id) {
            removed.push(summary(previous));
            if previous.source_reason == SOURCE_AUTOFETCH_DEPS {
                auto.insert(mod_id.clone(), summary(previous));
            }
        }
    }
    ProfileActivationDiff {
        from_generation_id: from.map(|manifest| manifest.generation_id.clone()),
        to_generation_id: to.generation_id.clone(),
        added_mods: added,
        removed_mods: removed,
        updated_mods: updated,
        auto_fetched_deps: auto.into_values().collect(),
    }
}

fn mod_map(manifest: &GenerationManifest) -> BTreeMap<String, GenerationModEntry> {
    manifest
        .mods
        .iter()
        .cloned()
        .map(|entry| (entry.mod_id.clone(), entry))
        .collect()
}

fn summary(entry: &GenerationModEntry) -> ProfileModSummary {
    ProfileModSummary {
        mod_id: entry.mod_id.clone(),
        version: entry.version.clone(),
        filename: entry.filename.clone(),
        sha512: entry.sha512.clone(),
        source_reason: entry.source_reason.clone(),
    }
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "atomic write target has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| format!("create atomic-write parent: {e}"))?;
    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{}.tmp-{}-{counter:x}",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("json"),
            std::process::id()
        ));
        reject_symlink_path(&temp)?;
        let created = OpenOptions::new().write(true).create_new(true).open(&temp);
        let mut file = match created {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create atomic temp file: {e}")),
        };
        let result = (|| {
            file.write_all(bytes)
                .map_err(|e| format!("write atomic temp file: {e}"))?;
            file.sync_all()
                .map_err(|e| format!("sync atomic temp file: {e}"))?;
            drop(file);
            fs::rename(&temp, destination).map_err(|e| format!("publish atomic file: {e}"))?;
            fsync_dir(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }
    Err("could not allocate a unique atomic temp file".into())
}

pub(crate) fn fsync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| format!("sync directory {}: {e}", path.display()))
}

pub(crate) fn reject_symlink_path(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("refusing symlink path {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect path {}: {e}", path.display())),
    }
}

fn unix_millis() -> Result<u64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock before Unix epoch".to_owned())?;
    Ok(duration.as_millis() as u64)
}

fn validate_hex(value: &str, len: usize, label: &str) -> Result<(), String> {
    if value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("invalid {label}"))
    }
}

/// Minimal streaming implementation of FIPS 180-4 SHA-512.
#[derive(Clone)]
pub(crate) struct Sha512 {
    state: [u64; 8],
    block: [u8; 128],
    used: usize,
    bytes: u128,
}

impl Sha512 {
    pub(crate) fn new() -> Self {
        Self {
            state: [
                0x6a09e667f3bcc908,
                0xbb67ae8584caa73b,
                0x3c6ef372fe94f82b,
                0xa54ff53a5f1d36f1,
                0x510e527fade682d1,
                0x9b05688c2b3e6c1f,
                0x1f83d9abfb41bd6b,
                0x5be0cd19137e2179,
            ],
            block: [0; 128],
            used: 0,
            bytes: 0,
        }
    }

    pub(crate) fn update(&mut self, mut input: &[u8]) {
        self.bytes += input.len() as u128;
        if self.used > 0 {
            let n = (128 - self.used).min(input.len());
            self.block[self.used..self.used + n].copy_from_slice(&input[..n]);
            self.used += n;
            input = &input[n..];
            if self.used == 128 {
                let block = self.block;
                self.compress(&block);
                self.used = 0;
            } else {
                return;
            }
        }
        while input.len() >= 128 {
            self.compress(&input[..128]);
            input = &input[128..];
        }
        self.block[..input.len()].copy_from_slice(input);
        self.used = input.len();
    }

    pub(crate) fn finalize(mut self) -> [u8; 64] {
        let bits = self.bytes * 8;
        self.block[self.used] = 0x80;
        self.used += 1;
        if self.used > 112 {
            self.block[self.used..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; 128];
            self.used = 0;
        } else {
            self.block[self.used..112].fill(0);
        }
        self.block[112..].copy_from_slice(&bits.to_be_bytes());
        let block = self.block;
        self.compress(&block);
        let mut out = [0; 64];
        for (index, word) in self.state.iter().enumerate() {
            let start = index * 8;
            out[start..start + 8].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8]) {
        let mut w = [0u64; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u64::from_be_bytes(block[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K512[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *x = x.wrapping_add(y);
        }
    }
}

const K512: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];
