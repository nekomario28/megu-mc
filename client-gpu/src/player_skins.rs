// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded, asynchronous lookup and caching for official Mojang player textures.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};

use crate::vanilla_bootstrap::{open_regular_file, CacheDirectory, StagedFile};
use crate::vanilla_jar::{decode_asset_png, png_dimensions};

const MAX_DISK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DISK_ENTRIES: usize = 16_384;
const MAX_JSON: usize = 64 * 1024;
const MAX_PNG: usize = 256 * 1024;
const MAX_SKINS: usize = 256;
const MAX_TEXTURE_DIMENSION: u32 = 512;
const JSON_LIMITS: JsonLimits = JsonLimits {
    max_depth: 24,
    max_nodes: 4096,
    max_bytes: MAX_JSON,
};

/// Where the resolved texture data came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkinSource {
    /// Loaded from the on-disk Mojang texture cache.
    DiskCache,
    /// Fetched from the official Mojang texture host.
    Mojang,
    /// Vanilla UUID-derived default. The renderer can resolve this name from
    /// its built-in skin atlas when `skin_rgba` is empty.
    VanillaDefault { name: &'static str },
}

/// Decoded player textures. Default skins have empty RGBA data and a zero
/// size, allowing the renderer to use its bundled vanilla atlas entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPlayerSkin {
    pub uuid: [u8; 16],
    pub skin_rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub slim: bool,
    pub cape_rgba: Option<(u32, u32, Vec<u8>)>,
    pub source: SkinSource,
}

/// Parse the base64 JSON `textures` property value, or a sessionserver profile
/// JSON document containing `properties[name="textures"].value`.
///
/// Texture URLs are normalized to HTTPS and strictly constrained to
/// `textures.minecraft.net`; a profile document may only be supplied by the
/// fixed sessionserver lookup performed by this module.
pub fn parse_textures_property(input: &str) -> Result<ParsedPlayerTextures, String> {
    if input.len() > MAX_JSON * 2 {
        return Err("textures property exceeds size limit".into());
    }
    let property = if input.trim_start().starts_with('{') {
        let profile = parse_skin_json(input)?;
        profile
            .get("properties")
            .and_then(JsonValue::as_array)
            .and_then(|properties| {
                properties
                    .iter()
                    .find(|property| property.get_str("name") == Some("textures"))
            })
            .and_then(|property| property.get_str("value"))
            .ok_or_else(|| "profile has no textures property".to_owned())?
            .to_owned()
    } else {
        input.to_owned()
    };
    let decoded = decode_base64(&property)?;
    if decoded.len() > MAX_JSON {
        return Err("decoded textures JSON exceeds size limit".into());
    }
    let json_text = std::str::from_utf8(&decoded).map_err(|_| "textures value is not UTF-8")?;
    let json = parse_skin_json(json_text)?;
    let textures = json
        .get("textures")
        .ok_or_else(|| "textures JSON has no textures object".to_owned())?;
    let skin = textures
        .get("SKIN")
        .ok_or_else(|| "textures JSON has no SKIN entry".to_owned())?;
    let skin_url = skin
        .get_str("url")
        .ok_or_else(|| "SKIN entry has no URL".to_owned())?;
    let skin_url = normalize_texture_url(skin_url)?;
    let slim = skin
        .get("metadata")
        .and_then(|metadata| metadata.get_str("model"))
        == Some("slim");
    let cape = textures
        .get("CAPE")
        .and_then(|entry| entry.get_str("url"))
        .map(normalize_texture_url)
        .transpose()?;
    Ok(ParsedPlayerTextures {
        skin_url,
        cape_url: cape,
        slim,
    })
}

/// Public parsed URL result for callers that already have a player-info property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedPlayerTextures {
    pub skin_url: String,
    pub cape_url: Option<String>,
    pub slim: bool,
}

fn parse_skin_json(text: &str) -> Result<JsonValue, String> {
    parse_json_with_limits(text, JSON_LIMITS, true).map_err(|error| error.to_string())
}

fn normalize_texture_url(url: &str) -> Result<String, String> {
    let https = if let Some(rest) = url.strip_prefix("http://textures.minecraft.net/") {
        format!("https://textures.minecraft.net/{rest}")
    } else {
        url.to_owned()
    };
    let rest = https
        .strip_prefix("https://textures.minecraft.net/")
        .ok_or_else(|| "texture URL must use the allowlisted HTTPS host".to_owned())?;
    if rest.is_empty() || rest.contains(['?', '#', '@', '\\', '\0']) {
        return Err("malformed texture URL".into());
    }
    let (path, hash) = rest
        .rsplit_once('/')
        .ok_or("texture URL has no hash path")?;
    if path.is_empty()
        || !valid_hash(hash)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("malformed texture URL or hash".into());
    }
    Ok(https)
}

fn valid_hash(hash: &str) -> bool {
    (16..=128).contains(&hash.len())
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn texture_hash(url: &str) -> Result<String, String> {
    let normalized = normalize_texture_url(url)?;
    normalized
        .rsplit('/')
        .next()
        .filter(|hash| valid_hash(hash))
        .map(str::to_owned)
        .ok_or_else(|| "invalid texture hash".into())
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_JSON * 2 || !bytes.len().is_multiple_of(4) {
        return Err("invalid base64 length".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let a = b64_value(chunk[0]).ok_or("invalid base64 character")?;
        let b = b64_value(chunk[1]).ok_or("invalid base64 character")?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            b64_value(chunk[2]).ok_or("invalid base64 character")?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            b64_value(chunk[3]).ok_or("invalid base64 character")?
        };
        if (!last && (chunk[2] == b'=' || chunk[3] == b'='))
            || (chunk[2] == b'=' && chunk[3] != b'=')
            || (chunk[2] == b'=' && b & 0x0f != 0)
            || (chunk[3] == b'=' && chunk[2] != b'=' && c & 0x03 != 0)
        {
            return Err("invalid base64 padding".into());
        }
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

fn b64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Upgrade supported player skin layouts to their renderer-ready form.
/// Legacy 64×32 skins gain a transparent lower half and copy the legacy
/// right-leg/right-arm regions into their modern left-limb locations.
pub fn normalize_skin_to_64x64(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(u32, u32, Vec<u8>), String> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("skin dimensions overflow")?;
    if expected != rgba.len() {
        return Err("skin RGBA length does not match dimensions".into());
    }
    if width == 64 && height == 64 {
        return Ok((width, height, rgba.to_vec()));
    }
    if width == 64 && height == 32 {
        let mut out = vec![0; 64 * 64 * 4];
        out[..rgba.len()].copy_from_slice(rgba);
        copy_legacy_limb(&mut out, rgba, 0, 16);
        copy_legacy_limb(&mut out, rgba, 40, 32);
        return Ok((64, 64, out));
    }
    if valid_skin_geometry(width, height) {
        return Ok((width, height, rgba.to_vec()));
    }
    Err("unsupported skin dimensions".into())
}

fn valid_skin_geometry(width: u32, height: u32) -> bool {
    (64..=MAX_TEXTURE_DIMENSION).contains(&width)
        && width.is_power_of_two()
        && (height == width || height == width / 2)
}

fn copy_legacy_limb(dst: &mut [u8], src: &[u8], source_x: usize, target_x: usize) {
    for row in 0..16 {
        let source = ((16 + row) * 64 + source_x) * 4;
        let target = ((48 + row) * 64 + target_x) * 4;
        dst[target..target + 64].copy_from_slice(&src[source..source + 64]);
    }
}

struct SkinRequest {
    uuid: [u8; 16],
    textures_property: Option<String>,
}

struct SkinCompletion {
    uuid: [u8; 16],
    skin: ResolvedPlayerSkin,
}

/// Asynchronous official texture lookup with bounded completed-memory storage.
/// `request_skin_for_uuid` and `poll_completed` never wait for the worker.
pub struct PlayerSkinCache {
    assets_dir: PathBuf,
    request_tx: SyncSender<SkinRequest>,
    completion_rx: Receiver<SkinCompletion>,
    resolved: HashMap<[u8; 16], ResolvedPlayerSkin>,
    order: VecDeque<[u8; 16]>,
    pending: HashSet<[u8; 16]>,
}

impl PlayerSkinCache {
    pub fn new(assets_dir: impl Into<PathBuf>) -> Self {
        let assets_dir = assets_dir.into();
        let (request_tx, request_rx) = mpsc::sync_channel::<SkinRequest>(MAX_SKINS);
        let (completion_tx, completion_rx) = mpsc::sync_channel::<SkinCompletion>(MAX_SKINS);
        let worker_assets = assets_dir.clone();
        let _ = thread::Builder::new()
            .name("megu-player-skins".into())
            .spawn(move || skin_worker(&worker_assets, request_rx, completion_tx));
        Self {
            assets_dir,
            request_tx,
            completion_rx,
            resolved: HashMap::new(),
            order: VecDeque::new(),
            pending: HashSet::new(),
        }
    }

    /// Queue a lookup. If a textures property is provided it is used directly;
    /// otherwise the worker performs the official UUID profile lookup.
    pub fn request_skin_for_uuid(&mut self, uuid: [u8; 16], textures_property: Option<&str>) {
        if self.resolved.contains_key(&uuid) || !self.pending.insert(uuid) {
            return;
        }
        let request = SkinRequest {
            uuid,
            textures_property: textures_property.map(str::to_owned),
        };
        if self.request_tx.try_send(request).is_err() {
            self.pending.remove(&uuid);
            self.insert(fallback_skin(uuid));
        }
    }

    /// Drain ready worker results without blocking.
    pub fn poll_completed(&mut self) {
        while let Ok(completion) = self.completion_rx.try_recv() {
            self.pending.remove(&completion.uuid);
            self.insert(completion.skin);
        }
    }

    pub fn get_skin(&self, uuid: [u8; 16]) -> Option<&ResolvedPlayerSkin> {
        self.resolved.get(&uuid)
    }

    /// The validated on-disk location for a texture URL, useful for cache
    /// inspection and deterministic offline cache seeding.
    pub fn disk_cache_path(&self, texture_url: &str) -> Result<PathBuf, String> {
        let hash = texture_hash(texture_url)?;
        Ok(self.assets_dir.join("skins").join(&hash[..2]).join(hash))
    }

    fn insert(&mut self, skin: ResolvedPlayerSkin) {
        let uuid = skin.uuid;
        if self.resolved.contains_key(&uuid) {
            self.order.retain(|item| *item != uuid);
        }
        self.resolved.insert(uuid, skin);
        self.order.push_back(uuid);
        while self.resolved.len() > MAX_SKINS {
            if let Some(oldest) = self.order.pop_front() {
                self.resolved.remove(&oldest);
            }
        }
    }
}

fn skin_worker(
    assets_dir: &Path,
    request_rx: Receiver<SkinRequest>,
    completion_tx: SyncSender<SkinCompletion>,
) {
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(5)))
        .timeout_recv_body(Some(Duration::from_secs(5)))
        .max_redirects(0)
        .build()
        .into();
    while let Ok(request) = request_rx.recv() {
        let skin = resolve_request(assets_dir, &agent, &request)
            .unwrap_or_else(|_| fallback_skin(request.uuid));
        if completion_tx
            .send(SkinCompletion {
                uuid: request.uuid,
                skin,
            })
            .is_err()
        {
            break;
        }
    }
}

fn resolve_request(
    assets_dir: &Path,
    agent: &ureq::Agent,
    request: &SkinRequest,
) -> Result<ResolvedPlayerSkin, String> {
    let textures = match &request.textures_property {
        Some(property) => parse_textures_property(property)?,
        None => {
            let profile = fetch_bounded(
                agent,
                &format!(
                    "https://sessionserver.mojang.com/session/minecraft/profile/{}?unsigned=false",
                    uuid_hex(request.uuid)
                ),
                MAX_JSON,
            )?;
            let profile_text =
                std::str::from_utf8(&profile).map_err(|_| "profile response is not UTF-8")?;
            parse_textures_property(profile_text)?
        }
    };
    let cache = CacheDirectory::open(assets_dir)
        .and_then(|root| root.child("skins"))
        .ok();
    let ((width, height, rgba), from_cache) = load_or_fetch(
        agent,
        cache.as_ref(),
        &textures.skin_url,
        valid_skin_geometry,
    )?;
    let (width, height, skin_rgba) = if (width, height) == (64, 32) {
        normalize_skin_to_64x64(width, height, &rgba)?
    } else {
        (width, height, rgba)
    };
    let cape_rgba = textures
        .cape_url
        .as_deref()
        .map(|url| {
            load_or_fetch(agent, cache.as_ref(), url, |w, h| {
                w <= MAX_TEXTURE_DIMENSION && h <= MAX_TEXTURE_DIMENSION
            })
            .map(|(image, _)| image)
        })
        .transpose()?;
    Ok(ResolvedPlayerSkin {
        uuid: request.uuid,
        skin_rgba,
        width,
        height,
        slim: textures.slim,
        cape_rgba,
        source: if from_cache {
            SkinSource::DiskCache
        } else {
            SkinSource::Mojang
        },
    })
}

fn fetch_bounded(agent: &ureq::Agent, url: &str, cap: usize) -> Result<Vec<u8>, String> {
    if !allowed_https_url(url) {
        return Err("refusing URL outside official Mojang hosts".into());
    }
    let mut response = agent.get(url).call().map_err(|error| error.to_string())?;
    response
        .body_mut()
        .with_config()
        .limit(cap as u64)
        .read_to_vec()
        .map_err(|error| error.to_string())
}

fn allowed_https_url(url: &str) -> bool {
    (url.starts_with("https://textures.minecraft.net/") && normalize_texture_url(url).is_ok())
        || (url.starts_with("https://sessionserver.mojang.com/session/minecraft/profile/")
            && url
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~:/?=".contains(&byte)))
}

type DecodedTexture = (u32, u32, Vec<u8>);

fn load_or_fetch(
    agent: &ureq::Agent,
    cache: Option<&CacheDirectory>,
    url: &str,
    geometry: impl Fn(u32, u32) -> bool,
) -> Result<(DecodedTexture, bool), String> {
    let hash = texture_hash(url)?;
    let prefix = cache.and_then(|root| root.child(&hash[..2]).ok());
    let path = prefix.as_ref().map(|dir| dir.file(&hash));
    let decode = |bytes: &[u8]| {
        let (width, height) = png_dimensions(bytes).ok_or("invalid texture PNG header")?;
        if !geometry(width, height) {
            return Err("unsupported texture dimensions".into());
        }
        decode_asset_png(bytes)
    };
    if let Some(image) = path
        .as_deref()
        .and_then(read_disk_png)
        .and_then(|bytes| decode(&bytes).ok())
    {
        return Ok((image, true));
    }
    let bytes = fetch_bounded(agent, url, MAX_PNG)?;
    let image = decode(&bytes)?;
    if let (Some(root), Some(path)) = (cache, path) {
        // Caching is optional. A full, busy or unwritable cache must not
        // discard a texture that was successfully fetched and validated.
        let _ = store_disk_png(root, &path, &hash, &bytes);
    }
    Ok((image, false))
}

fn read_disk_png(path: &Path) -> Option<Vec<u8>> {
    let file = open_regular_file(path).ok()?;
    if file.metadata().ok()?.len() > MAX_PNG as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_PNG as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= MAX_PNG).then_some(bytes)
}

fn store_disk_png(cache: &CacheDirectory, path: &Path, hash: &str, bytes: &[u8]) -> io::Result<()> {
    use std::fs::OpenOptions;
    let lock_path = cache.file(".quota-lock");
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Windows needs a writable handle for an exclusive lock.
            if !fs::symlink_metadata(&lock_path)?.is_file() {
                return Err(io::Error::other("invalid skin cache lock"));
            }
            OpenOptions::new().read(true).write(true).open(&lock_path)?
        }
        Err(e) => return Err(e),
    };
    lock.try_lock().map_err(io::Error::from)?;
    let mut used = bytes.len() as u64;
    let mut entries = Vec::new();
    let mut scanned = 0;
    for index in 0..=255 {
        let prefix = format!("{index:02x}");
        let dir = cache.file(&prefix);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            scanned += 1;
            if scanned > MAX_DISK_ENTRIES {
                return Err(io::Error::other("skin cache entry limit exceeded"));
            }
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| {
                (name.starts_with(&prefix) && valid_hash(name))
                    || (name.starts_with(".megu-") && name.ends_with(".part"))
            }) else {
                continue;
            };
            let meta = fs::symlink_metadata(entry.path())?;
            if !meta.is_file() || name == hash {
                continue;
            }
            used = used.saturating_add(meta.len());
            entries.push((
                meta.modified().ok(),
                prefix.clone(),
                name.to_owned(),
                meta.len(),
            ));
        }
    }
    entries.sort_unstable();
    for (_, prefix, name, size) in entries {
        if used <= MAX_DISK_BYTES {
            break;
        }
        let dir = cache.child(&prefix)?;
        fs::remove_file(dir.file(&name))?;
        used = used.saturating_sub(size);
    }
    let mut staged = StagedFile::new(path)?;
    staged.file().write_all(bytes)?;
    staged.install(path)
}

fn fallback_skin(uuid: [u8; 16]) -> ResolvedPlayerSkin {
    let (name, slim) = crate::vanilla_official_extras::default_skin_for_uuid(uuid);
    ResolvedPlayerSkin {
        uuid,
        skin_rgba: Vec::new(),
        width: 0,
        height: 0,
        slim,
        cape_rgba: None,
        source: SkinSource::VanillaDefault { name },
    }
}

fn uuid_hex(uuid: [u8; 16]) -> String {
    let mut output = String::with_capacity(32);
    for byte in uuid {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skin_cache_quota_evicts_old_entries_before_admitting_new_bytes() {
        let root = std::env::temp_dir().join(format!("megu-skin-quota-{}", std::process::id()));
        let cache = CacheDirectory::open(&root).unwrap();
        let first = cache.child("00").unwrap();
        let old_hash = "0".repeat(64);
        let old_path = first.file(&old_hash);
        let old = fs::File::create(&old_path).unwrap();
        old.set_len(MAX_DISK_BYTES).unwrap();
        drop(old);
        let second = cache.child("11").unwrap();
        let new_hash = "1".repeat(64);
        let new_path = second.file(&new_hash);
        store_disk_png(&cache, &new_path, &new_hash, b"texture").unwrap();
        assert!(!old_path.exists());
        assert_eq!(
            read_disk_png(&new_path).as_deref(),
            Some(b"texture".as_slice())
        );
        fs::remove_dir_all(root).unwrap();
    }
}
