// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Consent-driven, hash-verified Modrinth dependency downloads.

use megu_core::crypto::{hex_encode, Sha256};
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MOD_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_BATCH_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_MODS: usize = 64;
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModRequirement {
    pub mod_id: String,
    pub required_version: String,
    pub sha512: Option<String>,
    pub sha256: Option<String>,
    pub modrinth_slug: Option<String>,
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
        "https://api.modrinth.com",
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
    if reqs.len() > MAX_MODS {
        return Err(format!("mod count exceeds {MAX_MODS}"));
    }
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(15)))
        .max_redirects(0)
        .build()
        .into();
    let key = megu_core::crypto::sha256_hex(server_key.as_bytes());
    let mods_dir = game_dir.join("server-mods").join(&key[..16]);
    fs::create_dir_all(&mods_dir).map_err(|e| format!("create mod directory: {e}"))?;
    let mut total = 0u64;
    for (index, req) in reqs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Modrinth sync cancelled".into());
        }
        if req.mod_id.is_empty() {
            return Err("empty mod id".into());
        }
        let file = resolve_file(&agent, api_base, req)?;
        let filename = sanitize_filename(&file.filename)?;
        if file.size == 0 || file.size > MAX_MOD_BYTES {
            return Err(format!("{} file size outside allowed bounds", req.mod_id));
        }
        total = total.checked_add(file.size).ok_or("batch size overflow")?;
        if total > MAX_BATCH_BYTES {
            return Err(format!("batch exceeds {MAX_BATCH_BYTES} bytes"));
        }
        let destination = mods_dir.join(filename);
        let part = destination.with_extension("jar.part");
        set_progress(
            progress,
            ModSyncProgress::Downloading {
                mod_id: req.mod_id.clone(),
                file_index: index + 1,
                total_files: reqs.len(),
                downloaded_bytes: 0,
                total_bytes: Some(file.size),
            },
        );
        download_file(DownloadContext {
            agent: &agent,
            file: &file,
            requirement: req,
            part: &part,
            destination: &destination,
            progress,
            file_index: index + 1,
            total_files: reqs.len(),
            cancel,
        })?;
    }
    set_progress(
        progress,
        ModSyncProgress::Ready {
            mods_dir,
            downloaded_files: reqs.len(),
        },
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct ResolvedFile {
    url: String,
    filename: String,
    sha512: String,
    size: u64,
}

fn resolve_file(
    agent: &ureq::Agent,
    api_base: &str,
    req: &ModRequirement,
) -> Result<ResolvedFile, String> {
    let api = if let Some(hash) = req.sha512.as_deref() {
        validate_hex(hash, 128, "required SHA-512")?;
        format!("{api_base}/v2/version_file/{hash}?algorithm=sha512")
    } else {
        let slug = req.modrinth_slug.as_deref().unwrap_or(&req.mod_id);
        if !valid_slug(slug) {
            return Err("invalid Modrinth project slug".into());
        }
        format!("{api_base}/v2/project/{slug}/version?game_versions=%5B%221.21.1%22%5D")
    };
    let json = fetch_json(agent, &api)?;
    let file = if req.sha512.is_some() {
        json.get("files")
            .and_then(JsonValue::as_array)
            .and_then(|items| items.first())
            .ok_or("Modrinth hash lookup returned no file")?
    } else {
        let versions = json
            .as_array()
            .ok_or("Modrinth version response is not an array")?;
        let version = versions
            .iter()
            .find(|v| {
                v.get("version_number").and_then(JsonValue::as_str) == Some(&req.required_version)
            })
            .or_else(|| versions.first())
            .ok_or("Modrinth returned no compatible release")?;
        version
            .get("files")
            .and_then(JsonValue::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.get("primary").and_then(JsonValue::as_bool) == Some(true))
                    .or_else(|| items.first())
            })
            .ok_or("Modrinth release has no files")?
    };
    let url = field_str(file, "url")?.to_owned();
    if !host_allowed(&url) {
        return Err("refusing download URL outside Modrinth CDN".into());
    }
    let filename = field_str(file, "filename")?.to_owned();
    let hashes = file.get("hashes").ok_or("Modrinth file has no hashes")?;
    let sha512 = hashes
        .get("sha512")
        .and_then(JsonValue::as_str)
        .ok_or("Modrinth file has no SHA-512")?
        .to_ascii_lowercase();
    validate_hex(&sha512, 128, "Modrinth SHA-512")?;
    if req
        .sha512
        .as_ref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&sha512))
    {
        return Err("Modrinth SHA-512 differs from server requirement".into());
    }
    let size = field_num(file, "size")?;
    Ok(ResolvedFile {
        url,
        filename,
        sha512,
        size,
    })
}

struct DownloadContext<'a> {
    agent: &'a ureq::Agent,
    file: &'a ResolvedFile,
    requirement: &'a ModRequirement,
    part: &'a Path,
    destination: &'a Path,
    progress: &'a Arc<Mutex<ModSyncProgress>>,
    file_index: usize,
    total_files: usize,
    cancel: &'a AtomicBool,
}

fn download_file(context: DownloadContext<'_>) -> Result<(), String> {
    let DownloadContext {
        agent,
        file,
        requirement: req,
        part,
        destination: jar,
        progress,
        file_index: index,
        total_files: total,
        cancel,
    } = context;
    if !host_allowed(&file.url) {
        return Err("refusing download URL outside Modrinth CDN".into());
    }
    let mut response = agent
        .get(&file.url)
        .call()
        .map_err(|e| format!("download {}: {e}", req.mod_id))?;
    let mut output = File::create(part).map_err(|e| format!("create partial file: {e}"))?;
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
                    break Err(format!("write partial file: {e}"));
                }
                set_progress(
                    progress,
                    ModSyncProgress::Downloading {
                        mod_id: req.mod_id.clone(),
                        file_index: index,
                        total_files: total,
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
        let _ = fs::remove_file(part);
        return Err(error);
    }
    if let Err(e) = output.sync_all() {
        drop(output);
        let _ = fs::remove_file(part);
        return Err(format!("sync partial file: {e}"));
    }
    drop(output);
    set_progress(
        progress,
        ModSyncProgress::Verifying {
            mod_id: req.mod_id.clone(),
        },
    );
    let fail = |why: String| {
        let _ = fs::remove_file(part);
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
    if req
        .sha512
        .as_ref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&actual512))
    {
        return Err(fail("server SHA-512 verification failed".into()));
    }
    if let Some(expected) = &req.sha256 {
        validate_hex(expected, 64, "required SHA-256").map_err(fail)?;
        let actual = hex_encode(&sha256.finalize());
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(fail("server SHA-256 verification failed".into()));
        }
    }
    fs::rename(part, jar).map_err(|e| {
        let _ = fs::remove_file(part);
        format!("install verified mod: {e}")
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
    parse_json_with_limits(
        text,
        JsonLimits {
            max_depth: 64,
            max_nodes: 200_000,
            max_bytes: MAX_JSON_BYTES,
        },
        true,
    )
    .map_err(|e| format!("invalid Modrinth JSON: {e}"))
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
    #[cfg(test)]
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
    if filename.is_empty()
        || filename.contains("..")
        || filename
            .bytes()
            .any(|b| matches!(b, b'/' | b'\\' | b':' | 0))
        || !filename.to_ascii_lowercase().ends_with(".jar")
    {
        return Err("unsafe mod filename".into());
    }
    Ok(filename)
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn set_progress(shared: &Arc<Mutex<ModSyncProgress>>, next: ModSyncProgress) {
    *lock(shared) = next;
}

/// Minimal streaming implementation of FIPS 180-4 SHA-512.
#[derive(Clone)]
struct Sha512 {
    state: [u64; 8],
    block: [u8; 128],
    used: usize,
    bytes: u128,
}
impl Sha512 {
    fn new() -> Self {
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
    fn update(&mut self, mut input: &[u8]) {
        self.bytes += input.len() as u128;
        if self.used > 0 {
            let n = (128 - self.used).min(input.len());
            self.block[self.used..self.used + n].copy_from_slice(&input[..n]);
            self.used += n;
            input = &input[n..];
            if self.used == 128 {
                let b = self.block;
                self.compress(&b);
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
    fn finalize(mut self) -> [u8; 64] {
        let bits = self.bytes * 8;
        self.block[self.used] = 0x80;
        self.used += 1;
        if self.used > 112 {
            self.block[self.used..].fill(0);
            let b = self.block;
            self.compress(&b);
            self.block = [0; 128];
            self.used = 0;
        } else {
            self.block[self.used..112].fill(0);
        }
        self.block[112..].copy_from_slice(&bits.to_be_bytes());
        let b = self.block;
        self.compress(&b);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread;
    #[test]
    fn sha512_fips_vectors() {
        assert_eq!(hex_encode(&Sha512::new().finalize()), "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e");
        let mut abc = Sha512::new();
        abc.update(b"a");
        abc.update(b"bc");
        assert_eq!(hex_encode(&abc.finalize()),"ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f");
    }
    #[test]
    fn hostile_hosts_and_filenames_are_rejected() {
        assert!(!host_allowed("https://attacker.example/file.jar"));
        assert!(!host_allowed("https://api.modrinth.com.evil.invalid/file"));
        for name in [
            "../x.jar",
            "x/../../y.jar",
            "x\\y.jar",
            "x.jar:stream",
            "x.zip",
            "x\0.jar",
        ] {
            assert!(sanitize_filename(name).is_err(), "{name:?}");
        }
        assert!(host_allowed("https://cdn.modrinth.com/data/a.jar"));
    }
    #[test]
    fn computes_missing_and_version_mismatch() {
        let required = vec![
            ModRequirement {
                mod_id: "a".into(),
                required_version: "2".into(),
                sha512: None,
                sha256: None,
                modrinth_slug: None,
            },
            ModRequirement {
                mod_id: "b".into(),
                required_version: "1".into(),
                sha512: None,
                sha256: None,
                modrinth_slug: None,
            },
        ];
        let installed = BTreeMap::from([("a".into(), "1".into())]);
        let result = compute_missing_mods(&required, &installed);
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0].kind,
            ModMismatchKind::VersionMismatch {
                installed: "1".into(),
                required: "2".into()
            }
        );
        assert_eq!(result[1].kind, ModMismatchKind::Missing);
    }

    fn sha512_hex(bytes: &[u8]) -> String {
        let mut hash = Sha512::new();
        hash.update(bytes);
        hex_encode(&hash.finalize())
    }
    fn respond(mut stream: std::net::TcpStream, status: &str, content_type: &str, body: &[u8]) {
        let header=format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n", body.len());
        stream
            .write_all(header.as_bytes())
            .expect("write response header");
        stream.write_all(body).expect("write response body");
    }
    fn read_path(stream: &std::net::TcpStream) -> String {
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("read request line");
        line.split_whitespace().nth(1).unwrap_or("").to_owned()
    }
    fn serve_file_lookup(
        jar: Vec<u8>,
        advertised_hash: String,
        routes: usize,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("set bounded accept");
        let handle = thread::spawn(move || {
            let mut paths = Vec::new();
            for _index in 0..routes {
                let mut attempts = 0;
                let accepted = loop {
                    match listener.accept() {
                        Ok(connection) => break Some(connection),
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock && attempts < 300 =>
                        {
                            attempts += 1;
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break None,
                        Err(error) => panic!("accept test request: {error}"),
                    }
                };
                let Some((stream, _)) = accepted else {
                    break;
                };
                let path = read_path(&stream);
                paths.push(path.clone());
                if path.starts_with("/v2/version_file/") || path.starts_with("/v2/project/") {
                    let body = if path.starts_with("/v2/version_file/") {
                        format!("{{\"files\":[{{\"url\":\"http://{addr}/download\",\"filename\":\"example.jar\",\"size\":{},\"hashes\":{{\"sha512\":\"{advertised_hash}\"}},\"primary\":true}}]}}",jar.len())
                    } else {
                        format!("[{{\"version_number\":\"1.0\",\"files\":[{{\"url\":\"http://{addr}/download\",\"filename\":\"example.jar\",\"size\":{},\"hashes\":{{\"sha512\":\"{advertised_hash}\"}},\"primary\":true}}]}}]",jar.len())
                    };
                    respond(stream, "200 OK", "application/json", body.as_bytes());
                } else if path == "/download" {
                    respond(stream, "200 OK", "application/java-archive", &jar);
                } else {
                    respond(stream, "404 Not Found", "text/plain", b"no route");
                }
            }
            paths
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn loopback_hash_endpoint_downloads_and_verifies_before_install() {
        let jar = b"PK\x03\x04verified jar".to_vec();
        let hash = sha512_hex(&jar);
        let (api, server) = serve_file_lookup(jar.clone(), hash.clone(), 2);
        let root = std::env::temp_dir().join(format!("megu-mod-sync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let requirement = ModRequirement {
            mod_id: "example".into(),
            required_version: "1.0".into(),
            sha512: Some(hash.clone()),
            sha256: Some(hex_encode(&megu_core::crypto::sha256(&jar))),
            modrinth_slug: Some("example".into()),
        };
        let progress = Arc::new(Mutex::new(ModSyncProgress::Resolving));
        sync_mods_at(
            &[requirement],
            &root,
            "server-a",
            &progress,
            &api,
            &AtomicBool::new(false),
        )
        .expect("mock Modrinth sync");
        let paths = server.join().expect("server join");
        assert!(paths[0].starts_with(&format!("/v2/version_file/{hash}?algorithm=sha512")));
        assert_eq!(paths[1], "/download");
        let ModSyncProgress::Ready {
            mods_dir,
            downloaded_files,
        } = lock(&progress).clone()
        else {
            panic!("expected Ready")
        };
        assert_eq!(downloaded_files, 1);
        assert_eq!(
            fs::read(mods_dir.join("example.jar")).expect("installed file"),
            jar
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn loopback_project_lookup_and_hash_mismatch_remove_partial_file() {
        let actual = b"PK\x03\x04wrong bytes".to_vec();
        let advertised = sha512_hex(b"PK\x03\x04right bytes");
        let (api, server) = serve_file_lookup(actual, advertised, 2);
        let root = std::env::temp_dir().join(format!("megu-mod-sync-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let requirement = ModRequirement {
            mod_id: "example".into(),
            required_version: "1.0".into(),
            sha512: None,
            sha256: None,
            modrinth_slug: Some("example".into()),
        };
        let progress = Arc::new(Mutex::new(ModSyncProgress::Resolving));
        assert!(sync_mods_at(
            &[requirement],
            &root,
            "server-b",
            &progress,
            &api,
            &AtomicBool::new(false)
        )
        .is_err());
        let paths = server.join().expect("server join");
        assert!(paths[0].starts_with("/v2/project/example/version?game_versions="));
        let mods_dir = root.join("server-mods");
        let mut partials = 0;
        for entry in fs::read_dir(mods_dir).expect("isolated dir") {
            for file in fs::read_dir(entry.expect("server dir").path()).expect("mod dir") {
                let path = file.expect("file").path();
                if path.extension().is_some_and(|ext| ext == "part") {
                    partials += 1;
                }
            }
        }
        assert_eq!(partials, 0);
        let _ = fs::remove_dir_all(root);
    }
}
