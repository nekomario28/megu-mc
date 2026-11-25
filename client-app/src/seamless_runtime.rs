// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static RESUME_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeTarget {
    SingleplayerWorld {
        world_dir: PathBuf,
        display_name: String,
    },
    MultiplayerServer {
        host: String,
        port: u16,
        display_name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeRecord {
    pub target: ResumeTarget,
    pub last_timestamp: u64,
    pub mod_profile_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeStatus {
    Ready(ResumeRecord),
    TitleScreen(String),
}

pub struct InstantResumeStore {
    path: PathBuf,
}

impl InstantResumeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(
        &self,
        target: ResumeTarget,
        mod_profile_id: Option<String>,
    ) -> io::Result<ResumeRecord> {
        let target = sanitize_target(target)?;
        let mod_profile_id = mod_profile_id.map(|s| clean_text(&s)).transpose()?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let record = ResumeRecord {
            target,
            last_timestamp: timestamp,
            mod_profile_id,
        };
        let bytes = encode_record(&record);
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let sequence = RESUME_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .path
            .with_extension(format!("json.{}.{}.tmp", std::process::id(), sequence));
        let result: io::Result<()> = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(bytes.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        Ok(record)
    }

    pub fn resolve_resume(&self, requested: bool) -> ResumeStatus {
        if !requested {
            return ResumeStatus::TitleScreen("Resume was not requested".into());
        }
        match fs::read_to_string(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|s| decode_record(&s).map_err(|e| e.to_string()))
        {
            Ok(record) => match validate_target(record.target.clone()) {
                Ok(()) => ResumeStatus::Ready(record),
                Err(note) => ResumeStatus::TitleScreen(note),
            },
            Err(error) => ResumeStatus::TitleScreen(format!("Resume unavailable: {error}")),
        }
    }
}

fn clean_text(s: &str) -> io::Result<String> {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    let lower = s.to_ascii_lowercase();
    if [
        "password",
        "access_token",
        "auth_token",
        "refresh_token",
        "client_token",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || s.len() > 4096
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "resume field rejected by privacy filter",
        ));
    }
    Ok(s)
}

fn sanitize_target(target: ResumeTarget) -> io::Result<ResumeTarget> {
    match target {
        ResumeTarget::SingleplayerWorld {
            world_dir,
            display_name,
        } => Ok(ResumeTarget::SingleplayerWorld {
            world_dir,
            display_name: clean_text(&display_name)?,
        }),
        ResumeTarget::MultiplayerServer {
            host,
            port,
            display_name,
        } => Ok(ResumeTarget::MultiplayerServer {
            host: clean_text(&host)?,
            port,
            display_name: clean_text(&display_name)?,
        }),
    }
}

fn validate_target(target: ResumeTarget) -> Result<(), String> {
    match target {
        ResumeTarget::SingleplayerWorld { world_dir, .. } => {
            if world_dir.is_dir() {
                Ok(())
            } else {
                Err("Resume world is missing; returning to title screen".into())
            }
        }
        ResumeTarget::MultiplayerServer { host, port, .. } => {
            if valid_host(&host) && port != 0 {
                Ok(())
            } else {
                Err("Resume server address is malformed; returning to title screen".into())
            }
        }
    }
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.contains(['/', '@', '?', '#', ':']) {
        return host.parse::<Ipv6Addr>().is_ok();
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

fn escape(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn encode_record(r: &ResumeRecord) -> String {
    let profile = r
        .mod_profile_id
        .as_ref()
        .map(|s| escape(s))
        .unwrap_or_else(|| "null".into());
    let target = match &r.target {
        ResumeTarget::SingleplayerWorld {
            world_dir,
            display_name,
        } => format!(
            "{{\"kind\":\"world\",\"world_dir\":{},\"display_name\":{}}}",
            escape(&world_dir.to_string_lossy()),
            escape(display_name)
        ),
        ResumeTarget::MultiplayerServer {
            host,
            port,
            display_name,
        } => format!(
            "{{\"kind\":\"server\",\"host\":{},\"port\":{},\"display_name\":{}}}",
            escape(host),
            port,
            escape(display_name)
        ),
    };
    format!(
        "{{\"last_timestamp\":{},\"mod_profile_id\":{},\"target\":{}}}\n",
        r.last_timestamp, profile, target
    )
}

#[derive(Debug)]
enum Json {
    Null,
    Number(u64),
    String(String),
    Object(BTreeMap<String, Json>),
}
struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}
impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn byte(&mut self, b: u8) -> Result<(), String> {
        self.ws();
        if self.s.get(self.i) == Some(&b) {
            self.i += 1;
            Ok(())
        } else {
            Err("malformed resume JSON".into())
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.byte(b'"')?;
        let mut out = String::new();
        while let Some(&b) = self.s.get(self.i) {
            self.i += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = *self.s.get(self.i).ok_or("malformed escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        _ => return Err("unsupported escape".into()),
                    }
                }
                0..=31 => return Err("control character in JSON string".into()),
                _ if b.is_ascii() => out.push(b as char),
                _ => {
                    self.i -= 1;
                    let tail =
                        std::str::from_utf8(&self.s[self.i..]).map_err(|_| "invalid UTF-8")?;
                    let c = tail.chars().next().ok_or("invalid UTF-8")?;
                    out.push(c);
                    self.i += c.len_utf8();
                }
            }
        }
        Err("unterminated JSON string".into())
    }
    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.s.get(self.i).copied() {
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'{') => {
                self.i += 1;
                let mut m = BTreeMap::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Object(m));
                }
                loop {
                    let k = self.string()?;
                    self.byte(b':')?;
                    let v = self.value()?;
                    if m.insert(k, v).is_some() {
                        return Err("duplicate JSON key".into());
                    }
                    self.ws();
                    if self.s.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        break;
                    }
                    self.byte(b',')?;
                }
                Ok(Json::Object(m))
            }
            Some(b'n') if self.s.get(self.i..self.i + 4) == Some(b"null") => {
                self.i += 4;
                Ok(Json::Null)
            }
            Some(b'0'..=b'9') => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1
                }
                let n = std::str::from_utf8(&self.s[start..self.i])
                    .map_err(|_| "bad number")?
                    .parse()
                    .map_err(|_| "bad number")?;
                Ok(Json::Number(n))
            }
            _ => Err("malformed resume JSON".into()),
        }
    }
}
fn take_string(m: &mut BTreeMap<String, Json>, k: &str) -> Result<String, String> {
    match m.remove(k) {
        Some(Json::String(s)) => Ok(s),
        _ => Err(format!("missing or invalid {k}")),
    }
}
fn decode_record(s: &str) -> Result<ResumeRecord, String> {
    let mut p = Parser {
        s: s.as_bytes(),
        i: 0,
    };
    let Json::Object(mut root) = p.value()? else {
        return Err("resume record must be an object".into());
    };
    p.ws();
    if p.i != p.s.len() {
        return Err("trailing resume data".into());
    }
    let last_timestamp = match root.remove("last_timestamp") {
        Some(Json::Number(n)) => n,
        _ => return Err("invalid timestamp".into()),
    };
    let mod_profile_id = match root.remove("mod_profile_id") {
        Some(Json::Null) => None,
        Some(Json::String(s)) => Some(clean_text(&s).map_err(|e| e.to_string())?),
        _ => return Err("invalid profile id".into()),
    };
    let Json::Object(mut t) = root.remove("target").ok_or("missing target")? else {
        return Err("invalid target".into());
    };
    let kind = take_string(&mut t, "kind")?;
    let target = match kind.as_str() {
        "world" => ResumeTarget::SingleplayerWorld {
            world_dir: PathBuf::from(take_string(&mut t, "world_dir")?),
            display_name: clean_text(&take_string(&mut t, "display_name")?)
                .map_err(|e| e.to_string())?,
        },
        "server" => {
            let host = clean_text(&take_string(&mut t, "host")?).map_err(|e| e.to_string())?;
            let port = match t.remove("port") {
                Some(Json::Number(n)) => u16::try_from(n).map_err(|_| "invalid port")?,
                _ => return Err("invalid port".into()),
            };
            ResumeTarget::MultiplayerServer {
                host,
                port,
                display_name: clean_text(&take_string(&mut t, "display_name")?)
                    .map_err(|e| e.to_string())?,
            }
        }
        _ => return Err("unknown resume target".into()),
    };
    if !t.is_empty() || !root.is_empty() {
        return Err("unexpected resume fields".into());
    }
    Ok(ResumeRecord {
        target,
        last_timestamp,
        mod_profile_id,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AudioDeviceId(pub String);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSink {
    pub id: AudioDeviceId,
    pub fingerprint: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioAction {
    None,
    Rebind {
        sink: AudioSink,
        retry_after_ticks: u32,
    },
}
pub struct AudioDeviceHotSwapMonitor {
    current: Option<AudioSink>,
    errors: u32,
    zero_drains: u32,
    retry_ticks: u32,
    backoff: u32,
    fade_samples_total: usize,
    fade_samples_elapsed: usize,
}
impl Default for AudioDeviceHotSwapMonitor {
    fn default() -> Self {
        Self {
            current: None,
            errors: 0,
            zero_drains: 0,
            retry_ticks: 0,
            backoff: 1,
            fade_samples_total: 0,
            fade_samples_elapsed: 0,
        }
    }
}
impl AudioDeviceHotSwapMonitor {
    pub fn current(&self) -> Option<&AudioSink> {
        self.current.as_ref()
    }
    pub fn tick(
        &mut self,
        available: Option<AudioSink>,
        write_error: bool,
        zero_drain: bool,
        muted: bool,
        sample_rate: u32,
    ) -> AudioAction {
        if write_error {
            self.errors = self.errors.saturating_add(1)
        } else {
            self.errors = 0
        };
        if zero_drain {
            self.zero_drains = self.zero_drains.saturating_add(1)
        } else {
            self.zero_drains = 0
        };
        if muted || available.is_none() {
            self.retry_ticks = self.backoff;
            return AudioAction::None;
        }
        let changed = self.current.as_ref() != available.as_ref();
        let stalled = self.errors >= 3 || self.zero_drains >= 3;
        if !changed && !stalled {
            self.retry_ticks = 0;
            self.backoff = 1;
            return AudioAction::None;
        }
        if self.retry_ticks > 0 {
            self.retry_ticks -= 1;
            return AudioAction::None;
        }
        let sink = available.expect("checked above");
        self.current = Some(sink.clone());
        self.errors = 0;
        self.zero_drains = 0;
        self.fade_samples_total = ((sample_rate as usize).saturating_mul(20) / 1000).max(1);
        self.fade_samples_elapsed = 0;
        self.retry_ticks = self.backoff;
        self.backoff = (self.backoff.saturating_mul(2)).min(256);
        AudioAction::Rebind {
            sink,
            retry_after_ticks: self.retry_ticks,
        }
    }
    pub fn apply_rebind_fade_in(&mut self, pcm_i16: &mut [i16]) {
        let total = self.fade_samples_total;
        if total == 0 {
            return;
        }
        let count = pcm_i16
            .len()
            .min(total.saturating_sub(self.fade_samples_elapsed));
        for (i, sample) in pcm_i16[..count].iter_mut().enumerate() {
            let gain = (self.fade_samples_elapsed + i + 1) as i64;
            *sample = ((*sample as i64 * gain) / total as i64)
                .clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        }
        self.fade_samples_elapsed += count;
        if self.fade_samples_elapsed >= total {
            self.fade_samples_total = 0;
            self.fade_samples_elapsed = 0;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReloadChannel {
    Resources,
    Language,
    ShaderPack,
    FontAtlas,
}
#[derive(Debug)]
pub enum ReloadState<T> {
    Idle,
    Preparing { generation: u64 },
    ReadyToSwap { generation: u64, payload: T },
    Swapped { generation: u64 },
}
pub struct ZeroFreezeReloadCoordinator<T> {
    channels: [ReloadState<T>; 4],
    next_generation: u64,
    swap_frames: u64,
    max_swap_budget_ns: u128,
}
impl<T> Default for ZeroFreezeReloadCoordinator<T> {
    fn default() -> Self {
        Self {
            channels: std::array::from_fn(|_| ReloadState::Idle),
            next_generation: 1,
            swap_frames: 0,
            max_swap_budget_ns: 0,
        }
    }
}
fn channel_index(channel: ReloadChannel) -> usize {
    match channel {
        ReloadChannel::Resources => 0,
        ReloadChannel::Language => 1,
        ReloadChannel::ShaderPack => 2,
        ReloadChannel::FontAtlas => 3,
    }
}
impl<T> ZeroFreezeReloadCoordinator<T> {
    pub fn begin(&mut self, channel: ReloadChannel) -> u64 {
        let g = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.channels[channel_index(channel)] = ReloadState::Preparing { generation: g };
        g
    }
    pub fn prepare(&mut self, channel: ReloadChannel, generation: u64, payload: T) -> bool {
        let i = channel_index(channel);
        if matches!(self.channels[i],ReloadState::Preparing{generation:g} if g==generation) {
            self.channels[i] = ReloadState::ReadyToSwap {
                generation,
                payload,
            };
            true
        } else {
            false
        }
    }
    pub fn swap_at_frame(&mut self, channel: ReloadChannel, budget_ns: u128) -> Option<T> {
        let i = channel_index(channel);
        let state = std::mem::replace(&mut self.channels[i], ReloadState::Idle);
        match state {
            ReloadState::ReadyToSwap {
                generation,
                payload,
            } => {
                self.swap_frames = self.swap_frames.saturating_add(1);
                self.max_swap_budget_ns = self.max_swap_budget_ns.max(budget_ns);
                self.channels[i] = ReloadState::Swapped { generation };
                Some(payload)
            }
            other => {
                self.channels[i] = other;
                None
            }
        }
    }
    pub fn state(&self, channel: ReloadChannel) -> Option<&ReloadState<T>> {
        Some(&self.channels[channel_index(channel)])
    }
    pub fn swap_metrics(&self) -> (u64, u128) {
        (self.swap_frames, self.max_swap_budget_ns)
    }
}
