// SPDX-License-Identifier: LGPL-3.0-or-later
//! Consent-driven, hash-verified Modrinth dependency downloads.

use crate::mod_profile_tx::{
    fsync_dir, reject_symlink_path, verify_jar_archive_safety, GenerationModEntry, ModProfileStore,
    ProfileActivationDiff, Sha512, SOURCE_AUTOFETCH_DEPS, SOURCE_SERVER_SYNC,
};
use megu_core::crypto::{hex_encode, Sha256};
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_core::zip_archive::LazyZipIndex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MOD_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_BATCH_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_MODS: usize = 64;
pub const MAX_DEPENDENCY_DEPTH: usize = 16;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const DEFAULT_MODRINTH_API: &str = "https://api.modrinth.com";
static DOWNLOAD_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModRequirement {
    pub mod_id: String,
    pub required_version: String,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub modrinth_slug: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModLoader {
    NeoForge,
    Fabric,
    Quilt,
}

impl ModLoader {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeoForge => "neoforge",
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModSide {
    Client,
    Server,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactModRequirement {
    pub mod_id: String,
    pub required_version: String,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub modrinth_slug: Option<String>,
    pub project_id: Option<String>,
    pub version_id: Option<String>,
    pub game_version: String,
    pub loader: ModLoader,
    pub side: ModSide,
    pub requested_by: Vec<String>,
}

impl ExactModRequirement {
    pub fn dependency(
        mod_id: impl Into<String>,
        required_version: impl Into<String>,
        game_version: impl Into<String>,
        loader: ModLoader,
        side: ModSide,
    ) -> Self {
        Self {
            mod_id: mod_id.into(),
            required_version: required_version.into(),
            sha512: None,
            sha256: None,
            modrinth_slug: None,
            project_id: None,
            version_id: None,
            game_version: game_version.into(),
            loader,
            side,
            requested_by: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModMismatchKind {
    Missing,
    VersionMismatch { installed: String, required: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModMismatchEntry {
    pub mod_id: String,
    pub kind: ModMismatchKind,
}

pub fn compute_missing_mods(
    required: &[ModRequirement],
    installed_mod_ids: &BTreeMap<String, String>,
) -> Vec<ModMismatchEntry> {
    required
        .iter()
        .filter_map(|req| match installed_mod_ids.get(&req.mod_id) {
            None => Some(ModMismatchEntry {
                mod_id: req.mod_id.clone(),
                kind: ModMismatchKind::Missing,
            }),
            Some(installed) if installed != &req.required_version => Some(ModMismatchEntry {
                mod_id: req.mod_id.clone(),
                kind: ModMismatchKind::VersionMismatch {
                    installed: installed.clone(),
                    required: req.required_version.clone(),
                },
            }),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModSyncProgress {
    Resolving,
    Downloading {
        mod_id: String,
        file_index: usize,
        total_files: usize,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    Verifying {
        mod_id: String,
    },
    Ready {
        mods_dir: PathBuf,
        downloaded_files: usize,
    },
    Failed(String),
}

#[derive(Clone)]
pub struct ModSyncTask {
    progress: Arc<Mutex<ModSyncProgress>>,
}

impl ModSyncTask {
    pub fn progress(&self) -> ModSyncProgress {
        lock(&self.progress).clone()
    }
}

pub struct ModSyncWorker {
    pub task: ModSyncTask,
    thread: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
}

impl ModSyncWorker {
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Start work only after the UI has received explicit user confirmation.
pub fn spawn_modrinth_mod_sync(
    requirements: Vec<ModRequirement>,
    game_dir: PathBuf,
    server_key: String,
) -> ModSyncWorker {
    let progress = Arc::new(Mutex::new(ModSyncProgress::Resolving));
    let shared = Arc::clone(&progress);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let thread = thread::spawn(move || {
        if std::env::var_os("MEGU_OFFLINE").is_some()
            || std::env::var_os("MEGU_NO_AUTO_FETCH").is_some()
        {
            set_progress(
                &shared,
                ModSyncProgress::Failed("Mod auto-fetch is disabled by environment".into()),
            );
            return;
        }
        let result = sync_mods(
            &requirements,
            &game_dir,
            &server_key,
            &shared,
            &worker_cancel,
        );
        if let Err(error) = result {
            set_progress(&shared, ModSyncProgress::Failed(error));
        }
    });
    ModSyncWorker {
        task: ModSyncTask { progress },
        thread,
        cancel,
    }
}

fn sync_mods(
    reqs: &[ModRequirement],
    game_dir: &Path,
    server_key: &str,
    progress: &Arc<Mutex<ModSyncProgress>>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    sync_mods_at(
        reqs,
        game_dir,
        server_key,
        progress,
        DEFAULT_MODRINTH_API,
        cancel,
    )
}

fn sync_mods_at(
    reqs: &[ModRequirement],
    game_dir: &Path,
    server_key: &str,
    progress: &Arc<Mutex<ModSyncProgress>>,
    api_base: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if reqs.iter().any(|req| req.sha512.is_none()) {
        return Err(
            "legacy mod sync requirements need content SHA-512; use exact loader/game-version sync for unhashed requirements"
                .into(),
        );
    }
    let key = megu_core::crypto::sha256_hex(server_key.as_bytes());
    let profile_root = game_dir.join("server-mods").join(&key[..16]);
    let diff = sync_content_addressed_mods_at(reqs, &profile_root, progress, api_base, cancel)?;
    let store = ModProfileStore::open(&profile_root)?;
    let mods_dir = store
        .active_generation_dir()?
        .ok_or("transactional activation completed without an active generation")?;
    set_progress(
        progress,
        ModSyncProgress::Ready {
            mods_dir,
            downloaded_files: diff.added_mods.len() + diff.updated_mods.len(),
        },
    );
    Ok(())
}

pub fn sync_exact_mods(
    reqs: &[ExactModRequirement],
    profile_root: &Path,
    progress: &Arc<Mutex<ModSyncProgress>>,
    cancel: &AtomicBool,
) -> Result<ProfileActivationDiff, String> {
    sync_exact_mods_at(reqs, profile_root, progress, DEFAULT_MODRINTH_API, cancel)
}

pub fn sync_exact_mods_at(
    reqs: &[ExactModRequirement],
    profile_root: &Path,
    progress: &Arc<Mutex<ModSyncProgress>>,
    api_base: &str,
    cancel: &AtomicBool,
) -> Result<ProfileActivationDiff, String> {
    if reqs.is_empty() {
        return Err("exact mod sync requires at least one mod".into());
    }
    if reqs.len() > MAX_MODS {
        return Err(format!("mod count exceeds {MAX_MODS}"));
    }
    let agent = modrinth_agent();
    let resolved = resolve_required_closure(&agent, api_base, reqs)?;
    download_resolved_batch(
        &agent,
        resolved,
        profile_root,
        progress,
        cancel,
        SOURCE_SERVER_SYNC,
    )
}

fn sync_content_addressed_mods_at(
    reqs: &[ModRequirement],
    profile_root: &Path,
    progress: &Arc<Mutex<ModSyncProgress>>,
    api_base: &str,
    cancel: &AtomicBool,
) -> Result<ProfileActivationDiff, String> {
    if reqs.len() > MAX_MODS {
        return Err(format!("mod count exceeds {MAX_MODS}"));
    }
    let agent = modrinth_agent();
    let mut exact = Vec::new();
    for req in reqs {
        let Some(hash) = req.sha512.clone() else {
            return Err("content-addressed legacy sync requires SHA-512".into());
        };
        validate_hex(&hash, 128, "required SHA-512")?;
        exact.push(ExactModRequirement {
            mod_id: req.mod_id.clone(),
            required_version: req.required_version.clone(),
            sha512: Some(hash),
            sha256: req.sha256.clone(),
            modrinth_slug: req.modrinth_slug.clone(),
            project_id: None,
            version_id: None,
            game_version: "content-addressed".into(),
            loader: ModLoader::NeoForge,
            side: ModSide::Client,
            requested_by: vec![req.mod_id.clone()],
        });
    }
    let mut resolved = Vec::new();
    for req in &exact {
        resolved.push(ResolvedDownload {
            version: resolve_content_addressed(&agent, api_base, req)?,
            expected_mod_id: Some(req.mod_id.clone()),
            requested_by: req.requested_by.clone(),
            source_reason: SOURCE_SERVER_SYNC.to_owned(),
        });
    }
    resolved = resolve_exact_version_dependencies(&agent, api_base, resolved, &BTreeMap::new())?;
    download_resolved_batch(
        &agent,
        resolved,
        profile_root,
        progress,
        cancel,
        SOURCE_SERVER_SYNC,
    )
}

#[derive(Clone, Debug)]
struct ResolvedFile {
    url: String,
    filename: String,
    sha512: String,
    sha256: Option<String>,
    size: u64,
}

#[derive(Clone, Debug)]
struct ModrinthDependency {
    project_id: Option<String>,
    version_id: Option<String>,
    dependency_type: String,
}

#[derive(Clone, Debug)]
struct ResolvedVersion {
    project_id: String,
    version_id: String,
    version_number: String,
    file: ResolvedFile,
    dependencies: Vec<ModrinthDependency>,
    target_game_version: Option<String>,
    target_loader: Option<ModLoader>,
    target_side: Option<ModSide>,
}

#[derive(Clone, Debug)]
struct ResolvedDownload {
    version: ResolvedVersion,
    expected_mod_id: Option<String>,
    requested_by: Vec<String>,
    source_reason: String,
}

fn modrinth_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(15)))
        .max_redirects(0)
        .build()
        .into()
}

fn resolve_required_closure(
    agent: &ureq::Agent,
    api_base: &str,
    reqs: &[ExactModRequirement],
) -> Result<Vec<ResolvedDownload>, String> {
    let mut project_cache = BTreeMap::new();
    let mut out = Vec::new();
    for req in reqs {
        validate_exact_requirement(req)?;
        out.push(ResolvedDownload {
            version: resolve_exact_requirement(agent, api_base, req, &mut project_cache)?,
            expected_mod_id: Some(req.mod_id.clone()),
            requested_by: if req.requested_by.is_empty() {
                vec![req.mod_id.clone()]
            } else {
                req.requested_by.clone()
            },
            source_reason: SOURCE_SERVER_SYNC.to_owned(),
        });
    }
    resolve_exact_version_dependencies(agent, api_base, out, &project_cache)
}

fn resolve_exact_version_dependencies(
    agent: &ureq::Agent,
    api_base: &str,
    initial: Vec<ResolvedDownload>,
    project_cache: &BTreeMap<String, JsonValue>,
) -> Result<Vec<ResolvedDownload>, String> {
    let mut cache = project_cache.clone();
    let mut out = initial;
    let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
    let mut project_versions = BTreeMap::<String, String>::new();
    for (index, item) in out.iter().enumerate() {
        if let Some(existing) = project_versions.get(&item.version.project_id) {
            if existing != &item.version.version_id {
                return Err(format!(
                    "conflicting exact versions required for Modrinth project {}",
                    item.version.project_id
                ));
            }
            return Err(format!(
                "duplicate exact requirement for Modrinth project {}",
                item.version.project_id
            ));
        }
        project_versions.insert(
            item.version.project_id.clone(),
            item.version.version_id.clone(),
        );
        queue.push_back((index, 0));
    }
    let mut expanded = BTreeSet::new();
    while let Some((index, depth)) = queue.pop_front() {
        if depth >= MAX_DEPENDENCY_DEPTH {
            return Err(format!("dependency depth exceeds {MAX_DEPENDENCY_DEPTH}"));
        }
        if !expanded.insert(index) {
            continue;
        }
        let parent = out[index].clone();
        for dep in &parent.version.dependencies {
            if dep.dependency_type != "required" {
                continue;
            }
            if dep.project_id.is_none() && dep.version_id.is_none() {
                return Err(format!(
                    "required dependency of {} lacks project_id and version_id",
                    parent.version.project_id
                ));
            }
            if let Some(project_id) = &dep.project_id {
                if let Some(existing) = project_versions.get(project_id) {
                    if dep.version_id.as_ref().is_some_and(|id| id != existing) {
                        return Err(format!(
                            "conflicting exact versions required for Modrinth project {project_id}"
                        ));
                    }
                    continue;
                }
            }
            if out.len() >= MAX_MODS {
                return Err(format!("dependency closure exceeds {MAX_MODS} mods"));
            }
            let target = infer_target_from_parent(&parent.version)?;
            let dependency_key = dep
                .project_id
                .clone()
                .or_else(|| dep.version_id.clone())
                .ok_or("required dependency lacks a resolvable key")?;
            let req = ExactModRequirement {
                mod_id: dependency_key.clone(),
                required_version: "*".into(),
                sha512: None,
                sha256: None,
                modrinth_slug: None,
                project_id: dep.project_id.clone(),
                version_id: dep.version_id.clone(),
                game_version: target.0,
                loader: target.1,
                side: target.2,
                requested_by: vec![parent.version.project_id.clone()],
            };
            let version = resolve_exact_requirement(agent, api_base, &req, &mut cache)?;
            if let Some(existing) = project_versions.get(&version.project_id) {
                if existing != &version.version_id {
                    return Err(format!(
                        "conflicting exact versions required for Modrinth project {}",
                        version.project_id
                    ));
                }
                continue;
            }
            project_versions.insert(version.project_id.clone(), version.version_id.clone());
            out.push(ResolvedDownload {
                version,
                expected_mod_id: None,
                requested_by: vec![parent.version.project_id.clone()],
                source_reason: SOURCE_AUTOFETCH_DEPS.to_owned(),
            });
            queue.push_back((out.len() - 1, depth + 1));
        }
    }
    Ok(out)
}

fn infer_target_from_parent(
    version: &ResolvedVersion,
) -> Result<(String, ModLoader, ModSide), String> {
    let game = version
        .target_game_version
        .clone()
        .ok_or("cannot infer dependency target game version")?;
    Ok((
        game,
        version
            .target_loader
            .ok_or("cannot infer dependency loader")?,
        version.target_side.ok_or("cannot infer dependency side")?,
    ))
}

fn validate_exact_requirement(req: &ExactModRequirement) -> Result<(), String> {
    if req.mod_id.is_empty() || req.mod_id.len() > 128 {
        return Err("empty or oversized mod id".into());
    }
    if req.required_version.is_empty() || req.required_version.len() > 256 {
        return Err("empty or oversized required version/range".into());
    }
    if req.game_version.is_empty() || req.game_version.len() > 64 {
        return Err("empty or oversized target game version".into());
    }
    if let Some(hash) = &req.sha512 {
        validate_hex(hash, 128, "required SHA-512")?;
    }
    if let Some(hash) = &req.sha256 {
        validate_hex(hash, 64, "required SHA-256")?;
    }
    if let Some(project_id) = &req.project_id {
        validate_modrinth_id(project_id, "project id")?;
    }
    if let Some(version_id) = &req.version_id {
        validate_modrinth_id(version_id, "version id")?;
    }
    if req.project_id.is_none() && req.version_id.is_none() && req.sha512.is_none() {
        let slug = req.modrinth_slug.as_deref().unwrap_or(&req.mod_id);
        if !valid_slug(slug) {
            return Err("invalid Modrinth project slug".into());
        }
    }
    Ok(())
}

fn resolve_exact_requirement(
    agent: &ureq::Agent,
    api_base: &str,
    req: &ExactModRequirement,
    project_cache: &mut BTreeMap<String, JsonValue>,
) -> Result<ResolvedVersion, String> {
    validate_exact_requirement(req)?;
    let version = if req.sha512.is_some() {
        let hash = req
            .sha512
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let url = format!("{api_base}/v2/version_file/{hash}?algorithm=sha512");
        fetch_json(agent, &url)?
    } else if let Some(version_id) = &req.version_id {
        fetch_json(agent, &format!("{api_base}/v2/version/{version_id}"))?
    } else {
        let project_key = req
            .project_id
            .clone()
            .or_else(|| req.modrinth_slug.clone())
            .unwrap_or_else(|| req.mod_id.clone());
        let encoded_project = percent_encode(&project_key);
        let game = percent_encode(&format!("[\"{}\"]", req.game_version));
        let loader = percent_encode(&format!("[\"{}\"]", req.loader.as_str()));
        let url = format!(
            "{api_base}/v2/project/{encoded_project}/version?game_versions={game}&loaders={loader}"
        );
        let versions = fetch_json(agent, &url)?;
        let versions = versions
            .as_array()
            .ok_or("Modrinth version response is not an array")?;
        select_version_from_list(versions, req)?
    };
    resolved_version_from_json(&version, req, agent, api_base, project_cache)
}

fn resolve_content_addressed(
    agent: &ureq::Agent,
    api_base: &str,
    req: &ExactModRequirement,
) -> Result<ResolvedVersion, String> {
    let hash = req
        .sha512
        .as_deref()
        .ok_or("content-addressed resolution requires SHA-512")?
        .to_ascii_lowercase();
    let url = format!("{api_base}/v2/version_file/{hash}?algorithm=sha512");
    let json = fetch_json(agent, &url)?;
    let file = select_file(&json, req)?;
    let version_number = json
        .get("version_number")
        .and_then(JsonValue::as_str)
        .unwrap_or(&req.required_version)
        .to_owned();
    if !version_requirement_matches(&version_number, &req.required_version) {
        return Err(
            "content-addressed Modrinth version does not satisfy the exact requirement".into(),
        );
    }
    Ok(ResolvedVersion {
        project_id: json
            .get("project_id")
            .and_then(JsonValue::as_str)
            .unwrap_or(&req.mod_id)
            .to_owned(),
        version_id: json
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or("content-addressed")
            .to_owned(),
        version_number,
        file,
        dependencies: parse_dependencies(&json),
        target_game_version: None,
        target_loader: None,
        target_side: None,
    })
}

fn select_version_from_list(
    versions: &[JsonValue],
    req: &ExactModRequirement,
) -> Result<JsonValue, String> {
    let mut candidates = Vec::new();
    for version in versions {
        if version.get("status").and_then(JsonValue::as_str) != Some("listed") {
            continue;
        }
        if let Some(project_id) = &req.project_id {
            if version.get("project_id").and_then(JsonValue::as_str) != Some(project_id) {
                continue;
            }
        }
        if let Some(version_id) = &req.version_id {
            if version.get("id").and_then(JsonValue::as_str) != Some(version_id) {
                continue;
            }
        }
        if !json_string_array_contains(version.get("game_versions"), &req.game_version) {
            continue;
        }
        if !json_string_array_contains(version.get("loaders"), req.loader.as_str()) {
            continue;
        }
        let number = version
            .get("version_number")
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        if !version_requirement_matches(number, &req.required_version) {
            continue;
        }
        candidates.push(version.clone());
    }
    if candidates.is_empty() {
        return Err(format!(
            "no exact Modrinth release for {} version/range {} on {} ({})",
            req.mod_id,
            req.required_version,
            req.game_version,
            req.loader.as_str()
        ));
    }
    candidates.sort_by(|a, b| {
        version_type_rank(
            a.get("version_type")
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
        )
        .cmp(&version_type_rank(
            b.get("version_type")
                .and_then(JsonValue::as_str)
                .unwrap_or(""),
        ))
        .then_with(|| {
            version_key(
                b.get("version_number")
                    .and_then(JsonValue::as_str)
                    .unwrap_or(""),
            )
            .cmp(&version_key(
                a.get("version_number")
                    .and_then(JsonValue::as_str)
                    .unwrap_or(""),
            ))
        })
        .then_with(|| {
            a.get("id")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .cmp(b.get("id").and_then(JsonValue::as_str).unwrap_or(""))
        })
    });
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no exact Modrinth release survived deterministic ordering".to_owned())
}

fn resolved_version_from_json(
    version: &JsonValue,
    req: &ExactModRequirement,
    agent: &ureq::Agent,
    api_base: &str,
    project_cache: &mut BTreeMap<String, JsonValue>,
) -> Result<ResolvedVersion, String> {
    let project_id = version
        .get("project_id")
        .and_then(JsonValue::as_str)
        .ok_or("Modrinth version lacks project_id")?
        .to_owned();
    if let Some(expected) = &req.project_id {
        if &project_id != expected {
            return Err("Modrinth project_id differs from exact requirement".into());
        }
    }
    let version_id = version
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or("Modrinth version lacks id")?
        .to_owned();
    if let Some(expected) = &req.version_id {
        if &version_id != expected {
            return Err("Modrinth version_id differs from exact requirement".into());
        }
    }
    if version
        .get("status")
        .and_then(JsonValue::as_str)
        .is_some_and(|s| s != "listed")
    {
        return Err("Modrinth version is not listed".into());
    }
    if !json_string_array_contains(version.get("game_versions"), &req.game_version) {
        return Err("Modrinth version does not support the exact game version".into());
    }
    if !json_string_array_contains(version.get("loaders"), req.loader.as_str()) {
        return Err("Modrinth version does not support the exact loader".into());
    }
    validate_project_side(agent, api_base, project_cache, &project_id, req.side)?;
    let version_number = version
        .get("version_number")
        .and_then(JsonValue::as_str)
        .ok_or("Modrinth version lacks version_number")?
        .to_owned();
    if !version_requirement_matches(&version_number, &req.required_version) {
        return Err("Modrinth version_number does not satisfy the exact requirement".into());
    }
    let file = select_file(version, req)?;
    Ok(ResolvedVersion {
        project_id,
        version_id,
        version_number,
        file,
        dependencies: parse_dependencies(version),
        target_game_version: Some(req.game_version.clone()),
        target_loader: Some(req.loader),
        target_side: Some(req.side),
    })
}

fn validate_project_side(
    agent: &ureq::Agent,
    api_base: &str,
    project_cache: &mut BTreeMap<String, JsonValue>,
    project_id: &str,
    side: ModSide,
) -> Result<(), String> {
    if !project_cache.contains_key(project_id) {
        let json = fetch_json(agent, &format!("{api_base}/v2/project/{project_id}"))?;
        project_cache.insert(project_id.to_owned(), json);
    }
    let project = project_cache
        .get(project_id)
        .ok_or("Modrinth project metadata unavailable")?;
    let field = match side {
        ModSide::Client => "client_side",
        ModSide::Server => "server_side",
    };
    match project.get(field).and_then(JsonValue::as_str) {
        Some("required" | "optional") => Ok(()),
        Some("unsupported") => Err(format!(
            "Modrinth project {project_id} is unsupported on this side"
        )),
        _ => Err(format!(
            "Modrinth project {project_id} lacks exact {field} support metadata"
        )),
    }
}

fn parse_dependencies(version: &JsonValue) -> Vec<ModrinthDependency> {
    version
        .get("dependencies")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| ModrinthDependency {
                    project_id: item
                        .get("project_id")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned),
                    version_id: item
                        .get("version_id")
                        .and_then(JsonValue::as_str)
                        .map(str::to_owned),
                    dependency_type: item
                        .get("dependency_type")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn select_file(version: &JsonValue, req: &ExactModRequirement) -> Result<ResolvedFile, String> {
    let files = version
        .get("files")
        .and_then(JsonValue::as_array)
        .ok_or("Modrinth version has no files")?;
    let mut exact_digest_matches = Vec::new();
    let mut primary_candidates = Vec::new();
    for file in files {
        let Ok(candidate) = file_candidate(file) else {
            continue;
        };
        if let Some(expected) = &req.sha512 {
            if !candidate.sha512.eq_ignore_ascii_case(expected) {
                continue;
            }
        }
        if let Some(expected) = &req.sha256 {
            if candidate
                .sha256
                .as_deref()
                .is_none_or(|actual| !actual.eq_ignore_ascii_case(expected))
            {
                continue;
            }
        }
        if req.sha512.is_some() || req.sha256.is_some() {
            exact_digest_matches.push(candidate);
        } else if file.get("primary").and_then(JsonValue::as_bool) == Some(true) {
            primary_candidates.push(candidate);
        }
    }
    let pool = if req.sha512.is_some() || req.sha256.is_some() {
        exact_digest_matches
    } else {
        primary_candidates
    };
    if pool.is_empty() {
        return Err(if req.sha512.is_some() || req.sha256.is_some() {
            "no Modrinth file exactly matches the required digest(s)".into()
        } else {
            "Modrinth version has no valid primary file".into()
        });
    }
    let mut pool = pool;
    pool.sort_by(|a, b| a.filename.cmp(&b.filename).then_with(|| a.url.cmp(&b.url)));
    pool.into_iter()
        .next()
        .ok_or_else(|| "no exact Modrinth file selected".to_owned())
}

fn file_candidate(file: &JsonValue) -> Result<ResolvedFile, String> {
    let url = field_str(file, "url")?.to_owned();
    if !host_allowed(&url) {
        return Err("refusing download URL outside Modrinth CDN".into());
    }
    let filename = field_str(file, "filename")?.to_owned();
    sanitize_filename(&filename)?;
    let hashes = file.get("hashes").ok_or("Modrinth file has no hashes")?;
    let sha512 = hashes
        .get("sha512")
        .and_then(JsonValue::as_str)
        .ok_or("Modrinth file has no SHA-512")?
        .to_ascii_lowercase();
    validate_hex(&sha512, 128, "Modrinth SHA-512")?;
    let sha256 = hashes
        .get("sha256")
        .and_then(JsonValue::as_str)
        .map(str::to_ascii_lowercase);
    if let Some(hash) = &sha256 {
        validate_hex(hash, 64, "Modrinth SHA-256")?;
    }
    let size = field_num(file, "size")?;
    if size == 0 || size > MAX_MOD_BYTES {
        return Err("Modrinth file size outside allowed bounds".into());
    }
    Ok(ResolvedFile {
        url,
        filename,
        sha512,
        sha256,
        size,
    })
}

fn download_resolved_batch(
    agent: &ureq::Agent,
    resolved: Vec<ResolvedDownload>,
    profile_root: &Path,
    progress: &Arc<Mutex<ModSyncProgress>>,
    cancel: &AtomicBool,
    generation_source: &str,
) -> Result<ProfileActivationDiff, String> {
    let store = ModProfileStore::open(profile_root)?;
    let mut staged = store.stage_generation(generation_source)?;
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut filenames = BTreeSet::new();
    let mut inspected = Vec::new();
    for item in &resolved {
        total = total
            .checked_add(item.version.file.size)
            .ok_or("batch size overflow")?;
        if total > MAX_BATCH_BYTES {
            return Err(format!("batch exceeds {MAX_BATCH_BYTES} bytes"));
        }
        if !filenames.insert(item.version.file.filename.clone()) {
            return Err(format!(
                "two resolved mods use the same filename {}",
                item.version.file.filename
            ));
        }
    }
    for (index, item) in resolved.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Modrinth sync cancelled".into());
        }
        set_progress(
            progress,
            ModSyncProgress::Downloading {
                mod_id: item.version.project_id.clone(),
                file_index: index + 1,
                total_files: resolved.len(),
                downloaded_bytes: 0,
                total_bytes: Some(item.version.file.size),
            },
        );
        let jar = download_to_generation(
            agent,
            item,
            &staged.generation_dir,
            progress,
            index + 1,
            resolved.len(),
            cancel,
        )?;
        set_progress(
            progress,
            ModSyncProgress::Verifying {
                mod_id: item.version.project_id.clone(),
            },
        );
        verify_jar_archive_safety(&jar)?;
        let inspection = inspect_mod_jar(&jar)?;
        validate_inspected_jar(&inspection, item)?;
        let inspected_mod_id = inspection.mod_id.clone();
        inspected.push((item.clone(), inspection));
        entries.push(GenerationModEntry {
            mod_id: inspected_mod_id,
            version: item.version.version_number.clone(),
            filename: item.version.file.filename.clone(),
            sha512: item.version.file.sha512.clone(),
            sha256: item.version.file.sha256.clone(),
            source_reason: item.source_reason.clone(),
            modrinth_project_id: Some(item.version.project_id.clone()),
            modrinth_version_id: Some(item.version.version_id.clone()),
            requested_by: item.requested_by.clone(),
        });
    }
    let replacement_ids: BTreeSet<String> = inspected
        .iter()
        .map(|(_, inspection)| inspection.mod_id.clone())
        .collect();
    let mut complete_entries =
        copy_unaffected_active_entries(&store, &staged.generation_dir, &replacement_ids)?;
    let copied_count = complete_entries.len();
    complete_entries.append(&mut entries);
    let mut all_inspections = Vec::new();
    for entry in &complete_entries[..copied_count] {
        all_inspections.push(inspect_mod_jar(
            &staged.generation_dir.join(&entry.filename),
        )?);
    }
    all_inspections.extend(inspected.iter().map(|(_, inspection)| inspection.clone()));
    validate_dependency_manifest_closure(&all_inspections, &complete_entries)?;
    store.publish_staged_generation(&mut staged, generation_source, complete_entries)
}

fn copy_unaffected_active_entries(
    store: &ModProfileStore,
    staged_dir: &Path,
    replacement_ids: &BTreeSet<String>,
) -> Result<Vec<GenerationModEntry>, String> {
    let Some(active_id) = store.active_generation_id()? else {
        return Ok(Vec::new());
    };
    let active = store.manifest(&active_id)?;
    let active_dir = store.generations_root().join(&active_id);
    let mut out = Vec::new();
    for entry in active.mods {
        if replacement_ids.contains(&entry.mod_id) {
            continue;
        }
        let source = active_dir.join(&entry.filename);
        let destination = staged_dir.join(&entry.filename);
        reject_symlink_path(&source)?;
        reject_symlink_path(&destination)?;
        if destination.exists() {
            return Err("active profile filename collision in staged generation".into());
        }
        copy_file_exclusive(&source, &destination)?;
        out.push(entry);
    }
    Ok(out)
}

fn copy_file_exclusive(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "copy destination has no parent".to_owned())?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("copy destination is not UTF-8")?;
    let mut input = OpenOptions::new()
        .read(true)
        .open(source)
        .map_err(|e| format!("open active mod for copy: {e}"))?;
    for _ in 0..128 {
        let counter = DOWNLOAD_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{file_name}.copy-{}-{counter:x}",
            std::process::id()
        ));
        reject_symlink_path(&temp)?;
        let created = OpenOptions::new().write(true).create_new(true).open(&temp);
        let mut output = match created {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create exclusive copied-mod temp file: {e}")),
        };
        let result = (|| {
            let mut chunk = [0u8; CHUNK];
            loop {
                let read = input
                    .read(&mut chunk)
                    .map_err(|e| format!("read active mod for copy: {e}"))?;
                if read == 0 {
                    break;
                }
                output
                    .write_all(&chunk[..read])
                    .map_err(|e| format!("write copied mod: {e}"))?;
            }
            output
                .sync_all()
                .map_err(|e| format!("sync copied mod: {e}"))?;
            drop(output);
            reject_symlink_path(destination)?;
            if destination.exists() {
                return Err("active profile filename collision in staged generation".into());
            }
            fs::rename(&temp, destination)
                .map_err(|e| format!("publish copied mod into generation: {e}"))?;
            fsync_dir(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }
    Err("could not allocate a unique copied-mod temp file".into())
}

fn download_to_generation(
    agent: &ureq::Agent,
    item: &ResolvedDownload,
    generation_dir: &Path,
    progress: &Arc<Mutex<ModSyncProgress>>,
    file_index: usize,
    total_files: usize,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let file = &item.version.file;
    if !host_allowed(&file.url) {
        return Err("refusing download URL outside Modrinth CDN".into());
    }
    let counter = DOWNLOAD_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = generation_dir.join(format!(
        ".{}.download-{}-{counter:x}",
        file.filename,
        std::process::id()
    ));
    reject_symlink_path(&temp)?;
    let mut response = agent
        .get(&file.url)
        .call()
        .map_err(|e| format!("download {}: {e}", item.version.project_id))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| format!("create exclusive staged download: {e}"))?;
    let mut sha512 = Sha512::new();
    let mut sha256 = Sha256::new();
    let mut magic = [0u8; 4];
    let mut magic_len = 0usize;
    let mut bytes = 0u64;
    let mut chunk = [0u8; CHUNK];
    let mut reader = response.body_mut().as_reader();
    let read_result = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err("Modrinth sync cancelled".to_owned());
        }
        match reader.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                let Some(next_bytes) = bytes.checked_add(n as u64) else {
                    break Err("download size overflow".to_owned());
                };
                bytes = next_bytes;
                if bytes > MAX_MOD_BYTES || bytes > file.size {
                    break Err("download exceeds declared size or cap".to_owned());
                }
                let take = (4 - magic_len).min(n);
                magic[magic_len..magic_len + take].copy_from_slice(&chunk[..take]);
                magic_len += take;
                sha512.update(&chunk[..n]);
                sha256.update(&chunk[..n]);
                if let Err(e) = output.write_all(&chunk[..n]) {
                    break Err(format!("write staged download: {e}"));
                }
                set_progress(
                    progress,
                    ModSyncProgress::Downloading {
                        mod_id: item.version.project_id.clone(),
                        file_index,
                        total_files,
                        downloaded_bytes: bytes,
                        total_bytes: Some(file.size),
                    },
                );
            }
            Err(e) => break Err(format!("read mod download: {e}")),
        }
    };
    if let Err(error) = read_result {
        drop(output);
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(e) = output.sync_all() {
        drop(output);
        let _ = fs::remove_file(&temp);
        return Err(format!("sync staged download: {e}"));
    }
    drop(output);
    let fail = |why: String| {
        let _ = fs::remove_file(&temp);
        why
    };
    if bytes != file.size {
        return Err(fail(
            "download length differs from Modrinth metadata".into(),
        ));
    }
    if magic != *b"PK\x03\x04" {
        return Err(fail("download is not a JAR/ZIP archive".into()));
    }
    let actual512 = hex_encode(&sha512.finalize());
    if actual512 != file.sha512 {
        return Err(fail("Modrinth SHA-512 verification failed".into()));
    }
    let actual256 = hex_encode(&sha256.finalize());
    if let Some(expected) = &file.sha256 {
        if !expected.eq_ignore_ascii_case(&actual256) {
            return Err(fail("Modrinth SHA-256 verification failed".into()));
        }
    }
    verify_jar_archive_safety(&temp).map_err(fail)?;
    let destination = generation_dir.join(&file.filename);
    reject_symlink_path(&destination).map_err(fail)?;
    if destination.exists() {
        return Err(fail("staged destination already exists".into()));
    }
    fs::rename(&temp, &destination).map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("publish staged mod into generation: {e}")
    })?;
    fsync_dir(generation_dir)?;
    Ok(destination)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JarDependency {
    pub mod_id: String,
    pub version_range: String,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedModJar {
    pub mod_id: String,
    pub version: String,
    pub loader: ModLoader,
    pub side: Option<ModSide>,
    pub dependencies: Vec<JarDependency>,
}

pub fn inspect_mod_jar(path: &Path) -> Result<InspectedModJar, String> {
    verify_jar_archive_safety(path)?;
    let index = LazyZipIndex::open_all(path).map_err(|e| format!("open inspected JAR: {e}"))?;
    if index
        .entries()
        .iter()
        .any(|entry| entry.name == "fabric.mod.json")
    {
        let bytes = index
            .read_entry("fabric.mod.json", MAX_MANIFEST_BYTES)
            .map_err(|e| format!("read fabric.mod.json: {e}"))?;
        return inspect_fabric_manifest(&bytes);
    }
    if index
        .entries()
        .iter()
        .any(|entry| entry.name == "quilt.mod.json")
    {
        let bytes = index
            .read_entry("quilt.mod.json", MAX_MANIFEST_BYTES)
            .map_err(|e| format!("read quilt.mod.json: {e}"))?;
        return inspect_quilt_manifest(&bytes);
    }
    for name in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
        if index.entries().iter().any(|entry| entry.name == name) {
            let bytes = index
                .read_entry(name, MAX_MANIFEST_BYTES)
                .map_err(|e| format!("read {name}: {e}"))?;
            return inspect_neoforge_manifest(&bytes);
        }
    }
    Err("JAR has no supported Fabric/Quilt/NeoForge mod manifest".into())
}

fn inspect_fabric_manifest(bytes: &[u8]) -> Result<InspectedModJar, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "fabric.mod.json is not UTF-8")?;
    let json = parse_json_with_limits(text, small_json_limits(), true)
        .map_err(|e| format!("invalid fabric.mod.json: {e}"))?;
    let side = match json.get("environment").and_then(JsonValue::as_str) {
        Some("client") => Some(ModSide::Client),
        Some("server") => Some(ModSide::Server),
        _ => None,
    };
    let mut dependencies = Vec::new();
    if let Some(entries) = json.get("depends").and_then(JsonValue::as_object) {
        for (id, value) in entries {
            if matches!(id.as_str(), "minecraft" | "fabricloader" | "java") {
                continue;
            }
            let Some(version_range) = value.as_str() else {
                return Err(format!(
                    "fabric.mod.json dependency {id} uses a non-scalar version range"
                ));
            };
            dependencies.push(JarDependency {
                mod_id: id.clone(),
                version_range: version_range.to_owned(),
                required: true,
            });
        }
    }
    Ok(InspectedModJar {
        mod_id: json
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or("fabric.mod.json lacks id")?
            .to_owned(),
        version: json
            .get("version")
            .and_then(JsonValue::as_str)
            .ok_or("fabric.mod.json lacks version")?
            .to_owned(),
        loader: ModLoader::Fabric,
        side,
        dependencies,
    })
}

fn inspect_quilt_manifest(bytes: &[u8]) -> Result<InspectedModJar, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "quilt.mod.json is not UTF-8")?;
    let json = parse_json_with_limits(text, small_json_limits(), true)
        .map_err(|e| format!("invalid quilt.mod.json: {e}"))?;
    let loader_obj = json
        .get("quilt_loader")
        .ok_or("quilt.mod.json lacks quilt_loader")?;
    let mut dependencies = Vec::new();
    if let Some(items) = loader_obj.get("depends").and_then(JsonValue::as_array) {
        for item in items {
            let Some(id) = item.get("id").and_then(JsonValue::as_str) else {
                continue;
            };
            if matches!(id, "minecraft" | "quilt_loader" | "java") {
                continue;
            }
            dependencies.push(JarDependency {
                mod_id: id.to_owned(),
                version_range: item
                    .get("versions")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("*")
                    .to_owned(),
                required: item
                    .get("optional")
                    .and_then(JsonValue::as_bool)
                    .map(|optional| !optional)
                    .unwrap_or(true),
            });
        }
    }
    Ok(InspectedModJar {
        mod_id: loader_obj
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or("quilt.mod.json lacks id")?
            .to_owned(),
        version: loader_obj
            .get("version")
            .and_then(JsonValue::as_str)
            .ok_or("quilt.mod.json lacks version")?
            .to_owned(),
        loader: ModLoader::Quilt,
        side: None,
        dependencies,
    })
}

fn inspect_neoforge_manifest(bytes: &[u8]) -> Result<InspectedModJar, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "NeoForge mods.toml is not UTF-8")?;
    let mut mod_id = None;
    let mut version = None;
    let mut dependencies = Vec::new();
    let mut current_dep: Option<(String, String, bool)> = None;
    let mut in_mods = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[mods]]" {
            in_mods = true;
            continue;
        }
        if line.starts_with("[[dependencies.") {
            if let Some((id, range, mandatory)) = current_dep.take() {
                dependencies.push(JarDependency {
                    mod_id: id,
                    version_range: range,
                    required: mandatory,
                });
            }
            in_mods = false;
            let id = line
                .trim_start_matches("[[dependencies.")
                .trim_end_matches("]]")
                .to_owned();
            current_dep = Some((id, "*".to_owned(), true));
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = raw.trim().trim_matches('"').to_owned();
        if in_mods && key == "modId" && mod_id.is_none() {
            mod_id = Some(value.clone());
        } else if in_mods && key == "version" && version.is_none() {
            version = Some(value.clone());
        } else if let Some((id, range, mandatory)) = &mut current_dep {
            match key {
                "modId" => *id = value,
                "versionRange" => *range = value,
                "mandatory" => *mandatory = value.eq_ignore_ascii_case("true"),
                "type" if value == "optional" => *mandatory = false,
                _ => {}
            }
        }
    }
    if let Some((id, range, mandatory)) = current_dep {
        dependencies.push(JarDependency {
            mod_id: id,
            version_range: range,
            required: mandatory,
        });
    }
    dependencies.retain(|dep| {
        !matches!(
            dep.mod_id.as_str(),
            "minecraft" | "neoforge" | "forge" | "javafml"
        )
    });
    Ok(InspectedModJar {
        mod_id: mod_id.ok_or("NeoForge manifest lacks modId")?,
        version: version.ok_or("NeoForge manifest lacks mod version")?,
        loader: ModLoader::NeoForge,
        side: None,
        dependencies,
    })
}

fn validate_inspected_jar(
    inspection: &InspectedModJar,
    item: &ResolvedDownload,
) -> Result<(), String> {
    if inspection.mod_id.is_empty()
        || inspection.mod_id.len() > 128
        || !inspection
            .mod_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("downloaded JAR manifest has an invalid mod id".into());
    }
    if let Some(expected_id) = &item.expected_mod_id {
        if &inspection.mod_id != expected_id && inspection.mod_id != item.version.project_id {
            return Err(format!(
                "downloaded JAR mod id {} does not match expected {expected_id}",
                inspection.mod_id
            ));
        }
    }
    if inspection.version != item.version.version_number {
        return Err("downloaded JAR version differs from exact Modrinth version".into());
    }
    if let Some(loader) = item.version.target_loader {
        if inspection.loader != loader {
            return Err(
                "downloaded JAR manifest loader differs from exact loader selection".into(),
            );
        }
    }
    if let Some(target_side) = item.version.target_side {
        if inspection.side.is_some_and(|side| side != target_side) {
            return Err("downloaded JAR manifest side is incompatible with target side".into());
        }
    }
    Ok(())
}

fn validate_dependency_manifest_closure(
    inspected: &[InspectedModJar],
    entries: &[GenerationModEntry],
) -> Result<(), String> {
    let available: BTreeMap<&str, &str> = entries
        .iter()
        .map(|entry| (entry.mod_id.as_str(), entry.version.as_str()))
        .collect();
    for jar in inspected {
        for dep in &jar.dependencies {
            if !dep.required {
                continue;
            }
            let Some(installed_version) = available.get(dep.mod_id.as_str()) else {
                return Err(format!(
                    "JAR {} declares required dependency {} outside the transactional closure",
                    jar.mod_id, dep.mod_id
                ));
            };
            if !version_requirement_matches(installed_version, &dep.version_range) {
                return Err(format!(
                    "JAR {} requires {} version {} but the transaction provides {}",
                    jar.mod_id, dep.mod_id, dep.version_range, installed_version
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDependencyDescriptor {
    pub mod_id: String,
    pub required_version_range: String,
    pub requesting_mods: Vec<String>,
    pub loader: ModLoader,
    pub game_version: String,
    pub side: ModSide,
    pub project_id: Option<String>,
    pub version_id: Option<String>,
    pub modrinth_slug: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModrinthCandidate {
    pub mod_id: String,
    pub project_id: String,
    pub version_id: String,
    pub version_number: String,
    pub filename: String,
    pub download_url: String,
    pub size_bytes: u64,
    pub sha512: String,
    pub sha256: Option<String>,
    pub requesting_mods: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsatisfiedMissingDependency {
    pub mod_id: String,
    pub required_version_range: String,
    pub requesting_mods: Vec<String>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MissingDependencyConsentChoice {
    ApproveAutoFetchAndSoftReload,
    IsolateUnsatisfiedMods,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDependencyConsentPlan {
    pub candidates: Vec<ResolvedModrinthCandidate>,
    pub unsatisfied_dependencies: Vec<UnsatisfiedMissingDependency>,
    pub isolated_mods_if_declined: Vec<String>,
    pub choices: Vec<MissingDependencyConsentChoice>,
}

pub fn build_missing_dep_consent_plan(
    descriptors: &[MissingDependencyDescriptor],
) -> Result<MissingDependencyConsentPlan, String> {
    build_missing_dep_consent_plan_at(descriptors, DEFAULT_MODRINTH_API)
}

pub fn build_missing_dep_consent_plan_at(
    descriptors: &[MissingDependencyDescriptor],
    api_base: &str,
) -> Result<MissingDependencyConsentPlan, String> {
    if descriptors.len() > MAX_MODS {
        return Err(format!("missing dependency count exceeds {MAX_MODS}"));
    }
    let agent = modrinth_agent();
    let mut candidates = Vec::new();
    let mut unsatisfied = Vec::new();
    let mut isolated = BTreeSet::new();
    for descriptor in descriptors {
        for requester in &descriptor.requesting_mods {
            isolated.insert(requester.clone());
        }
        let req = ExactModRequirement {
            mod_id: descriptor.mod_id.clone(),
            required_version: descriptor.required_version_range.clone(),
            sha512: None,
            sha256: None,
            modrinth_slug: descriptor.modrinth_slug.clone(),
            project_id: descriptor.project_id.clone(),
            version_id: descriptor.version_id.clone(),
            game_version: descriptor.game_version.clone(),
            loader: descriptor.loader,
            side: descriptor.side,
            requested_by: descriptor.requesting_mods.clone(),
        };
        match resolve_required_closure(&agent, api_base, &[req]) {
            Ok(versions) => {
                for download in versions {
                    candidates.push(ResolvedModrinthCandidate {
                        mod_id: download
                            .expected_mod_id
                            .unwrap_or(download.version.project_id.clone()),
                        project_id: download.version.project_id,
                        version_id: download.version.version_id,
                        version_number: download.version.version_number,
                        filename: download.version.file.filename,
                        download_url: download.version.file.url,
                        size_bytes: download.version.file.size,
                        sha512: download.version.file.sha512,
                        sha256: download.version.file.sha256,
                        requesting_mods: descriptor.requesting_mods.clone(),
                    });
                }
            }
            Err(reason) => unsatisfied.push(UnsatisfiedMissingDependency {
                mod_id: descriptor.mod_id.clone(),
                required_version_range: descriptor.required_version_range.clone(),
                requesting_mods: descriptor.requesting_mods.clone(),
                reason,
            }),
        }
    }
    let mut deduped = BTreeMap::new();
    for candidate in candidates {
        let key = (
            candidate.project_id.clone(),
            candidate.version_id.clone(),
            candidate.filename.clone(),
        );
        match deduped.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                for requester in candidate.requesting_mods {
                    if !entry.get().requesting_mods.contains(&requester) {
                        entry.get_mut().requesting_mods.push(requester);
                    }
                }
                entry.get_mut().requesting_mods.sort();
            }
        }
    }
    let candidates = deduped.into_values().collect();
    Ok(MissingDependencyConsentPlan {
        candidates,
        unsatisfied_dependencies: unsatisfied,
        isolated_mods_if_declined: isolated.into_iter().collect(),
        choices: vec![
            MissingDependencyConsentChoice::ApproveAutoFetchAndSoftReload,
            MissingDependencyConsentChoice::IsolateUnsatisfiedMods,
        ],
    })
}

fn fetch_json(agent: &ureq::Agent, url: &str) -> Result<JsonValue, String> {
    if !host_allowed(url) {
        return Err("refusing API URL outside Modrinth".into());
    }
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| format!("Modrinth API request failed: {e}"))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_JSON_BYTES as u64)
        .read_to_vec()
        .map_err(|e| format!("read Modrinth response: {e}"))?;
    let text = std::str::from_utf8(&body).map_err(|_| "Modrinth response is not UTF-8")?;
    parse_json_with_limits(text, small_json_limits(), true)
        .map_err(|e| format!("invalid Modrinth JSON: {e}"))
}

fn small_json_limits() -> JsonLimits {
    JsonLimits {
        max_depth: 64,
        max_nodes: 200_000,
        max_bytes: MAX_JSON_BYTES,
    }
}

pub fn host_allowed(url: &str) -> bool {
    let Some((scheme, authority)) = url.split_once("://") else {
        return false;
    };
    if authority.is_empty() {
        return false;
    }
    let hostport = authority.split('/').next().unwrap_or("");
    if hostport.is_empty() || hostport.contains(['@', '#', '?']) {
        return false;
    }
    #[cfg(debug_assertions)]
    if scheme == "http" {
        if let Some((host, port)) = hostport.rsplit_once(':') {
            if (host == "127.0.0.1" || host == "localhost")
                && port.parse::<u16>().is_ok_and(|port| port > 0)
            {
                return true;
            }
        }
    }
    scheme == "https"
        && matches!(
            hostport,
            "api.modrinth.com"
                | "api.modrinth.com:443"
                | "cdn.modrinth.com"
                | "cdn.modrinth.com:443"
        )
}

pub fn sanitize_filename(filename: &str) -> Result<&str, String> {
    crate::mod_profile_tx::validate_member_filename(filename)?;
    Ok(filename)
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_modrinth_id(value: &str, label: &str) -> Result<(), String> {
    if !value.is_empty() && value.len() <= 64 && value.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!("invalid Modrinth {label}"))
    }
}

fn validate_hex(value: &str, len: usize, label: &str) -> Result<(), String> {
    if value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("invalid {label}"))
    }
}

fn field_str<'a>(value: &'a JsonValue, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("Modrinth response lacks {key}"))
}

fn field_num(value: &JsonValue, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(JsonValue::as_f64)
        .filter(|n| *n >= 0.0 && *n <= u64::MAX as f64 && n.fract() == 0.0)
        .map(|n| n as u64)
        .ok_or_else(|| format!("Modrinth response has invalid {key}"))
}

fn json_string_array_contains(value: Option<&JsonValue>, needle: &str) -> bool {
    value.and_then(JsonValue::as_array).is_some_and(|items| {
        items
            .iter()
            .filter_map(JsonValue::as_str)
            .any(|item| item == needle)
    })
}

fn version_type_rank(value: &str) -> u8 {
    match value {
        "release" => 0,
        "beta" => 1,
        "alpha" => 2,
        _ => 3,
    }
}

fn version_key(version: &str) -> (u8, Vec<u64>, String) {
    let numeric: Vec<u64> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .take(8)
        .collect();
    (
        if numeric.is_empty() { 0 } else { 1 },
        numeric,
        version.to_owned(),
    )
}

fn version_requirement_matches(version: &str, requirement: &str) -> bool {
    let requirement = requirement.trim();
    if requirement.is_empty() {
        return false;
    }
    if requirement == "*" {
        return true;
    }
    if !requirement.bytes().any(|b| {
        matches!(
            b,
            b'*' | b'[' | b']' | b'(' | b')' | b',' | b'<' | b'>' | b'=' | b'~' | b'^'
        )
    }) {
        return version == requirement;
    }
    parse_range_constraints(requirement).is_some_and(|constraints| {
        constraints.iter().all(|(op, bound)| {
            compare_versions(version, bound).is_some_and(|ordering| match *op {
                ">=" => ordering != std::cmp::Ordering::Less,
                ">" => ordering == std::cmp::Ordering::Greater,
                "<=" => ordering != std::cmp::Ordering::Greater,
                "<" => ordering == std::cmp::Ordering::Less,
                "=" => ordering == std::cmp::Ordering::Equal,
                _ => false,
            })
        })
    })
}

fn parse_range_constraints(requirement: &str) -> Option<Vec<(&str, String)>> {
    let mut out = Vec::new();
    let trimmed = requirement.trim();
    if trimmed == "*" {
        return Some(out);
    }
    if (trimmed.starts_with('[') || trimmed.starts_with('('))
        && (trimmed.ends_with(']') || trimmed.ends_with(')'))
    {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut parts = inner.split(',').map(str::trim);
        let lower = parts.next().unwrap_or("");
        let upper = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return None;
        }
        if !lower.is_empty()
            && upper.is_empty()
            && trimmed.starts_with('[')
            && trimmed.ends_with(']')
            && !inner.contains(',')
        {
            out.push(("=", lower.to_owned()));
            return Some(out);
        }
        if !lower.is_empty() {
            out.push((
                if trimmed.starts_with('[') { ">=" } else { ">" },
                lower.to_owned(),
            ));
        }
        if !upper.is_empty() {
            out.push((
                if trimmed.ends_with(']') { "<=" } else { "<" },
                upper.to_owned(),
            ));
        }
        return Some(out);
    }
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(bound) = trimmed.strip_prefix(op) {
            return Some(vec![(op, bound.trim().to_owned())]);
        }
    }
    if let Some(bound) = trimmed.strip_prefix('~') {
        let bound = bound.trim();
        let upper = bump_version_bound(bound, 1)?;
        return Some(vec![(">=", bound.to_owned()), ("<", upper)]);
    }
    if let Some(bound) = trimmed.strip_prefix('^') {
        let bound = bound.trim();
        let upper = bump_version_bound(bound, 0)?;
        return Some(vec![(">=", bound.to_owned()), ("<", upper)]);
    }
    if trimmed.bytes().any(|b| matches!(b, b',')) {
        let mut all = Vec::new();
        for part in trimmed.split(',') {
            all.extend(parse_range_constraints(part.trim())?);
        }
        return Some(all);
    }
    None
}

fn bump_version_bound(bound: &str, component: usize) -> Option<String> {
    let mut parts = numeric_version_parts(bound)?;
    while parts.len() <= component {
        parts.push(0);
    }
    parts[component] = parts[component].checked_add(1)?;
    for part in parts.iter_mut().skip(component + 1) {
        *part = 0;
    }
    Some(
        parts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join("."),
    )
}

fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let av = numeric_version_parts(a)?;
    let bv = numeric_version_parts(b)?;
    let len = av.len().max(bv.len());
    for index in 0..len {
        let x = av.get(index).copied().unwrap_or(0);
        let y = bv.get(index).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => {}
            ordering => return Some(ordering),
        }
    }
    Some(std::cmp::Ordering::Equal)
}

fn numeric_version_parts(version: &str) -> Option<Vec<u64>> {
    let version = version.strip_prefix(['v', 'V']).unwrap_or(version);
    let end = version
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(version.len());
    let numeric = &version[..end];
    if numeric.is_empty() {
        return None;
    }
    numeric
        .split('.')
        .map(|part| {
            if part.is_empty() {
                None
            } else {
                part.parse::<u64>().ok()
            }
        })
        .collect()
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn set_progress(shared: &Arc<Mutex<ModSyncProgress>>, next: ModSyncProgress) {
    *lock(shared) = next;
}
