// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded, asynchronous lookup and caching for official Mojang player textures.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

use megu_client_mesh::png_decode::decode_png_rgba;
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};

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
    if bytes.is_empty() || bytes.len() > MAX_JSON * 2 || bytes.len() % 4 != 0 {
        return Err("invalid base64 length".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
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
        copy_rect(&mut out, 64, rgba, 64, 0, 16, 16, 16, 16, 48);
        copy_rect(&mut out, 64, rgba, 64, 40, 16, 16, 16, 32, 48);
        return Ok((64, 64, out));
    }
    let valid_hd = width >= 128
        && width <= MAX_TEXTURE_DIMENSION
        && (width == height || width == height.saturating_mul(2))
        && width.is_power_of_two()
        && (height == width || height == width / 2);
    if valid_hd {
        return Ok((width, height, rgba.to_vec()));
    }
    Err("unsupported skin dimensions".into())
}

fn copy_rect(
    dst: &mut [u8],
    dst_width: usize,
    src: &[u8],
    src_width: usize,
    sx: usize,
    sy: usize,
    width: usize,
    height: usize,
    dx: usize,
    dy: usize,
) {
    for row in 0..height {
        let source = ((sy + row) * src_width + sx) * 4;
        let target = ((dy + row) * dst_width + dx) * 4;
        let bytes = width * 4;
        dst[target..target + bytes].copy_from_slice(&src[source..source + bytes]);
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
        loop {
            match self.completion_rx.try_recv() {
                Ok(completion) => {
                    self.pending.remove(&completion.uuid);
                    self.insert(completion.skin);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
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
    let skin_path = cache_path(assets_dir, &textures.skin_url)?;
    let (skin_png, from_cache) = load_or_fetch(agent, &skin_path, &textures.skin_url, MAX_PNG)?;
    let (width, height, rgba) = decode_png_rgba(&skin_png).map_err(|error| error.to_string())?;
    let (width, height, skin_rgba) = normalize_skin_to_64x64(width, height, &rgba)?;
    let cape_rgba = if let Some(url) = textures.cape_url.as_deref() {
        let path = cache_path(assets_dir, url)?;
        let (png, _) = load_or_fetch(agent, &path, url, MAX_PNG)?;
        let (w, h, pixels) = decode_png_rgba(&png).map_err(|error| error.to_string())?;
        if w == 0
            || h == 0
            || w > MAX_TEXTURE_DIMENSION
            || h > MAX_TEXTURE_DIMENSION
            || pixels.len() != w as usize * h as usize * 4
        {
            return Err("invalid cape dimensions".into());
        }
        Some((w, h, pixels))
    } else {
        None
    };
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

fn cache_path(assets_dir: &Path, url: &str) -> Result<PathBuf, String> {
    let hash = texture_hash(url)?;
    Ok(assets_dir.join("skins").join(&hash[..2]).join(hash))
}

fn load_or_fetch(
    agent: &ureq::Agent,
    path: &Path,
    url: &str,
    cap: usize,
) -> Result<(Vec<u8>, bool), String> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() <= cap && decode_png_rgba(&bytes).is_ok() {
            return Ok((bytes, true));
        }
    }
    let bytes = fetch_bounded(agent, url, cap)?;
    decode_png_rgba(&bytes).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension(format!("{}.part", std::process::id()));
    fs::write(&temporary, &bytes).map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        error.to_string()
    })?;
    Ok((bytes, false))
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
