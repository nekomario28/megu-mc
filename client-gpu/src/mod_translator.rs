// SPDX-License-Identifier: LGPL-3.0-or-later
//! On-demand local Mod Japanese auto-translation powered by PLaMo 3 Translate.
//!
//! The model and `llama-server` runtime are never bundled. After explicit user
//! consent the model is fetched directly from Preferred Networks' official
//! HuggingFace repositories, a transient loopback-only `llama-server` process
//! performs translation, and the process exits before gameplay resumes.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use megu_core::hash::Fnv1a64;
use megu_core::json::{parse_json_with_limits, write_json_string, JsonLimits, JsonValue};

use crate::vanilla_jar::{read_bounded_file, read_selected_entries};

pub const PLAMO3_LICENSE_NAME: &str = "PLaMo Community License";
pub const PLAMO3_LICENSE_URL: &str = "https://plamo.preferredai.jp/info/plamo-community-license-en";
pub const PLAMO3_GUIDANCE_JA: &str = "PLaMo 3 Translate は Megu に同梱されていません。ユーザーの同意後、Preferred Networks の公式 HuggingFace リポジトリから直接ダウンロードします。個人・学術利用は無償で、年間売上 10 億円未満の商用利用は PFN 登録で可能です。ローカル LLM は翻訳中だけ起動し、終了直後に停止するためゲームプレイ中の RAM/VRAM を使用しません。";
pub const PLAMO3_GUIDANCE_EN: &str = "PLaMo 3 Translate is not bundled with Megu. After user consent it is downloaded directly from Preferred Networks' official HuggingFace repository. It is free for personal and academic use, and commercial use under JPY 1B annual revenue is available with PFN registration. The local LLM runs only while translating and terminates immediately afterwards, using zero RAM/VRAM during gameplay.";

pub const AUTO_TRANSLATION_PACK_DIR: &str = "megu_auto_ja_jp";
pub const AUTO_TRANSLATION_MANIFEST: &str = ".megu_translation_manifest.json";
pub const TRANSLATION_BATCH_SIZE: usize = 16;
pub const MAX_LANG_JSON_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CHAT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MODEL_DOWNLOAD_BYTES: u64 = 7 * 1024 * 1024 * 1024;
pub const MAX_CHAT_TEMPLATE_BYTES: u64 = 128 * 1024;

const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: 64,
    max_nodes: 200_000,
    max_bytes: MAX_LANG_JSON_BYTES,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Plamo3Preset {
    Lightweight08B,
    #[default]
    Standard2B,
    HighQuality8B,
}

pub const PLAMO3_PRESETS: [Plamo3Preset; 3] = [
    Plamo3Preset::Lightweight08B,
    Plamo3Preset::Standard2B,
    Plamo3Preset::HighQuality8B,
];

impl Plamo3Preset {
    pub fn huggingface_repo(self) -> &'static str {
        match self {
            Self::Lightweight08B => "pfnet/plamo-3-translate-0.8B-GGUF",
            Self::Standard2B => "pfnet/plamo-3-translate-2b-GGUF",
            Self::HighQuality8B => "pfnet/plamo-3-translate-8b-GGUF",
        }
    }

    pub fn gguf_filename(self) -> &'static str {
        match self {
            Self::Lightweight08B => "plamo-3-translate-0.8b-Q4_K_M.gguf",
            Self::Standard2B => "plamo-3-translate-2b-Q4_K_M.gguf",
            Self::HighQuality8B => "plamo-3-translate-8b-Q4_K_M.gguf",
        }
    }

    pub fn approximate_size_bytes(self) -> u64 {
        match self {
            Self::Lightweight08B => 550 * 1024 * 1024,
            Self::Standard2B => 1_700 * 1024 * 1024,
            Self::HighQuality8B => 5_000 * 1024 * 1024,
        }
    }

    pub fn max_download_bytes(self) -> u64 {
        match self {
            Self::Lightweight08B => 768 * 1024 * 1024,
            Self::Standard2B => 2_300 * 1024 * 1024,
            Self::HighQuality8B => 6_500 * 1024 * 1024,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lightweight08B => "PLaMo 3 Translate 0.8B (軽量・CPU向け)",
            Self::Standard2B => "PLaMo 3 Translate 2B (推奨・バランス)",
            Self::HighQuality8B => "PLaMo 3 Translate 8B (高品質・GPU向け)",
        }
    }

    pub fn manifest_id(self) -> &'static str {
        match self {
            Self::Lightweight08B => "lightweight-0.8b",
            Self::Standard2B => "standard-2b",
            Self::HighQuality8B => "high-quality-8b",
        }
    }

    pub fn cache_dir_name(self) -> &'static str {
        match self {
            Self::Lightweight08B => "plamo-3-translate-0.8b",
            Self::Standard2B => "plamo-3-translate-2b",
            Self::HighQuality8B => "plamo-3-translate-8b",
        }
    }

    pub fn model_path(self, cache_dir: &Path) -> PathBuf {
        cache_dir
            .join("models")
            .join(self.cache_dir_name())
            .join(self.gguf_filename())
    }

    pub fn chat_template_path(self, cache_dir: &Path) -> PathBuf {
        cache_dir
            .join("models")
            .join(self.cache_dir_name())
            .join("chat_template.jinja")
    }

    pub fn resolve_url(self, filename: &str) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/{filename}",
            self.huggingface_repo()
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranslatorInstallState {
    ModelNotInstalled,
    RuntimeMissing,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslatorInstallStatus {
    pub preset: Plamo3Preset,
    pub model_path: PathBuf,
    pub chat_template_path: PathBuf,
    pub model_installed: bool,
    pub chat_template_installed: bool,
    pub llama_server_path: Option<PathBuf>,
}

impl TranslatorInstallStatus {
    pub fn state(&self) -> TranslatorInstallState {
        if !self.model_installed || !self.chat_template_installed {
            TranslatorInstallState::ModelNotInstalled
        } else if self.llama_server_path.is_none() {
            TranslatorInstallState::RuntimeMissing
        } else {
            TranslatorInstallState::Ready
        }
    }

    pub fn action_label(&self) -> &'static str {
        match self.state() {
            TranslatorInstallState::ModelNotInstalled => "[翻訳モデルをインストール]",
            TranslatorInstallState::RuntimeMissing => {
                "[llama-server を導入してください / インストール]"
            }
            TranslatorInstallState::Ready => "[未翻訳 Mod を日本語化]",
        }
    }
}

pub fn default_translation_cache_dir() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("megu").join("translation");
    }
    #[cfg(target_os = "windows")]
    if let Some(local) = env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(local).join("megu").join("translation");
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home)
            .join("Library")
            .join("Caches")
            .join("megu")
            .join("translation");
    }
    if let Some(home) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(home)
            .join(".cache")
            .join("megu")
            .join("translation")
    } else {
        env::temp_dir().join("megu-translation-cache")
    }
}

pub fn detect_llama_server(cache_dir: &Path) -> Option<PathBuf> {
    if let Some(path) = env::var_os("MEGU_LLAMA_SERVER_BIN").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let bundled = cache_dir.join("bin").join(if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    });
    if bundled.is_file() {
        return Some(bundled);
    }
    let binary = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

pub fn detect_install_status(cache_dir: &Path, preset: Plamo3Preset) -> TranslatorInstallStatus {
    let model_path = preset.model_path(cache_dir);
    let chat_template_path = preset.chat_template_path(cache_dir);
    TranslatorInstallStatus {
        preset,
        model_installed: model_path.is_file(),
        chat_template_installed: chat_template_path.is_file(),
        llama_server_path: detect_llama_server(cache_dir),
        model_path,
        chat_template_path,
    }
}

fn host_port(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some((scheme, authority))
}

pub fn huggingface_download_url_allowed(url: &str) -> bool {
    let Some((scheme, authority)) = host_port(url) else {
        return false;
    };
    if scheme != "https" {
        return false;
    }
    let host = authority
        .strip_suffix(":443")
        .unwrap_or(authority)
        .to_ascii_lowercase();
    if host.contains(':') {
        return false;
    }
    host == "huggingface.co"
        || host == "cas-bridge.xethub.hf.co"
        || (host.starts_with("cdn-lfs")
            && host.ends_with(".hf.co")
            && host.len() >= "cdn-lfs.hf.co".len())
}

fn resolve_redirect(current: &str, location: &str) -> Option<String> {
    if location.starts_with("https://") {
        return Some(location.to_owned());
    }
    if let Some(path) = location.strip_prefix('/') {
        let (scheme, rest) = current.split_once("://")?;
        let authority = rest.split(['/', '?', '#']).next()?;
        return Some(format!("{scheme}://{authority}/{path}"));
    }
    None
}

fn allowed_download_response(
    agent: &ureq::Agent,
    url: &str,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut current = url.to_owned();
    for _ in 0..8 {
        if !huggingface_download_url_allowed(&current) {
            return Err("refusing download URL outside HuggingFace allowlist".into());
        }
        let response = agent
            .get(&current)
            .call()
            .map_err(|e| format!("HuggingFace download request failed: {e}"))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("HuggingFace redirect lacks Location header")?;
            current = resolve_redirect(&current, location)
                .ok_or("unsupported HuggingFace redirect target")?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!(
                "HuggingFace download returned HTTP {}",
                response.status()
            ));
        }
        return Ok(response);
    }
    Err("too many HuggingFace redirects".into())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .replace(['/', '\\'], "_")
    ));
    let write_result = (|| {
        let mut file = File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        if path.exists() {
            fs::remove_file(path).map_err(|e| format!("replace {}: {e}", path.display()))?;
        }
        fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn download_allowlisted_file(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    max_bytes: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let part = destination.with_extension("part");
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let result = (|| {
        let mut response = allowed_download_response(agent, url)?;
        let total = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if total.is_some_and(|n| n == 0 || n > max_bytes) {
            return Err("download size outside allowed bounds".into());
        }
        let mut output =
            File::create(&part).map_err(|e| format!("create partial download: {e}"))?;
        let mut reader = response.body_mut().as_reader();
        let mut bytes = 0u64;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("PLaMo 3 model installation cancelled".into());
            }
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    bytes = bytes
                        .checked_add(n as u64)
                        .ok_or("download size overflow")?;
                    if bytes > max_bytes || total.is_some_and(|declared| bytes > declared) {
                        return Err("download exceeds declared size or safety cap".into());
                    }
                    output
                        .write_all(&chunk[..n])
                        .map_err(|e| format!("write partial download: {e}"))?;
                    progress(bytes, total);
                }
                Err(e) => return Err(format!("read HuggingFace download: {e}")),
            }
        }
        output
            .sync_all()
            .map_err(|e| format!("sync partial download: {e}"))?;
        drop(output);
        if bytes == 0 {
            return Err("empty HuggingFace download".into());
        }
        if destination.exists() {
            fs::remove_file(destination).map_err(|e| format!("replace installed file: {e}"))?;
        }
        fs::rename(&part, destination).map_err(|e| format!("install downloaded file: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

pub fn install_plamo3_model(
    cache_dir: &Path,
    preset: Plamo3Preset,
    cancel: &AtomicBool,
    mut progress_cb: impl FnMut(&str, u64, Option<u64>),
) -> Result<(), String> {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(120)))
        .max_redirects(0)
        .build()
        .into();
    let template = preset.chat_template_path(cache_dir);
    if !template.is_file() {
        let url = preset.resolve_url("chat_template.jinja");
        download_allowlisted_file(
            &agent,
            &url,
            &template,
            MAX_CHAT_TEMPLATE_BYTES,
            cancel,
            &mut |done, total| progress_cb("chat_template.jinja", done, total),
        )?;
    }
    let model = preset.model_path(cache_dir);
    if !model.is_file() {
        let url = preset.resolve_url(preset.gguf_filename());
        download_allowlisted_file(
            &agent,
            &url,
            &model,
            preset.max_download_bytes().min(MAX_MODEL_DOWNLOAD_BYTES),
            cancel,
            &mut |done, total| progress_cb(preset.gguf_filename(), done, total),
        )?;
    }
    Ok(())
}

pub struct TransientLlamaServer {
    child: Child,
    endpoint: String,
}

impl TransientLlamaServer {
    pub fn spawn(
        cache_dir: &Path,
        preset: Plamo3Preset,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let status = detect_install_status(cache_dir, preset);
        if status.state() != TranslatorInstallState::Ready {
            return Err("PLaMo 3 model or llama-server runtime is not installed".into());
        }
        let server = status
            .llama_server_path
            .ok_or("llama-server runtime is not installed")?;
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("bind loopback probe: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("read loopback probe port: {e}"))?
            .port();
        drop(listener);
        let mut child = Command::new(server)
            .arg("-m")
            .arg(&status.model_path)
            .arg("--chat-template-file")
            .arg(&status.chat_template_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("-c")
            .arg("8192")
            .arg("--alias")
            .arg("plamo-3-translate")
            .arg("--no-prefill-assistant")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn llama-server: {e}"))?;
        let endpoint = format!("http://127.0.0.1:{port}");
        let started = Instant::now();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(300)))
            .timeout_recv_response(Some(Duration::from_millis(500)))
            .max_redirects(0)
            .build()
            .into();
        while started.elapsed() < Duration::from_secs(90) {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("llama-server startup cancelled".into());
            }
            if let Ok(Some(exit)) = child.try_wait() {
                return Err(format!("llama-server exited during startup: {exit}"));
            }
            if agent
                .get(&format!("{endpoint}/health"))
                .call()
                .is_ok_and(|response| response.status().as_u16() == 200)
            {
                return Ok(Self { child, endpoint });
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err("timed out waiting for llama-server /health".into())
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for TransientLlamaServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UntranslatedModSummary {
    pub mod_file: PathBuf,
    pub namespace: String,
    pub total_keys: usize,
    pub already_ja_keys: usize,
    pub missing_keys: usize,
    pub fingerprint: String,
}

type LangPair = (Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Clone, Debug)]
struct ModLangScan {
    mod_file: PathBuf,
    namespace: String,
    fingerprint: String,
    en_us: BTreeMap<String, String>,
    ja_jp: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranslationRunSummary {
    pub scanned_mods: usize,
    pub translated_namespaces: usize,
    pub translated_entries: usize,
    pub output_pack: PathBuf,
}

fn lang_namespace(path: &str, file: &str) -> Option<String> {
    let rest = path.strip_prefix("assets/")?;
    let (namespace, tail) = rest.split_once('/')?;
    if tail == format!("lang/{file}") && !namespace.is_empty() {
        Some(namespace.to_owned())
    } else {
        None
    }
}

fn parse_lang_json(bytes: &[u8], context: &str) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{context} is not UTF-8"))?;
    let json = parse_json_with_limits(text, JSON_LIMITS, true)
        .map_err(|e| format!("invalid {context}: {e}"))?;
    let object = json
        .as_object()
        .ok_or_else(|| format!("{context} is not a JSON object"))?;
    let mut out = BTreeMap::new();
    for (key, value) in object {
        if let Some(value) = value.as_str() {
            out.insert(key.clone(), value.to_owned());
        }
    }
    Ok(out)
}

fn scan_fingerprint(jar_len: u64, entries: &[(String, Vec<u8>)]) -> String {
    let mut hash = Fnv1a64::new();
    hash.update_u64(jar_len);
    for (name, bytes) in entries {
        hash.update_str(name);
        hash.update_u64(bytes.len() as u64);
        hash.update(bytes);
    }
    format!("{:016x}", hash.finish())
}

fn scan_mod_lang_data(mods_dir: &Path) -> Result<Vec<ModLangScan>, String> {
    let mut jars = Vec::new();
    if mods_dir.is_dir() {
        for entry in fs::read_dir(mods_dir).map_err(|e| format!("read mods directory: {e}"))? {
            let path = entry
                .map_err(|e| format!("read mods directory entry: {e}"))?
                .path();
            if path.extension().is_some_and(|ext| ext == "jar") && path.is_file() {
                jars.push(path);
            }
        }
    }
    jars.sort();
    let mut scans = Vec::new();
    for jar in jars {
        let jar_len = jar.metadata().map(|m| m.len()).unwrap_or(0);
        let selected = read_selected_entries(&jar, |name| {
            lang_namespace(name, "en_us.json").is_some()
                || lang_namespace(name, "ja_jp.json").is_some()
        });
        let (entries, report) = selected.map_err(|error| {
            format!(
                "{} cannot be scanned as a safe mod JAR: {error}",
                jar.display()
            )
        })?;
        if !report.skipped.is_empty() {
            let detail = report
                .skipped
                .iter()
                .map(|(name, reason)| format!("{name}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "{} lang entries could not be read safely: {detail}",
                jar.display()
            ));
        }
        let mut by_namespace: BTreeMap<String, LangPair> = BTreeMap::new();
        let mut fingerprint_entries = Vec::new();
        for entry in entries {
            if let Some(ns) = lang_namespace(&entry.name, "en_us.json") {
                by_namespace.entry(ns).or_default().0 = Some(entry.bytes.clone());
                fingerprint_entries.push((entry.name, entry.bytes));
            } else if let Some(ns) = lang_namespace(&entry.name, "ja_jp.json") {
                by_namespace.entry(ns).or_default().1 = Some(entry.bytes.clone());
                fingerprint_entries.push((entry.name, entry.bytes));
            }
        }
        fingerprint_entries.sort();
        let fingerprint = scan_fingerprint(jar_len, &fingerprint_entries);
        for (namespace, (en, ja)) in by_namespace {
            let Some(en_bytes) = en else {
                continue;
            };
            let en_us = parse_lang_json(&en_bytes, "mod en_us.json")?;
            let ja_jp = ja
                .map(|bytes| parse_lang_json(&bytes, "mod ja_jp.json"))
                .transpose()?
                .unwrap_or_default();
            scans.push(ModLangScan {
                mod_file: jar.clone(),
                namespace,
                fingerprint: fingerprint.clone(),
                en_us,
                ja_jp,
            });
        }
    }
    Ok(scans)
}

fn manifest_path(resourcepacks_dir: &Path) -> PathBuf {
    auto_translation_pack_path(resourcepacks_dir).join(AUTO_TRANSLATION_MANIFEST)
}

fn load_manifest_fingerprints(resourcepacks_dir: &Path) -> BTreeMap<String, String> {
    let Ok(bytes) = read_bounded_file(&manifest_path(resourcepacks_dir), MAX_LANG_JSON_BYTES)
    else {
        return BTreeMap::new();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return BTreeMap::new();
    };
    let Ok(json) = parse_json_with_limits(text, JSON_LIMITS, true) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(mods) = json.get("mods").and_then(JsonValue::as_array) {
        for entry in mods {
            if let (Some(file), Some(fingerprint)) =
                (entry.get_str("file"), entry.get_str("fingerprint"))
            {
                out.insert(file.to_owned(), fingerprint.to_owned());
            }
        }
    }
    out
}

fn untranslated_summaries(
    scans: &[ModLangScan],
    resourcepacks_dir: &Path,
) -> Vec<UntranslatedModSummary> {
    let previous = load_manifest_fingerprints(resourcepacks_dir);
    let mut out = Vec::new();
    for scan in scans {
        let file_name = scan
            .mod_file
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        if previous.get(&file_name) == Some(&scan.fingerprint) {
            continue;
        }
        let total_keys = scan.en_us.len();
        let already_ja_keys = scan
            .en_us
            .keys()
            .filter(|key| scan.ja_jp.contains_key(*key))
            .count();
        let missing_keys = total_keys.saturating_sub(already_ja_keys);
        if missing_keys > 0 {
            out.push(UntranslatedModSummary {
                mod_file: scan.mod_file.clone(),
                namespace: scan.namespace.clone(),
                total_keys,
                already_ja_keys,
                missing_keys,
                fingerprint: scan.fingerprint.clone(),
            });
        }
    }
    out
}

pub fn scan_untranslated_mods(
    mods_dir: &Path,
    resourcepacks_dir: &Path,
) -> Result<Vec<UntranslatedModSummary>, String> {
    Ok(untranslated_summaries(
        &scan_mod_lang_data(mods_dir)?,
        resourcepacks_dir,
    ))
}

pub fn protect_placeholders(input: &str) -> (String, Vec<String>) {
    let chars: Vec<char> = input.chars().collect();
    let mut placeholders = Vec::new();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let mut token_len = 0usize;
        if chars[i] == '§' && i + 1 < chars.len() {
            let c = chars[i + 1].to_ascii_lowercase();
            if c.is_ascii_digit() || matches!(c, 'a'..='f' | 'k'..='o' | 'r') {
                token_len = 2;
            }
        } else if chars[i] == '%' {
            if i + 1 < chars.len() && chars[i + 1] == '%' {
                token_len = 2;
            } else {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '$' {
                    j += 1;
                }
                while j < chars.len() && matches!(chars[j], '-' | '+' | ' ' | '0' | '#' | ',') {
                    j += 1;
                }
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == '.' {
                    j += 1;
                    while j < chars.len() && chars[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                if j < chars.len() && (chars[j].is_ascii_alphabetic() || chars[j] == '%') {
                    token_len = j + 1 - i;
                }
            }
        } else if chars[i] == '\\' {
            if i + 2 < chars.len() && chars[i + 1] == '\\' && chars[i + 2] == 'n' {
                token_len = 3;
            } else if i + 1 < chars.len() && matches!(chars[i + 1], 'n' | 't') {
                token_len = 2;
            }
        } else if matches!(chars[i], '\n' | '\t') {
            token_len = 1;
        }
        if token_len > 0 {
            let token: String = chars[i..i + token_len].iter().collect();
            out.push_str(&format!("⟦P{}⟧", placeholders.len()));
            placeholders.push(token);
            i += token_len;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    (out, placeholders)
}

pub fn restore_placeholders(translated: &str, placeholders: &[String]) -> Option<String> {
    let mut out = translated.to_owned();
    for (index, original) in placeholders.iter().enumerate() {
        let marker = format!("⟦P{index}⟧");
        if out.matches(&marker).count() != 1 {
            return None;
        }
        out = out.replacen(&marker, original, 1);
    }
    if out.contains('⟦') || out.contains('⟧') {
        return None;
    }
    Some(out)
}

fn parse_lang_key_values(protected: &[(String, String)]) -> String {
    let mut user = String::from(
        "Translate each numbered Minecraft string to Japanese. Return exactly the same number of numbered lines, no explanations, and preserve markers like ⟦P0⟧ verbatim.\n",
    );
    for (index, (_key, source)) in protected.iter().enumerate() {
        user.push_str(&format!("{}. {}\n", index + 1, source));
    }
    user
}

fn loopback_endpoint_valid(endpoint: &str) -> bool {
    let Some((scheme, authority)) = host_port(endpoint) else {
        return false;
    };
    if scheme != "http" {
        return false;
    }
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, ""));
    if !port.is_empty() && port.parse::<u16>().is_err() {
        return false;
    }
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn chat_completions_url(endpoint: &str) -> Result<String, String> {
    if !loopback_endpoint_valid(endpoint) {
        return Err("translation endpoint must be loopback HTTP (127.0.0.1/localhost)".into());
    }
    let base = endpoint.trim_end_matches('/');
    if base.ends_with("/v1/chat/completions") {
        Ok(base.to_owned())
    } else {
        Ok(format!("{base}/v1/chat/completions"))
    }
}

fn chat_request_body(batch: &[(String, String)]) -> String {
    let messages = vec![
        JsonValue::Object(vec![
            ("role".into(), JsonValue::String("system".into())),
            (
                "content".into(),
                JsonValue::String(
                    "[plamo-translate]\ntarget_lang=ja\n---\nMinecraft 1.21.1 mod UI/item/block names and tooltips. Keep game terms concise and natural in Japanese. Preserve markers like ⟦P0⟧ verbatim."
                        .into(),
                ),
            ),
        ]),
        JsonValue::Object(vec![
            ("role".into(), JsonValue::String("user".into())),
            ("content".into(), JsonValue::String(parse_lang_key_values(batch))),
        ]),
    ];
    JsonValue::Object(vec![
        (
            "model".into(),
            JsonValue::String("plamo-3-translate".into()),
        ),
        ("messages".into(), JsonValue::Array(messages)),
        ("temperature".into(), JsonValue::Number(0.0)),
    ])
    .to_json_string()
}

fn extract_chat_content(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "translation response is not UTF-8")?;
    let json = parse_json_with_limits(
        text,
        JsonLimits {
            max_depth: 64,
            max_nodes: 100_000,
            max_bytes: MAX_CHAT_RESPONSE_BYTES,
        },
        true,
    )
    .map_err(|e| format!("invalid translation response JSON: {e}"))?;
    json.get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get_str("content"))
        .map(str::to_owned)
        .ok_or_else(|| "translation response lacks choices[0].message.content".into())
}

fn parse_numbered_lines(content: &str, expected: usize) -> Option<Vec<String>> {
    let mut rows = Vec::new();
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut split = line.splitn(2, ['.', ':']);
        let index = split.next()?.trim().parse::<usize>().ok()?;
        let value = split.next()?.trim().to_owned();
        rows.push((index, value));
    }
    if rows.len() != expected {
        return None;
    }
    rows.sort_by_key(|(index, _)| *index);
    if rows
        .iter()
        .enumerate()
        .any(|(i, (index, _))| *index != i + 1)
    {
        return None;
    }
    Some(rows.into_iter().map(|(_, value)| value).collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BatchTranslationError {
    LineCountMismatch,
    Failed(String),
}

impl BatchTranslationError {
    fn failed(error: impl Into<String>) -> Self {
        Self::Failed(error.into())
    }
}

fn post_translation_batch(
    agent: &ureq::Agent,
    url: &str,
    batch: &[(String, String)],
) -> Result<Vec<String>, BatchTranslationError> {
    let body = chat_request_body(batch);
    let mut response = agent
        .post(url)
        .content_type("application/json; charset=utf-8")
        .send(body.as_bytes())
        .map_err(|e| {
            BatchTranslationError::failed(format!("PLaMo 3 translation request failed: {e}"))
        })?;
    if !response.status().is_success() {
        return Err(BatchTranslationError::failed(format!(
            "PLaMo 3 translation returned HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_CHAT_RESPONSE_BYTES as u64)
        .read_to_vec()
        .map_err(|e| BatchTranslationError::failed(format!("read PLaMo 3 response: {e}")))?;
    let content = extract_chat_content(&bytes).map_err(BatchTranslationError::Failed)?;
    parse_numbered_lines(&content, batch.len()).ok_or(BatchTranslationError::LineCountMismatch)
}

pub fn translate_entries_via_plamo3(
    endpoint: &str,
    entries: &[(String, String)],
    cancel: &AtomicBool,
    mut progress_cb: impl FnMut(usize, usize),
) -> Result<BTreeMap<String, String>, String> {
    let url = chat_completions_url(endpoint)?;
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(180)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .max_redirects(0)
        .build()
        .into();
    let total = entries.len();
    let mut completed = 0usize;
    let mut out = BTreeMap::new();
    for chunk in entries.chunks(TRANSLATION_BATCH_SIZE) {
        if cancel.load(Ordering::Relaxed) {
            return Err("translation cancelled".into());
        }
        let protected: Vec<(String, String)> = chunk
            .iter()
            .map(|(key, source)| (key.clone(), protect_placeholders(source).0))
            .collect();
        let rows = match post_translation_batch(&agent, &url, &protected) {
            Ok(rows) => rows,
            Err(BatchTranslationError::LineCountMismatch) if protected.len() > 1 => {
                let mut singles = Vec::new();
                for item in &protected {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("translation cancelled".into());
                    }
                    let one = match post_translation_batch(&agent, &url, std::slice::from_ref(item))
                    {
                        Ok(mut rows) => rows.pop().unwrap_or_else(|| item.1.clone()),
                        Err(BatchTranslationError::LineCountMismatch) => item.1.clone(),
                        Err(BatchTranslationError::Failed(error)) => return Err(error),
                    };
                    singles.push(one);
                }
                singles
            }
            Err(BatchTranslationError::LineCountMismatch) => {
                protected.iter().map(|(_, source)| source.clone()).collect()
            }
            Err(BatchTranslationError::Failed(error)) => return Err(error),
        };
        for ((key, source), translated) in chunk.iter().zip(rows) {
            let (_protected_source, placeholders) = protect_placeholders(source);
            let restored =
                restore_placeholders(&translated, &placeholders).unwrap_or_else(|| source.clone());
            out.insert(key.clone(), restored);
        }
        completed += chunk.len();
        progress_cb(completed, total);
    }
    Ok(out)
}

fn json_string_map(map: &BTreeMap<String, String>) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_json_string(&mut out, key);
        out.push(':');
        write_json_string(&mut out, value);
    }
    out.push('}');
    out
}

pub fn auto_translation_pack_path(resourcepacks_dir: &Path) -> PathBuf {
    resourcepacks_dir.join(AUTO_TRANSLATION_PACK_DIR)
}

pub fn auto_translation_pack_exists(resourcepacks_dir: &Path) -> bool {
    auto_translation_pack_path(resourcepacks_dir)
        .join("pack.mcmeta")
        .is_file()
}

fn load_auto_translation_pack_grouped(
    resourcepacks_dir: &Path,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let root = auto_translation_pack_path(resourcepacks_dir).join("assets");
    let mut out = BTreeMap::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for namespace in
        fs::read_dir(&root).map_err(|e| format!("read translation pack assets: {e}"))?
    {
        let namespace = namespace
            .map_err(|e| format!("read translation pack namespace: {e}"))?
            .path();
        let Some(ns_name) = namespace.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let lang = namespace.join("lang").join("ja_jp.json");
        if lang.is_file() {
            let bytes = read_bounded_file(&lang, MAX_LANG_JSON_BYTES)
                .map_err(|e| format!("read {}: {e}", lang.display()))?;
            out.insert(
                ns_name.to_owned(),
                parse_lang_json(&bytes, "auto translation ja_jp.json")?,
            );
        }
    }
    Ok(out)
}

pub fn load_auto_translation_pack_ja_jp(
    resourcepacks_dir: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (_namespace, entries) in load_auto_translation_pack_grouped(resourcepacks_dir)? {
        out.extend(entries);
    }
    Ok(out)
}

pub fn set_auto_translation_pack_enabled(enabled_packs: &mut Vec<String>, enabled: bool) {
    enabled_packs.retain(|pack| pack != AUTO_TRANSLATION_PACK_DIR);
    if enabled {
        enabled_packs.push(AUTO_TRANSLATION_PACK_DIR.to_owned());
    }
}

fn write_auto_translation_pack(
    resourcepacks_dir: &Path,
    translations: &BTreeMap<String, BTreeMap<String, String>>,
    preset: Plamo3Preset,
    scans: &[ModLangScan],
) -> Result<(), String> {
    let root = auto_translation_pack_path(resourcepacks_dir);
    fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    atomic_write(
        &root.join("pack.mcmeta"),
        br#"{"pack":{"pack_format":34,"description":"Megu Auto-Translated Mod Language Pack (ja_jp)"}}"#,
    )?;
    for (namespace, entries) in translations {
        if namespace.is_empty()
            || !namespace.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
            })
        {
            return Err("unsafe namespace in generated translation pack".into());
        }
        let path = root
            .join("assets")
            .join(namespace)
            .join("lang")
            .join("ja_jp.json");
        atomic_write(&path, json_string_map(entries).as_bytes())?;
    }
    let mut seen = BTreeSet::new();
    let mods: Vec<JsonValue> = scans
        .iter()
        .filter_map(|scan| {
            let file = scan.mod_file.file_name()?.to_str()?.to_owned();
            if !seen.insert((file.clone(), scan.namespace.clone())) {
                return None;
            }
            Some(JsonValue::Object(vec![
                ("file".into(), JsonValue::String(file)),
                (
                    "namespace".into(),
                    JsonValue::String(scan.namespace.clone()),
                ),
                (
                    "fingerprint".into(),
                    JsonValue::String(scan.fingerprint.clone()),
                ),
                (
                    "key_count".into(),
                    JsonValue::Number(scan.en_us.len() as f64),
                ),
            ]))
        })
        .collect();
    let manifest = JsonValue::Object(vec![
        ("format".into(), JsonValue::Number(1.0)),
        (
            "preset".into(),
            JsonValue::String(preset.manifest_id().into()),
        ),
        ("target_lang".into(), JsonValue::String("ja_jp".into())),
        ("mods".into(), JsonValue::Array(mods)),
    ]);
    atomic_write(
        &root.join(AUTO_TRANSLATION_MANIFEST),
        manifest.to_json_pretty().as_bytes(),
    )
}

pub fn translate_mods_against_endpoint(
    mods_dir: &Path,
    resourcepacks_dir: &Path,
    endpoint: &str,
    preset: Plamo3Preset,
    cancel: &AtomicBool,
    mut progress_cb: impl FnMut(usize, usize),
) -> Result<TranslationRunSummary, String> {
    let scans = scan_mod_lang_data(mods_dir)?;
    let pending = untranslated_summaries(&scans, resourcepacks_dir);
    let pending_keys: BTreeSet<(PathBuf, String)> = pending
        .iter()
        .map(|summary| (summary.mod_file.clone(), summary.namespace.clone()))
        .collect();
    let scans_to_translate: Vec<ModLangScan> = scans
        .iter()
        .filter(|scan| pending_keys.contains(&(scan.mod_file.clone(), scan.namespace.clone())))
        .cloned()
        .collect();
    let mut new_translations: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut entries = Vec::new();
    for scan in &scans_to_translate {
        for (key, source) in &scan.en_us {
            if !scan.ja_jp.contains_key(key) {
                entries.push((format!("{}\u{1f}{key}", scan.namespace), source.clone()));
            }
        }
    }
    let translated_flat = if entries.is_empty() {
        BTreeMap::new()
    } else {
        translate_entries_via_plamo3(endpoint, &entries, cancel, &mut progress_cb)?
    };
    for (compound, translated) in translated_flat {
        if let Some((namespace, key)) = compound.split_once('\u{1f}') {
            new_translations
                .entry(namespace.to_owned())
                .or_default()
                .insert(key.to_owned(), translated);
        }
    }
    let mut all_translations: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    all_translations.extend(load_auto_translation_pack_grouped(resourcepacks_dir)?);
    for (namespace, values) in new_translations {
        all_translations
            .entry(namespace)
            .or_default()
            .extend(values);
    }
    if !scans_to_translate.is_empty() {
        let mut pack_scans = scans_to_translate.clone();
        let old_fps = load_manifest_fingerprints(resourcepacks_dir);
        pack_scans.extend(
            scans
                .iter()
                .filter(|scan| {
                    scan.mod_file
                        .file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|name| old_fps.get(name) == Some(&scan.fingerprint))
                })
                .cloned(),
        );
        write_auto_translation_pack(resourcepacks_dir, &all_translations, preset, &pack_scans)?;
    }
    Ok(TranslationRunSummary {
        scanned_mods: scans.len(),
        translated_namespaces: scans_to_translate.len(),
        translated_entries: entries.len(),
        output_pack: auto_translation_pack_path(resourcepacks_dir),
    })
}

pub fn translate_mods_with_plamo3(
    cache_dir: &Path,
    mods_dir: &Path,
    resourcepacks_dir: &Path,
    preset: Plamo3Preset,
    cancel: &AtomicBool,
    progress_cb: impl FnMut(usize, usize),
) -> Result<TranslationRunSummary, String> {
    if let Ok(endpoint) = env::var("MEGU_TRANSLATE_ENDPOINT") {
        if !endpoint.is_empty() {
            return translate_mods_against_endpoint(
                mods_dir,
                resourcepacks_dir,
                &endpoint,
                preset,
                cancel,
                progress_cb,
            );
        }
    }
    let server = TransientLlamaServer::spawn(cache_dir, preset, cancel)?;
    translate_mods_against_endpoint(
        mods_dir,
        resourcepacks_dir,
        server.endpoint(),
        preset,
        cancel,
        progress_cb,
    )
}
