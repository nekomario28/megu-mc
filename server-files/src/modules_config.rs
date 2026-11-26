// SPDX-License-Identifier: LGPL-3.0-or-later
//! Strict, deterministic configuration and integrity pins for opt-in server modules.

use crate::atomic::write_atomic;
use megu_core::json::{parse_json, JsonValue};
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Component, Path};

pub const MODULES_CONFIG_FILE: &str = "megu-modules.toml";
pub const MOD_LOCKFILE: &str = "megu-mods.lock.json";
const SCHEMA_VERSION: u16 = 1;

/// Optional official server modules. Every module stays disabled by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModulesConfig {
    pub schema_version: u16,
    pub container_protection: bool,
    pub town_claims: bool,
    pub delta_audit_rollback: bool,
    pub role_permissions: bool,
    pub homes_warps_economy: bool,
    pub live_web_map: bool,
    pub region_builder: bool,
}

impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            container_protection: false,
            town_claims: false,
            delta_audit_rollback: false,
            role_permissions: false,
            homes_warps_economy: false,
            live_web_map: false,
            region_builder: false,
        }
    }
}

impl ModulesConfig {
    /// Deterministic TOML with every switch made explicit.
    pub fn to_toml_string(&self) -> String {
        format!(
            "schema_version = {}\ncontainer_protection = {}\ntown_claims = {}\ndelta_audit_rollback = {}\nrole_permissions = {}\nhomes_warps_economy = {}\nlive_web_map = {}\nregion_builder = {}\n",
            self.schema_version,
            self.container_protection,
            self.town_claims,
            self.delta_audit_rollback,
            self.role_permissions,
            self.homes_warps_economy,
            self.live_web_map,
            self.region_builder
        )
    }

    pub fn save(&self, path: &Path) -> Result<(), LockfileError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(LockfileError::UnsupportedSchema(self.schema_version));
        }
        write_atomic(path, self.to_toml_string().as_bytes()).map_err(LockfileError::Io)
    }

    pub fn load(path: &Path) -> Result<Self, LockfileError> {
        let text = std::fs::read_to_string(path).map_err(LockfileError::Io)?;
        parse_modules_toml(&text)
    }
}

/// Parse the deliberately small, strict TOML subset used by `megu-modules.toml`.
pub fn parse_modules_toml(input: &str) -> Result<ModulesConfig, LockfileError> {
    let mut config = ModulesConfig::default();
    let mut seen = BTreeSet::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line = raw_line.split_once('#').map_or(raw_line, |(head, _)| head).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            LockfileError::InvalidToml(format!("line {}: expected key = value", index + 1))
        })?;
        let key = key.trim();
        let value = value.trim();
        if !seen.insert(key.to_owned()) {
            return Err(LockfileError::InvalidToml(format!(
                "line {}: duplicate key {key}",
                index + 1
            )));
        }
        match key {
            "schema_version" => {
                let version = value.parse::<u16>().map_err(|_| {
                    LockfileError::InvalidToml(format!("line {}: invalid schema_version", index + 1))
                })?;
                if version != SCHEMA_VERSION {
                    return Err(LockfileError::UnsupportedSchema(version));
                }
                config.schema_version = version;
            }
            "container_protection" => config.container_protection = parse_bool(key, value, index)?,
            "town_claims" => config.town_claims = parse_bool(key, value, index)?,
            "delta_audit_rollback" => config.delta_audit_rollback = parse_bool(key, value, index)?,
            "role_permissions" => config.role_permissions = parse_bool(key, value, index)?,
            "homes_warps_economy" => config.homes_warps_economy = parse_bool(key, value, index)?,
            "live_web_map" => config.live_web_map = parse_bool(key, value, index)?,
            "region_builder" => config.region_builder = parse_bool(key, value, index)?,
            _ => return Err(LockfileError::UnknownKey(key.to_owned())),
        }
    }
    if !seen.contains("schema_version") {
        return Err(LockfileError::MissingField("schema_version"));
    }
    Ok(config)
}

fn parse_bool(key: &str, value: &str, line: usize) -> Result<bool, LockfileError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(LockfileError::InvalidToml(format!(
            "line {}: {key} must be true or false",
            line + 1
        ))),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModLockEntry {
    pub mod_id: String,
    pub version: String,
    pub filename: String,
    pub sha512_hex: String,
    pub source_kind: String,
    pub worldgen_epoch: Option<String>,
    pub required_capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModLockfile {
    pub schema_version: u16,
    pub entries: Vec<ModLockEntry>,
}

impl ModLockfile {
    pub fn new(entries: Vec<ModLockEntry>) -> Result<Self, LockfileError> {
        let mut lockfile = Self { schema_version: SCHEMA_VERSION, entries };
        lockfile.canonicalize_and_validate()?;
        Ok(lockfile)
    }

    pub fn parse(input: &str) -> Result<Self, LockfileError> {
        let root = parse_json(input).map_err(LockfileError::Json)?;
        ensure_known_fields(&root, &["schema_version", "entries"])?;
        let schema_version = required_u16(&root, "schema_version")?;
        if schema_version != SCHEMA_VERSION {
            return Err(LockfileError::UnsupportedSchema(schema_version));
        }
        let values = root.get("entries").and_then(JsonValue::as_array)
            .ok_or(LockfileError::WrongType("entries"))?;
        let mut entries = Vec::with_capacity(values.len());
        for value in values {
            ensure_known_fields(value, &["mod_id", "version", "filename", "sha512_hex", "source_kind", "worldgen_epoch", "required_capabilities"])?;
            let worldgen_epoch = match value.get("worldgen_epoch") {
                None | Some(JsonValue::Null) => None,
                Some(JsonValue::String(v)) => Some(v.clone()),
                Some(_) => return Err(LockfileError::WrongType("worldgen_epoch")),
            };
            let caps = value.get("required_capabilities").and_then(JsonValue::as_array)
                .ok_or(LockfileError::WrongType("required_capabilities"))?;
            let required_capabilities = caps.iter().map(|cap| {
                cap.as_str().map(str::to_owned).ok_or(LockfileError::WrongType("required_capabilities[]"))
            }).collect::<Result<Vec<_>, _>>()?;
            entries.push(ModLockEntry {
                mod_id: required_string(value, "mod_id")?,
                version: required_string(value, "version")?,
                filename: required_string(value, "filename")?,
                sha512_hex: required_string(value, "sha512_hex")?,
                source_kind: required_string(value, "source_kind")?,
                worldgen_epoch,
                required_capabilities,
            });
        }
        let mut lockfile = Self { schema_version, entries };
        lockfile.canonicalize_and_validate()?;
        Ok(lockfile)
    }

    pub fn to_json_string(&self) -> Result<String, LockfileError> {
        let mut canonical = self.clone();
        canonical.canonicalize_and_validate()?;
        let entries = canonical.entries.into_iter().map(|mut entry| {
            entry.required_capabilities.sort();
            JsonValue::Object(vec![
                ("mod_id".to_owned(), JsonValue::String(entry.mod_id)),
                ("version".to_owned(), JsonValue::String(entry.version)),
                ("filename".to_owned(), JsonValue::String(entry.filename)),
                ("sha512_hex".to_owned(), JsonValue::String(entry.sha512_hex)),
                ("source_kind".to_owned(), JsonValue::String(entry.source_kind)),
                ("worldgen_epoch".to_owned(), entry.worldgen_epoch.map_or(JsonValue::Null, JsonValue::String)),
                ("required_capabilities".to_owned(), JsonValue::Array(entry.required_capabilities.into_iter().map(JsonValue::String).collect())),
            ])
        }).collect();
        Ok(JsonValue::Object(vec![
            ("schema_version".to_owned(), JsonValue::Number(f64::from(canonical.schema_version))),
            ("entries".to_owned(), JsonValue::Array(entries)),
        ]).to_json_string())
    }

    pub fn save(&self, path: &Path) -> Result<(), LockfileError> {
        write_atomic(path, self.to_json_string()?.as_bytes()).map_err(LockfileError::Io)
    }

    pub fn load(path: &Path) -> Result<Self, LockfileError> {
        Self::parse(&std::fs::read_to_string(path).map_err(LockfileError::Io)?)
    }

    fn canonicalize_and_validate(&mut self) -> Result<(), LockfileError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(LockfileError::UnsupportedSchema(self.schema_version));
        }
        let mut ids = BTreeSet::new();
        for entry in &mut self.entries {
            if entry.mod_id.is_empty() || !ids.insert(entry.mod_id.clone()) {
                return Err(LockfileError::DuplicateModId(entry.mod_id.clone()));
            }
            if !is_hex_digest(&entry.sha512_hex, 128) {
                return Err(LockfileError::InvalidSha512(entry.mod_id.clone()));
            }
            validate_filename(&entry.filename)?;
            entry.required_capabilities.sort();
            if entry.required_capabilities.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(LockfileError::DuplicateCapability(entry.mod_id.clone()));
            }
        }
        self.entries.sort_by(|a, b| a.mod_id.cmp(&b.mod_id));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModDownloadPolicy {
    pub offline_only: bool,
}

impl ModDownloadPolicy {
    /// Fail closed before any remote request when explicitly offline or env-guarded.
    pub fn verify_download_allowed(&self) -> Result<(), LockfileError> {
        if self.offline_only || std::env::var("MEGU_DISABLE_MOD_DOWNLOAD").as_deref() == Ok("1") {
            Err(LockfileError::DownloadsDisabled)
        } else {
            Ok(())
        }
    }
}

pub fn verify_directory_against_lockfile(mods_dir: &Path, lockfile: &ModLockfile) -> Result<(), LockfileError> {
    let mut validated = lockfile.clone();
    validated.canonicalize_and_validate()?;
    let root_metadata = std::fs::symlink_metadata(mods_dir)
        .map_err(|_| LockfileError::MissingFile(mods_dir.display().to_string()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(LockfileError::MissingFile(mods_dir.display().to_string()));
    }
    for entry in &validated.entries {
        let mut path = mods_dir.to_path_buf();
        for (index, component) in Path::new(&entry.filename).components().enumerate() {
            let Component::Normal(part) = component else {
                return Err(LockfileError::UnsafeFilename(entry.filename.clone()));
            };
            path.push(part);
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|_| LockfileError::MissingFile(entry.filename.clone()))?;
            let is_final = index + 1 == Path::new(&entry.filename).components().count();
            if metadata.file_type().is_symlink()
                || (is_final && !metadata.file_type().is_file())
                || (!is_final && !metadata.file_type().is_dir())
            {
                return Err(LockfileError::MissingFile(entry.filename.clone()));
            }
        }
        let actual = sha512_file(&path).map_err(LockfileError::Io)?;
        if !actual.eq_ignore_ascii_case(&entry.sha512_hex) {
            return Err(LockfileError::DigestMismatch(entry.filename.clone()));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum LockfileError {
    Io(io::Error),
    Json(megu_core::json::JsonError),
    InvalidToml(String),
    UnknownKey(String),
    MissingField(&'static str),
    WrongType(&'static str),
    UnsupportedSchema(u16),
    InvalidSha512(String),
    UnsafeFilename(String),
    DuplicateModId(String),
    DuplicateCapability(String),
    DownloadsDisabled,
    MissingFile(String),
    DigestMismatch(String),
}

impl fmt::Display for LockfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Json(e) => write!(f, "invalid lockfile JSON: {e}"),
            Self::InvalidToml(e) => write!(f, "invalid modules TOML: {e}"),
            Self::UnknownKey(k) => write!(f, "unknown configuration key: {k}"),
            Self::MissingField(k) => write!(f, "missing field: {k}"),
            Self::WrongType(k) => write!(f, "wrong type for field: {k}"),
            Self::UnsupportedSchema(v) => write!(f, "unsupported schema_version: {v}"),
            Self::InvalidSha512(id) => write!(f, "invalid SHA-512 digest for {id}"),
            Self::UnsafeFilename(name) => write!(f, "unsafe mod filename: {name}"),
            Self::DuplicateModId(id) => write!(f, "empty or duplicate mod_id: {id}"),
            Self::DuplicateCapability(id) => write!(f, "duplicate required capability for {id}"),
            Self::DownloadsDisabled => f.write_str("mod downloads are disabled by offline policy"),
            Self::MissingFile(name) => write!(f, "missing or unsafe mod file: {name}"),
            Self::DigestMismatch(name) => write!(f, "SHA-512 mismatch for mod file: {name}"),
        }
    }
}
impl std::error::Error for LockfileError {}

fn required_string(value: &JsonValue, key: &'static str) -> Result<String, LockfileError> {
    value.get(key).and_then(JsonValue::as_str).map(str::to_owned).ok_or(LockfileError::MissingField(key))
}

fn required_u16(value: &JsonValue, key: &'static str) -> Result<u16, LockfileError> {
    let number = value.get(key).and_then(JsonValue::as_f64).ok_or(LockfileError::WrongType(key))?;
    if number.fract() != 0.0 || !(0.0..=u16::MAX as f64).contains(&number) {
        return Err(LockfileError::WrongType(key));
    }
    Ok(number as u16)
}

fn ensure_known_fields(value: &JsonValue, allowed: &[&str]) -> Result<(), LockfileError> {
    let members = value.as_object().ok_or(LockfileError::WrongType("object"))?;
    let mut seen = BTreeSet::new();
    for (key, _) in members {
        if !seen.insert(key.as_str()) {
            return Err(LockfileError::InvalidToml(format!(
                "duplicate JSON field: {key}"
            )));
        }
        if !allowed.contains(&key.as_str()) {
            return Err(LockfileError::UnknownKey(key.clone()));
        }
    }
    Ok(())
}

fn is_hex_digest(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_filename(filename: &str) -> Result<(), LockfileError> {
    let path = Path::new(filename);
    if filename.is_empty()
        || filename.contains('\\')
        || filename.contains(':')
        || path.is_absolute()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LockfileError::UnsafeFilename(filename.to_owned()));
    }
    Ok(())
}

fn sha512_file(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize_hex())
}

// SHA-512 (FIPS 180-4), local to this lockfile until the shared core exposes it.
struct Sha512 { state: [u64; 8], block: [u8; 128], block_len: usize, total: u128 }

impl Sha512 {
    const K: [u64; 80] = [
        0x428a2f98d728ae22,0x7137449123ef65cd,0xb5c0fbcfec4d3b2f,0xe9b5dba58189dbbc,0x3956c25bf348b538,0x59f111f1b605d019,0x923f82a4af194f9b,0xab1c5ed5da6d8118,
        0xd807aa98a3030242,0x12835b0145706fbe,0x243185be4ee4b28c,0x550c7dc3d5ffb4e2,0x72be5d74f27b896f,0x80deb1fe3b1696b1,0x9bdc06a725c71235,0xc19bf174cf692694,
        0xe49b69c19ef14ad2,0xefbe4786384f25e3,0x0fc19dc68b8cd5b5,0x240ca1cc77ac9c65,0x2de92c6f592b0275,0x4a7484aa6ea6e483,0x5cb0a9dcbd41fbd4,0x76f988da831153b5,
        0x983e5152ee66dfab,0xa831c66d2db43210,0xb00327c898fb213f,0xbf597fc7beef0ee4,0xc6e00bf33da88fc2,0xd5a79147930aa725,0x06ca6351e003826f,0x142929670a0e6e70,
        0x27b70a8546d22ffc,0x2e1b21385c26c926,0x4d2c6dfc5ac42aed,0x53380d139d95b3df,0x650a73548baf63de,0x766a0abb3c77b2a8,0x81c2c92e47edaee6,0x92722c851482353b,
        0xa2bfe8a14cf10364,0xa81a664bbc423001,0xc24b8b70d0f89791,0xc76c51a30654be30,0xd192e819d6ef5218,0xd69906245565a910,0xf40e35855771202a,0x106aa07032bbd1b8,
        0x19a4c116b8d2d0c8,0x1e376c085141ab53,0x2748774cdf8eeb99,0x34b0bcb5e19b48a8,0x391c0cb3c5c95a63,0x4ed8aa4ae3418acb,0x5b9cca4f7763e373,0x682e6ff3d6b2b8a3,
        0x748f82ee5defb2fc,0x78a5636f43172f60,0x84c87814a1f0ab72,0x8cc702081a6439ec,0x90befffa23631e28,0xa4506cebde82bde9,0xbef9a3f7b2c67915,0xc67178f2e372532b,
        0xca273eceea26619c,0xd186b8c721c0c207,0xeada7dd6cde0eb1e,0xf57d4f7fee6ed178,0x06f067aa72176fba,0x0a637dc5a2c898a6,0x113f9804bef90dae,0x1b710b35131c471b,
        0x28db77f523047d84,0x32caab7b40c72493,0x3c9ebe0a15c9bebc,0x431d67c49c100d4c,0x4cc5d4becb3e42b6,0x597f299cfc657e2a,0x5fcb6fab3ad6faec,0x6c44198c4a475817,
    ];

    fn new() -> Self {
        Self { state: [0x6a09e667f3bcc908,0xbb67ae8584caa73b,0x3c6ef372fe94f82b,0xa54ff53a5f1d36f1,0x510e527fade682d1,0x9b05688c2b3e6c1f,0x1f83d9abfb41bd6b,0x5be0cd19137e2179], block: [0; 128], block_len: 0, total: 0 }
    }

    fn compress(&mut self, block: &[u8; 128]) {
        let mut w = [0u64; 80];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            *word = u64::from_be_bytes(block[index * 8..index * 8 + 8].try_into().expect("fixed block"));
        }
        for index in 16..80 {
            let a = w[index - 15];
            let b = w[index - 2];
            let s0 = a.rotate_right(1) ^ a.rotate_right(8) ^ (a >> 7);
            let s1 = b.rotate_right(19) ^ b.rotate_right(61) ^ (b >> 6);
            w[index] = w[index - 16].wrapping_add(s0).wrapping_add(w[index - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (word, constant) in w.into_iter().zip(Self::K) {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let choice = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(choice).wrapping_add(constant).wrapping_add(word);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            [a,b,c,d,e,f,g,h] = [t1.wrapping_add(t2),a,b,c,d.wrapping_add(t1),e,f,g];
        }
        for (state, value) in self.state.iter_mut().zip([a,b,c,d,e,f,g,h]) { *state = state.wrapping_add(value); }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total = self.total.wrapping_add(input.len() as u128);
        if self.block_len > 0 {
            let take = (128 - self.block_len).min(input.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&input[..take]);
            self.block_len += take;
            input = &input[take..];
            if self.block_len == 128 { let block = self.block; self.compress(&block); self.block_len = 0; }
        }
        while input.len() >= 128 {
            let block: &[u8; 128] = input[..128].try_into().expect("fixed block");
            self.compress(block);
            input = &input[128..];
        }
        if !input.is_empty() { self.block[..input.len()].copy_from_slice(input); self.block_len = input.len(); }
    }

    fn finalize_hex(mut self) -> String {
        let bit_length = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.block_len != 112 { self.update(&[0]); }
        self.update(&bit_length.to_be_bytes());
        let mut output = String::with_capacity(128);
        for word in self.state { use std::fmt::Write; write!(output, "{word:016x}").expect("string formatting"); }
        output
    }
}
