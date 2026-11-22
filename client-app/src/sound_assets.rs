// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Vanilla sound-asset resolution for the live audio pipeline.
//!
//! Vanilla sounds are OGG Vorbis files in the user's own Minecraft asset
//! store — nothing is bundled with this client. Resolution is a two-step
//! lookup:
//!
//! 1. `<assetsDir>/indexes/<assetIndex>.json` maps logical names
//!    (`"minecraft/sounds.json"`, `"minecraft/sounds/entity/levelup.ogg"`)
//!    to content-addressed files under `objects/<hash[..2]>/<hash>`.
//! 2. `minecraft/sounds.json` maps each sound-event name
//!    (`"entity.player.levelup"`) to a weighted list of variants with
//!    per-variant `volume`/`pitch` multipliers.
//!
//! Decoding uses symphonia (`vorbis` + `ogg` only). Events that cannot be
//! resolved or decoded fall back to a deterministic placeholder tone — the
//! caller counts those under `placeholder_voices`, never pretending real
//! audio played.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use symphonia::core::io::MediaSource;

use crate::protocol::{parse_json, JsonValue};

/// Mixer bus rate; decoded OGGs (usually 44.1 kHz) are linear-resampled to
/// this so the [`crate::spatial_audio::SpatialAudioEngine`] voices never see
/// a sample-rate mismatch.
pub const MIX_SAMPLE_RATE: u32 = 48_000;

/// Upper bound on JSON nodes inside an index/sounds.json file. Vanilla
/// `sounds.json` carries ~17k event definitions (~150k nodes); this is far
/// above `MAX_STATUS_JSON_NODES` used for server pings.
const MAX_ASSET_JSON_NODES: usize = 2_000_000;
/// Upper bound on an index or sounds.json file size (vanilla ~1.5 MiB).
const MAX_ASSET_JSON_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum bytes of OGG input a single sound may decode from.
const MAX_OGG_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum decoded mono samples per sound (guards the PCM budget).
const MAX_DECODED_SAMPLES: usize = MIX_SAMPLE_RATE as usize * 300;

/// Hard cap on decoded PCM bytes held by [`SoundLibrary`]'s replay cache.
/// 48 kHz mono `f32` runs ~11 MiB/minute, so 32 MiB is ~3 minutes of
/// resident audio — long enough for every repeated sound effect while
/// keeping long music tracks from ballooning RSS without bound.
pub const DEFAULT_SOUND_PCM_CACHE_CAP_BYTES: usize = 32 * 1024 * 1024;

/// The `minecraft:sound_event` registry ids mc-spine's server can emit.
/// `id -> name`; mirrors the `sound_id` table in `src/play_protocol.rs`
/// (minecraft-data 1.21.1 registration order). Events arriving with an id
/// outside this subset are still played — labelled `sound.<id>` — and
/// counted as `unknown_sound_id` by the caller.
const SOUND_ID_TO_NAME: &[(u32, &str)] = &[
    (8, "ambient.cave"),
    (51, "block.anvil.land"),
    (303, "block.chest.open"),
    (375, "entity.creeper.primed"),
    (456, "entity.egg.throw"),
    (470, "entity.ender_dragon.death"),
    (483, "entity.enderman.teleport"),
    (502, "entity.experience_orb.pickup"),
    (506, "entity.firework_rocket.blast"),
    (510, "entity.firework_rocket.launch"),
    (577, "entity.generic.explode"),
    (629, "block.grass.break"),
    (759, "entity.item_frame.place"),
    (763, "entity.item.pickup"),
    (783, "entity.lightning_bolt.thunder"),
    (989, "block.note_block.bell"),
    (993, "block.note_block.harp"),
    (995, "block.note_block.pling"),
    (1110, "entity.player.hurt"),
    (1115, "entity.player.levelup"),
    (1133, "block.portal.travel"),
    (1256, "entity.skeleton.ambient"),
    (1344, "entity.spider.ambient"),
    (1362, "block.stone.break"),
    (1434, "ui.toast.challenge_complete"),
    (1461, "entity.villager.yes"),
    (1548, "entity.wither.spawn"),
    (1586, "entity.zombie.ambient"),
    (1591, "entity.zombie.death"),
    (84, "entity.arrow.hit_player"),
];

/// Registry id → event name for the ids mc-spine emits.
pub fn sound_name_for_id(id: u32) -> Option<&'static str> {
    SOUND_ID_TO_NAME
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| *v)
}

/// Full `minecraft:sound_event` numeric-id → name table — populated either
/// from the server's `registry_data` packet (authoritative, covers every
/// Mod namespace too) or from a `registries.json`-shaped document on disk.
/// [`sound_name_for_id`] remains the built-in fallback for the ids the
/// bundled table knows.
#[derive(Clone, Debug, Default)]
pub struct SoundEventRegistry {
    /// `names[id]` is the entry name (`"entity.player.levelup"`).
    names: Vec<String>,
}

impl SoundEventRegistry {
    /// Build from the registry entries in wire order (`names[i]` ↔ id `i`).
    pub fn from_names(names: Vec<String>) -> Self {
        Self { names }
    }

    /// Load a vanilla `registries.json`-shaped document:
    /// `{"minecraft:sound_event": {"entries": {"name": {"protocol_id": N}}}}`.
    /// Missing registry/entries keys fail closed.
    pub fn load_registries_json(path: &Path) -> Result<Self, String> {
        let raw = read_capped(path, MAX_ASSET_JSON_BYTES)?;
        let text = String::from_utf8(raw)
            .map_err(|_| format!("{} is not UTF-8 JSON", path.display()))?;
        let doc = parse_json(&text, MAX_ASSET_JSON_NODES)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let entries = doc
            .get("minecraft:sound_event")
            .and_then(|r| r.get("entries"))
            .and_then(|e| match e {
                JsonValue::Obj(m) => Some(m),
                _ => None,
            })
            .ok_or_else(|| {
                format!("{}: no minecraft:sound_event entries", path.display())
            })?;
        let mut pairs: Vec<(u32, &str)> = Vec::with_capacity(entries.len());
        for (name, val) in entries {
            if let Some(id) = val
                .get("protocol_id")
                .and_then(JsonValue::as_i64)
                .and_then(|v| u32::try_from(v).ok())
            {
                pairs.push((id, name.as_str()));
            }
        }
        if pairs.is_empty() {
            return Err(format!("{}: empty minecraft:sound_event", path.display()));
        }
        pairs.sort_unstable_by_key(|(id, _)| *id);
        let max_id = pairs
            .last()
            .map(|(id, _)| *id as usize)
            .unwrap_or(0)
            .min(65_535);
        let mut names = vec![String::new(); max_id + 1];
        for (id, name) in pairs {
            names[id as usize] = name.to_string();
        }
        Ok(Self { names })
    }

    /// Event name for `id` (`"entity.player.levelup"`), normalized to the
    /// bare path form. `None` when the registry does not cover the id.
    pub fn name(&self, id: u32) -> Option<&str> {
        self.names
            .get(id as usize)
            .map(String::as_str)
            .filter(|n| !n.is_empty())
            .map(normalize_sound_name)
    }

    /// Number of id slots the registry covers.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// `true` when no entries loaded.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Strip a `minecraft:` namespace prefix — sounds.json keys, stop-sound
/// names, and holder identifiers all normalize to the bare path form.
pub fn normalize_sound_name(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

/// FNV-1a over a sound name — stable `SoundKey::sound_id` for limiter
/// coalescing and deterministic placeholder/variant hashing.
pub fn sound_name_hash(name: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for b in normalize_sound_name(name).as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One playable variant of a sound event from `sounds.json`.
#[derive(Clone, Debug)]
pub struct SoundVariant {
    /// Logical object key in the index (`"minecraft/sounds/x/y.ogg"`).
    pub object_key: String,
    /// Per-variant volume multiplier (default 1.0).
    pub volume: f32,
    /// Per-variant pitch multiplier (default 1.0).
    pub pitch: f32,
    /// Selection weight (default 1). The packet `seed` picks within the
    /// cumulative weights the way vanilla's `seed`-seeded RNG does.
    pub weight: u32,
    /// `type: "event"` redirects to another event's variants.
    pub redirect: Option<String>,
    /// Per-variant Vanilla attenuation distance (`attenuation_distance`,
    /// default 16 — jukeboxes declare 64, explosions reach 16 × volume).
    pub attenuation_distance: Option<f32>,
    /// `stream: true` — vanilla marks long music files; kept for API
    /// completeness (the client decodes once into bounded memory either way).
    pub stream: bool,
}

/// A `sounds.json` event definition.
#[derive(Clone, Debug, Default)]
pub struct SoundEventDef {
    /// Variants in declaration order.
    pub variants: Vec<SoundVariant>,
    /// `subtitle` translation key, when declared (e.g.
    /// `subtitles.entity.player.levelup`).
    pub subtitle: Option<String>,
    /// `replace: true` — a higher-priority layer discards lower layers'
    /// variants entirely instead of appending (resource packs use this).
    pub replace: bool,
}

/// Where a resolved `.ogg` actually lives — the multi-layer equivalent of
/// "a path in the vanilla object store".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundSourceData {
    /// Plain filesystem path (vanilla object store or resource-pack dir).
    File(PathBuf),
    /// One entry inside a ZIP/JAR — decompressed in memory on decode; no
    /// file is ever extracted to disk.
    ZipEntry {
        /// Archive path (Mod JAR or pack ZIP).
        archive_path: PathBuf,
        /// Safe entry name (`assets/<ns>/sounds/...`), indexed at layer
        /// merge time.
        entry_name: String,
    },
}

/// Decoded PCM payload — `Mono` for world sounds and legacy callers,
/// `Stereo` for 2D-bypassed assets (Music/UI) that must keep their L/R.
#[derive(Clone, Debug)]
pub enum DecodedPcm {
    /// 48 kHz mono samples.
    Mono(Vec<f32>),
    /// 48 kHz `[L, R]` frames.
    Stereo(Vec<[f32; 2]>),
}

impl DecodedPcm {
    /// Mono copy — downmixes stereo `(L+R)*0.5` for 3D use.
    pub fn to_mono(&self) -> Vec<f32> {
        match self {
            Self::Mono(v) => v.clone(),
            Self::Stereo(v) => v.iter().map(|f| (f[0] + f[1]) * 0.5).collect(),
        }
    }

    /// Frame count (samples for mono, `[L,R]` pairs for stereo).
    pub fn frames(&self) -> usize {
        match self {
            Self::Mono(v) => v.len(),
            Self::Stereo(v) => v.len(),
        }
    }

    /// Decoded byte footprint (cache accounting).
    pub fn bytes(&self) -> usize {
        self.frames() * std::mem::size_of::<f32>() * if self.is_stereo() { 2 } else { 1 }
    }

    /// `true` for the stereo variant.
    pub fn is_stereo(&self) -> bool {
        matches!(self, Self::Stereo(_))
    }
}

/// A resolved variant ready to decode.
#[derive(Clone, Debug)]
pub struct ResolvedSound {
    /// Filesystem path of the `.ogg` (object store or pack dir); for
    /// archive-resident entries this is a `jar!/entry` surrogate used for
    /// display and cache keys — never opened directly.
    pub path: PathBuf,
    /// Where to read the OGG bytes from.
    pub source: SoundSourceData,
    /// Combined volume multiplier (variant-level).
    pub volume: f32,
    /// Combined pitch multiplier (variant-level).
    pub pitch: f32,
    /// `stream: true` variant flag (vanilla long-form/music marker).
    pub stream: bool,
    /// Variant `attenuation_distance` when declared (default 16 vanilla).
    pub attenuation_distance: Option<f32>,
}

/// One decoded sound held by [`SoundLibrary`]'s LRU replay cache.
struct CachedPcm {
    /// Shared decoded 48 kHz PCM (mono or stereo).
    pcm: Arc<DecodedPcm>,
    /// `pcm.len() * size_of::<f32>()` — the bytes counted against the cap.
    bytes: usize,
    /// Last `lru_clock` tick that inserted or touched this entry; the
    /// smallest value in the map is the eviction victim.
    last_used: u64,
}

/// One merged `sounds.json` layer — a resource-pack directory or a ZIP/JAR
/// archive (Mod JAR or zipped resource pack). Layers are ordered
/// lowest→highest priority at merge time; object lookups scan them
/// highest→lowest so packs always win over Mod assets, which always win
/// over the vanilla index.
struct SoundLayer {
    /// Resource-pack root directory (`assets/` sits directly under it).
    dir: Option<PathBuf>,
    /// Indexed archive (Mod JAR / zipped pack) with safe entry names.
    zip: Option<crate::zip_entries::ZipArchive>,
}

/// The parsed asset index + `sounds.json` for one `--assetsDir`/
/// `--assetIndex` pair, layered with any Mod JARs and resource packs merged
/// through [`SoundLibrary::merge_sound_layer`], plus a decoded-PCM cache so
/// replayed sounds decode once. The cache is strictly bounded to
/// [`DEFAULT_SOUND_PCM_CACHE_CAP_BYTES`] (configurable via
/// [`SoundLibrary::with_pcm_cache_cap_bytes`]) and evicts
/// least-recently-used entries when an insert would overflow it; a single
/// clip larger than the whole cap is played without being cached so
/// resident decoded PCM never exceeds the budget.
pub struct SoundLibrary {
    objects: HashMap<String, PathBuf>,
    events: HashMap<String, SoundEventDef>,
    /// Extra layers in lowest→highest priority order (Mod JARs first, then
    /// resource packs). Their merged event definitions already live in
    /// `events`; the layer list exists to resolve object paths.
    layers: Vec<SoundLayer>,
    decode_cache: HashMap<PathBuf, CachedPcm>,
    /// Byte cap applied to `decode_cache` on every insert.
    pcm_cache_cap_bytes: usize,
    /// Sum of `CachedPcm::bytes` currently in `decode_cache`.
    pcm_cache_bytes: usize,
    /// Lifetime count of LRU evictions (telemetry).
    pcm_cache_evictions: u64,
    /// Lifetime count of `decode` calls served from the cache (telemetry).
    pcm_cache_hits: u64,
    /// Monotonic recency counter — never wall time, so eviction order is
    /// deterministic and identical on every replay of the same call order.
    lru_clock: u64,
}

impl SoundLibrary {
    /// Load `indexes/<asset_index>.json` plus `minecraft/sounds.json` out of
    /// `assets_dir`. `asset_index` may be passed with or without `.json`.
    /// Errors are descriptive strings; callers fall back to placeholder
    /// tones.
    pub fn load(assets_dir: &Path, asset_index: &str) -> Result<Self, String> {
        let index_name = if asset_index.ends_with(".json") {
            asset_index.to_string()
        } else {
            format!("{asset_index}.json")
        };
        if index_name.contains('/') || index_name.contains('\\') || index_name.contains("..") {
            return Err(format!("refusing asset index name '{index_name}'"));
        }
        let index_path = assets_dir.join("indexes").join(index_name);
        let raw = read_capped(&index_path, MAX_ASSET_JSON_BYTES)?;
        let text = String::from_utf8(raw)
            .map_err(|_| format!("{} is not UTF-8 JSON", index_path.display()))?;
        let doc = parse_json(&text, MAX_ASSET_JSON_NODES)
            .map_err(|e| format!("{}: {e}", index_path.display()))?;
        let Some(JsonValue::Obj(object_entries)) = doc.get("objects") else {
            return Err(format!("{} has no \"objects\" map", index_path.display()));
        };
        let mut objects = HashMap::with_capacity(object_entries.len());
        for (key, value) in object_entries {
            let Some(hash) = value.get("hash").and_then(JsonValue::as_str) else {
                continue;
            };
            if !is_object_hash(hash) {
                continue;
            }
            objects.insert(
                key.clone(),
                assets_dir.join("objects").join(&hash[..2]).join(hash),
            );
        }

        let mut events = HashMap::new();
        if let Some(sounds_path) = objects.get("minecraft/sounds.json").cloned() {
            let raw = read_capped(&sounds_path, MAX_ASSET_JSON_BYTES)?;
            let text = String::from_utf8(raw)
                .map_err(|_| format!("{} is not UTF-8 JSON", sounds_path.display()))?;
            let doc = parse_json(&text, MAX_ASSET_JSON_NODES)
                .map_err(|e| format!("{}: {e}", sounds_path.display()))?;
            if let JsonValue::Obj(top) = doc {
                for (event, def) in &top {
                    events.insert(
                        normalize_sound_name(event).to_string(),
                        parse_sound_event_def(def),
                    );
                }
            }
        }
        Ok(Self {
            objects,
            events,
            layers: Vec::new(),
            decode_cache: HashMap::new(),
            pcm_cache_cap_bytes: DEFAULT_SOUND_PCM_CACHE_CAP_BYTES,
            pcm_cache_bytes: 0,
            pcm_cache_evictions: 0,
            pcm_cache_hits: 0,
            lru_clock: 0,
        })
    }

    /// An empty library (no vanilla index); layers can still be merged.
    /// Used by Mod-only sessions and by tests building pack/JAR layers.
    pub fn empty() -> Self {
        Self {
            objects: HashMap::new(),
            events: HashMap::new(),
            layers: Vec::new(),
            decode_cache: HashMap::new(),
            pcm_cache_cap_bytes: DEFAULT_SOUND_PCM_CACHE_CAP_BYTES,
            pcm_cache_bytes: 0,
            pcm_cache_evictions: 0,
            pcm_cache_hits: 0,
            lru_clock: 0,
        }
    }

    /// Merge one `sounds.json` layer into the library. `path` may be:
    ///
    /// * a resource-pack **directory** — every `assets/<ns>/sounds.json`
    ///   under it merges, and `.ogg` files resolve from disk;
    /// * a **ZIP/JAR** (Mod JAR or zipped pack) — indexed by the bounded
    ///   reader; `assets/<ns>/sounds.json` merges and `.ogg` entries decode
    ///   straight from memory (nothing is extracted).
    ///
    /// Call order defines priority: later merges sit above earlier ones —
    /// the intended order is vanilla → Mod JARs → resource packs.
    /// `replace: true` in a higher layer discards lower layers' variants
    /// for that event; `replace` absent/`false` appends variants.
    ///
    /// Returns the number of `sounds.json` documents merged.
    pub fn merge_sound_layer(&mut self, path: &Path) -> Result<usize, String> {
        if path.is_dir() {
            let assets = path.join("assets");
            let ns_entries = std::fs::read_dir(&assets)
                .map_err(|e| format!("cannot scan {}: {e}", assets.display()))?;
            let mut count = 0usize;
            for ns_entry in ns_entries {
                let ns_entry = ns_entry.map_err(|e| format!("read_dir: {e}"))?;
                let sj = ns_entry.path().join("sounds.json");
                if sj.is_file() {
                    let ns = ns_entry.file_name().to_string_lossy().into_owned();
                    self.merge_defs(
                        ns.as_str(),
                        parse_sounds_json_bytes(
                            &read_capped(&sj, MAX_ASSET_JSON_BYTES)?,
                            &sj,
                        )?,
                    );
                    count += 1;
                }
            }
            self.layers.push(SoundLayer {
                dir: Some(path.to_path_buf()),
                zip: None,
            });
            return Ok(count);
        }
        let zip = crate::zip_entries::ZipArchive::open(path)?;
        let mut count = 0usize;
        let names: Vec<String> = zip
            .entry_names()
            .filter(|n| {
                n.starts_with("assets/")
                    && n.ends_with("/sounds.json")
                    && n.split('/').count() == 3
            })
            .map(str::to_string)
            .collect();
        for name in names {
            let bytes = zip
                .read_entry(&name, MAX_ASSET_JSON_BYTES as usize)?
                .ok_or_else(|| format!("{name} vanished from archive"))?;
            let ns = name.split('/').nth(1).unwrap_or_default();
            self.merge_defs(ns, parse_sounds_json_bytes(&bytes, &PathBuf::from(&name))?);
            count += 1;
        }
        self.layers.push(SoundLayer {
            dir: None,
            zip: Some(zip),
        });
        Ok(count)
    }

    /// Apply one layer's definitions under namespace `ns`, honoring
    /// `replace`. `sounds.json` keys carry no namespace on the wire — the
    /// namespace comes from the `assets/<ns>/` directory — so each event
    /// merges as `<ns>:<event>` (`minecraft:` normalizes to the bare
    /// vanilla form the wire packets already use).
    fn merge_defs(&mut self, ns: &str, defs: Vec<(String, SoundEventDef)>) {
        for (raw_name, def) in defs {
            let qualified = if raw_name.contains(':') {
                raw_name
            } else {
                format!("{ns}:{raw_name}")
            };
            let name = normalize_sound_name(&qualified).to_string();
            if def.replace {
                self.events.insert(name, def);
            } else {
                match self.events.get_mut(&name) {
                    Some(existing) => {
                        existing.variants.extend(def.variants);
                        if def.subtitle.is_some() {
                            existing.subtitle = def.subtitle;
                        }
                    }
                    None => {
                        self.events.insert(name, def);
                    }
                }
            }
        }
    }

    /// Override the decoded-PCM cache byte cap. Builder-style so tests can
    /// pin a tiny deterministic budget; shrinking below the current
    /// occupancy evicts least-recently-used entries immediately.
    pub fn with_pcm_cache_cap_bytes(mut self, cap_bytes: usize) -> Self {
        self.pcm_cache_cap_bytes = cap_bytes;
        self.evict_pcm_over_cap();
        self
    }

    /// Number of event definitions loaded from `sounds.json`.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Configured decoded-PCM cache cap in bytes.
    pub fn pcm_cache_cap_bytes(&self) -> usize {
        self.pcm_cache_cap_bytes
    }

    /// Decoded PCM bytes currently held by the replay cache. Never exceeds
    /// [`SoundLibrary::pcm_cache_cap_bytes`] after an insert returns.
    pub fn pcm_cache_bytes(&self) -> usize {
        self.pcm_cache_bytes
    }

    /// Alias of [`SoundLibrary::pcm_cache_bytes`] kept for call sites that
    /// describe the same total in cache terms.
    pub fn cached_pcm_bytes(&self) -> usize {
        self.pcm_cache_bytes
    }

    /// Lifetime count of least-recently-used evictions (telemetry).
    pub fn pcm_cache_evictions(&self) -> u64 {
        self.pcm_cache_evictions
    }

    /// Lifetime count of `decode` calls served from cache (telemetry).
    pub fn pcm_cache_hits(&self) -> u64 {
        self.pcm_cache_hits
    }

    /// Whether `path` currently has decoded PCM cached.
    pub fn is_cached(&self, path: &Path) -> bool {
        self.decode_cache.contains_key(path)
    }

    /// Drop least-recently-used entries until `pcm_cache_bytes` fits the
    /// cap. `last_used` ticks are unique per `decode`/`insert_decoded_pcm`
    /// call, so the victim choice is deterministic.
    fn evict_pcm_over_cap(&mut self) {
        while self.pcm_cache_bytes > self.pcm_cache_cap_bytes {
            let mut victim: Option<(&PathBuf, u64)> = None;
            for (key, entry) in &self.decode_cache {
                let older = match victim {
                    None => true,
                    Some((_, best_tick)) => entry.last_used < best_tick,
                };
                if older {
                    victim = Some((key, entry.last_used));
                }
            }
            let Some((key, _)) = victim else {
                break;
            };
            let key = key.clone();
            if let Some(entry) = self.decode_cache.remove(&key) {
                self.pcm_cache_bytes -= entry.bytes;
                self.pcm_cache_evictions += 1;
            }
        }
    }

    /// Insert already-decoded 48 kHz mono PCM under `path`, bounded by the
    /// same LRU budget [`SoundLibrary::decode`] uses — the prewarm/decode
    /// worker entry point. Returns `false` without caching when the clip
    /// alone exceeds the cap, so resident decoded PCM can never overflow
    /// the budget.
    pub fn insert_decoded_pcm(&mut self, path: &Path, pcm: Arc<Vec<f32>>) -> bool {
        self.lru_clock += 1;
        let tick = self.lru_clock;
        let bytes = pcm.len() * std::mem::size_of::<f32>();
        if bytes > self.pcm_cache_cap_bytes {
            // A single clip bigger than the whole budget is played
            // uncached; dropping any previous entry keeps the invariant
            // that cached bytes never exceed the cap.
            if let Some(entry) = self.decode_cache.remove(path) {
                self.pcm_cache_bytes -= entry.bytes;
            }
            return false;
        }
        let pcm = Arc::new(DecodedPcm::Mono((*pcm).clone()));
        if let Some(entry) = self.decode_cache.get_mut(path) {
            self.pcm_cache_bytes -= entry.bytes;
            entry.pcm = pcm;
            entry.bytes = bytes;
            entry.last_used = tick;
            self.pcm_cache_bytes += bytes;
            self.evict_pcm_over_cap();
            return true;
        }
        self.decode_cache.insert(
            path.to_path_buf(),
            CachedPcm {
                pcm,
                bytes,
                last_used: tick,
            },
        );
        self.pcm_cache_bytes += bytes;
        self.evict_pcm_over_cap();
        true
    }

    /// Resolve `event_name` (`"entity.player.levelup"` or namespaced) to a
    /// playable OGG source plus its variant multipliers. `seed` (the
    /// packet's sound seed) picks the variant by cumulative weight,
    /// following vanilla's seeded-variant rule. `type: "event"`
    /// indirections resolve through their target, bounded at 4 hops. The
    /// winning layer supplies the object: resource packs first, then Mod
    /// JARs, then the vanilla object index.
    pub fn resolve(&self, event_name: &str, seed: i64) -> Option<ResolvedSound> {
        let mut name = normalize_sound_name(event_name);
        for _ in 0..4 {
            let def = self.events.get(name)?;
            let total: u64 = def
                .variants
                .iter()
                .map(|v| u64::from(v.weight.max(1)))
                .sum();
            if total == 0 {
                return None;
            }
            let mut pick = (seed as u64).rem_euclid(total);
            let mut chosen = &def.variants[0];
            for v in &def.variants {
                let w = u64::from(v.weight.max(1));
                if pick < w {
                    chosen = v;
                    break;
                }
                pick -= w;
            }
            if let Some(redirect) = &chosen.redirect {
                name = normalize_sound_name(redirect);
                continue;
            }
            let (source, path) = self.resolve_source(&chosen.object_key)?;
            return Some(ResolvedSound {
                path,
                source,
                volume: chosen.volume,
                pitch: chosen.pitch,
                stream: chosen.stream,
                attenuation_distance: chosen.attenuation_distance,
            });
        }
        None
    }

    /// Locate `object_key` (`"ns/sounds/path.ogg"`) across layers —
    /// resource packs first, then Mod JARs, then the vanilla object index.
    /// Fail-closed: a hostile `sounds.json` name containing `..`, leading
    /// slashes, or backslashes resolves to nothing rather than escaping
    /// the layer root.
    fn resolve_source(&self, object_key: &str) -> Option<(SoundSourceData, PathBuf)> {
        if object_key.is_empty()
            || object_key.contains('\0')
            || object_key.contains('\\')
            || object_key.starts_with('/')
            || object_key.split('/').any(|p| p == "..")
        {
            return None;
        }
        let entry_name = format!("assets/{object_key}");
        for layer in self.layers.iter().rev() {
            if let Some(dir) = &layer.dir {
                let candidate = dir.join(&entry_name);
                if candidate.is_file() {
                    return Some((SoundSourceData::File(candidate.clone()), candidate));
                }
            }
            if let Some(zip) = &layer.zip {
                if zip.entry_names().any(|n| n == entry_name) {
                    let archive_path = zip_archive_path(zip);
                    return Some((
                        SoundSourceData::ZipEntry {
                            archive_path: archive_path.clone(),
                            entry_name: entry_name.clone(),
                        },
                        PathBuf::from(format!("{}!{entry_name}", archive_path.display())),
                    ));
                }
            }
        }
        self.objects
            .get(object_key)
            .cloned()
            .map(|p| (SoundSourceData::File(p.clone()), p))
    }

    /// Decode a resolved source to 48 kHz [`DecodedPcm`] (mono or stereo),
    /// memoized under the bounded LRU replay cache.
    pub fn decode_source(&mut self, source: &SoundSourceData) -> Result<Arc<DecodedPcm>, String> {
        let cache_key = match source {
            SoundSourceData::File(p) => p.clone(),
            SoundSourceData::ZipEntry {
                archive_path,
                entry_name,
            } => PathBuf::from(format!("{}!{entry_name}", archive_path.display())),
        };
        self.lru_clock += 1;
        let tick = self.lru_clock;
        if let Some(entry) = self.decode_cache.get_mut(&cache_key) {
            entry.last_used = tick;
            self.pcm_cache_hits += 1;
            return Ok(entry.pcm.clone());
        }
        let pcm = match source {
            SoundSourceData::File(p) => decode_ogg(p, MAX_OGG_BYTES, MAX_DECODED_SAMPLES)?,
            SoundSourceData::ZipEntry {
                archive_path,
                entry_name,
            } => {
                let zip = self
                    .layers
                    .iter()
                    .filter_map(|l| l.zip.as_ref())
                    .find(|z| {
                        archive_path == &zip_archive_path(z)
                            && z.entry_names().any(|n| n == entry_name.as_str())
                    });
                let bytes = match zip {
                    Some(z) => z
                        .read_entry(entry_name, MAX_OGG_BYTES as usize)?
                        .ok_or_else(|| format!("zip entry {entry_name} vanished"))?,
                    None => {
                        // Source resolved before a later merge reordered
                        // layers — open the archive fresh rather than fail.
                        crate::zip_entries::ZipArchive::open(archive_path)?
                            .read_entry(entry_name, MAX_OGG_BYTES as usize)?
                            .ok_or_else(|| format!("zip entry {entry_name} vanished"))?
                    }
                };
                decode_ogg_mem(&bytes, MAX_DECODED_SAMPLES)?
            }
        };
        let pcm = match pcm {
            (DecodedPcm::Mono(v), rate) => {
                DecodedPcm::Mono(resample_if_needed(&v, rate))
            }
            (DecodedPcm::Stereo(v), rate) => DecodedPcm::Stereo(resample_stereo(&v, rate)),
        };
        let shared: Arc<DecodedPcm> = Arc::new(pcm);
        let bytes = shared.bytes();
        if bytes <= self.pcm_cache_cap_bytes {
            self.decode_cache.insert(
                cache_key,
                CachedPcm {
                    pcm: shared.clone(),
                    bytes,
                    last_used: tick,
                },
            );
            self.pcm_cache_bytes += bytes;
            self.evict_pcm_over_cap();
        }
        Ok(shared)
    }

    /// Decode an object-store OGG to 48 kHz mono `f32`, memoized by path
    /// under the bounded LRU replay cache. Cache hits refresh the entry's
    /// recency; decoded clips that would push the cache over
    /// [`SoundLibrary::pcm_cache_cap_bytes`] evict least-recently-used
    /// entries first, and a clip larger than the whole cap is returned
    /// uncached so resident PCM never exceeds the budget. Stereo assets
    /// are downmixed — channel-preserving callers use
    /// [`SoundLibrary::decode_source`].
    pub fn decode(&mut self, path: &Path) -> Result<Arc<Vec<f32>>, String> {
        let pcm = self.decode_source(&SoundSourceData::File(path.to_path_buf()))?;
        Ok(Arc::new(pcm.to_mono()))
    }
}

/// The path a stored [`SoundLayer::zip`] was opened from (used for cache
/// keys and `SoundSourceData::ZipEntry::archive_path`).
fn zip_archive_path(zip: &crate::zip_entries::ZipArchive) -> PathBuf {
    zip.path().to_path_buf()
}

/// Resample mono PCM to [`MIX_SAMPLE_RATE`] when needed.
fn resample_if_needed(pcm: &[f32], rate: u32) -> Vec<f32> {
    if rate == MIX_SAMPLE_RATE {
        pcm.to_vec()
    } else {
        resample_linear(pcm, rate, MIX_SAMPLE_RATE)
    }
}

/// Stereo-aware resample: each channel runs the same linear interpolator.
fn resample_stereo(pcm: &[[f32; 2]], rate: u32) -> Vec<[f32; 2]> {
    if rate == MIX_SAMPLE_RATE {
        return pcm.to_vec();
    }
    let l: Vec<f32> = pcm.iter().map(|f| f[0]).collect();
    let r: Vec<f32> = pcm.iter().map(|f| f[1]).collect();
    let l = resample_linear(&l, rate, MIX_SAMPLE_RATE);
    let r = resample_linear(&r, rate, MIX_SAMPLE_RATE);
    l.into_iter().zip(r).map(|(a, b)| [a, b]).collect()
}

/// Parse one `sounds.json` document into `(event, def)` pairs.
fn parse_sounds_json_bytes(
    raw: &[u8],
    origin: &Path,
) -> Result<Vec<(String, SoundEventDef)>, String> {
    let text = String::from_utf8(raw.to_vec())
        .map_err(|_| format!("{} is not UTF-8 JSON", origin.display()))?;
    let doc = parse_json(&text, MAX_ASSET_JSON_NODES)
        .map_err(|e| format!("{}: {e}", origin.display()))?;
    let JsonValue::Obj(top) = doc else {
        return Err(format!("{} is not a sounds.json object", origin.display()));
    };
    Ok(top
        .iter()
        .map(|(event, def)| (event.clone(), parse_sound_event_def(def)))
        .collect())
}

/// One parsed `sounds.json` event: `{sounds: [...], subtitle?, replace?}`.
fn parse_sound_event_def(value: &JsonValue) -> SoundEventDef {
    let mut def = SoundEventDef::default();
    if let Some(subtitle) = value.get("subtitle").and_then(JsonValue::as_str) {
        def.subtitle = Some(subtitle.to_string());
    }
    def.replace = matches!(value.get("replace"), Some(JsonValue::Bool(true)));
    let Some(JsonValue::Arr(sounds)) = value.get("sounds") else {
        return def;
    };
    for entry in sounds {
        // Bare-string shorthand: {"sounds": ["entity/levelup"]}.
        if let Some(name) = entry.as_str() {
            def.variants.push(SoundVariant {
                object_key: sound_object_key(name),
                volume: 1.0,
                pitch: 1.0,
                weight: 1,
                redirect: None,
                attenuation_distance: None,
                stream: false,
            });
            continue;
        }
        let Some(name) = entry.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        let is_event = entry
            .get("type")
            .and_then(JsonValue::as_str)
            .is_some_and(|t| t == "event");
        def.variants.push(SoundVariant {
            object_key: if is_event {
                String::new()
            } else {
                sound_object_key(name)
            },
            volume: entry
                .get("volume")
                .and_then(JsonValue::as_f64)
                .map(|v| v as f32)
                .unwrap_or(1.0),
            pitch: entry
                .get("pitch")
                .and_then(JsonValue::as_f64)
                .map(|v| v as f32)
                .unwrap_or(1.0),
            weight: entry
                .get("weight")
                .and_then(JsonValue::as_i64)
                .and_then(|w| u32::try_from(w).ok())
                .unwrap_or(1),
            redirect: if is_event {
                Some(normalize_sound_name(name).to_string())
            } else {
                None
            },
            attenuation_distance: entry
                .get("attenuation_distance")
                .and_then(JsonValue::as_f64)
                .map(|d| (d as f32).max(0.0))
                .filter(|d| d.is_finite() && *d > 0.0),
            stream: matches!(entry.get("stream"), Some(JsonValue::Bool(true))),
        });
    }
    def
}

/// `"entity/levelup"` → `"minecraft/sounds/entity/levelup.ogg"`;
/// `"other:path/to/snd"` → `"other/sounds/path/to/snd.ogg"`. Names that
/// already carry a file suffix keep it (custom packs ship `.ogg` spelled
/// out, and vanilla tolerates it).
fn sound_object_key(name: &str) -> String {
    let (ns, path) = match name.split_once(':') {
        Some((ns, path)) => (ns, path),
        None => ("minecraft", name),
    };
    if path.ends_with(".ogg") {
        format!("{ns}/sounds/{path}")
    } else {
        format!("{ns}/sounds/{path}.ogg")
    }
}

/// Object hashes are sha1 hex; anything else is refused so a hostile index
/// cannot traverse out of `objects/`.
fn is_object_hash(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if meta.len() > cap {
        return Err(format!("{} exceeds {cap}-byte cap", path.display()));
    }
    let f =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut out = Vec::with_capacity(meta.len() as usize);
    f.take(cap + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if out.len() as u64 > cap {
        return Err(format!("{} exceeds {cap}-byte cap", path.display()));
    }
    Ok(out)
}

/// Decode OGG bytes entirely in memory — the entry point for
/// ZIP/JAR-resident sounds; `cap_samples` bounds decoded frames.
/// Channel count is preserved (mono stays `Mono`, multi-channel keeps
/// `Stereo` L/R — extra channels fold into L/R symmetrically).
pub fn decode_ogg_mem(
    bytes: &[u8],
    cap_samples: usize,
) -> Result<(DecodedPcm, u32), String> {
    struct MemSource {
        inner: std::io::Cursor<Vec<u8>>,
        len: u64,
    }
    impl std::io::Read for MemSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }
    impl std::io::Seek for MemSource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }
    impl MediaSource for MemSource {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.len)
        }
    }
    let len = bytes.len() as u64;
    let src = MemSource {
        inner: std::io::Cursor::new(bytes.to_vec()),
        len,
    };
    decode_vorbis(Box::new(src), cap_samples, "memory")
}

/// Decode an OGG Vorbis file preserving channels at its native rate —
/// mono yields [`DecodedPcm::Mono`], multi-channel yields `Stereo` L/R.
/// `cap_bytes`/`cap_samples` bound hostile inputs.
pub fn decode_ogg(
    path: &Path,
    cap_bytes: u64,
    cap_samples: usize,
) -> Result<(DecodedPcm, u32), String> {
    let src = file_source(path, cap_bytes)?;
    decode_vorbis(src, cap_samples, &path.display().to_string())
}

/// Decode an OGG Vorbis file to mono `f32` at its native rate.
/// `cap_bytes`/`cap_samples` bound hostile inputs. All channels are
/// downmixed equally; symphonia already sanitizes non-finite output, and
/// the mixer bus limiter clamps whatever survives.
pub fn decode_ogg_mono(
    path: &Path,
    cap_bytes: u64,
    cap_samples: usize,
) -> Result<(Vec<f32>, u32), String> {
    let src = file_source(path, cap_bytes)?;
    let (pcm, rate) = decode_vorbis(src, cap_samples, &path.display().to_string())?;
    Ok((pcm.to_mono(), rate))
}

/// Bounded `File` as a symphonia `MediaSource`: reads never yield more
/// than `cap` absolute bytes, even across seeks (the OGG demuxer seeks to
/// the tail for duration).
fn file_source(path: &Path, cap_bytes: u64) -> Result<Box<dyn MediaSource>, String> {
    struct Capped {
        inner: std::fs::File,
        pos: u64,
        cap: u64,
    }
    impl std::io::Read for Capped {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let take = buf.len().min(self.cap.saturating_sub(self.pos) as usize);
            let n = self.inner.read(&mut buf[..take])?;
            self.pos += n as u64;
            Ok(n)
        }
    }
    impl std::io::Seek for Capped {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            let target = match pos {
                std::io::SeekFrom::Start(p) => p as i64,
                std::io::SeekFrom::End(d) => self.cap as i64 + d,
                std::io::SeekFrom::Current(d) => self.pos as i64 + d,
            };
            let target = target.clamp(0, self.cap as i64) as u64;
            self.pos = self.inner.seek(std::io::SeekFrom::Start(target))?;
            Ok(self.pos)
        }
    }
    impl MediaSource for Capped {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.cap)
        }
    }

    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let len = file
        .metadata()
        .map(|m| m.len().min(cap_bytes))
        .unwrap_or(cap_bytes);
    Ok(Box::new(Capped {
        inner: file,
        pos: 0,
        cap: len,
    }))
}

/// Channel-preserving symphonia decode shared by the file and memory
/// entry points. Mono/stereo assets keep their channel layout; assets
/// with more channels fold into symmetric L/R pairs so 2D playback stays
/// binaural and 3D downmixes correctly.
fn decode_vorbis(
    src: Box<dyn MediaSource>,
    cap_samples: usize,
    what: &str,
) -> Result<(DecodedPcm, u32), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let mss = MediaSourceStream::new(src, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("ogg");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("{what}: probe failed: {e}"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| format!("{what}: no audio track"))?;
    let rate = track.codec_params.sample_rate.unwrap_or(MIX_SAMPLE_RATE);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("{what}: decoder init failed: {e}"))?;
    let track_id = track.id;
    let mut mono: Vec<f32> = Vec::new();
    let mut stereo: Vec<[f32; 2]> = Vec::new();
    let mut stereo_mode = false;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(_)) => break,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(format!("{what}: demux failed: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("{what}: decode failed: {e}")),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let frames = decoded.frames();
        let mut buf = SampleBuffer::<f32>::new(frames as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        if channels == 1 {
            for &s in samples.iter().take(frames.min(samples.len())) {
                mono.push(s);
                if mono.len() >= cap_samples {
                    return Ok((DecodedPcm::Mono(mono), rate));
                }
            }
        } else {
            // Once any packet decodes multi-channel the asset is stereo —
            // earlier mono frames can never occur (channel count is fixed
            // per Vorbis stream), so `stereo` starts empty on first use.
            stereo_mode = true;
            let l_idx = 0usize;
            let r_idx = if channels == 2 { 1 } else { channels / 2 };
            for i in 0..frames.min(samples.len() / channels) {
                let l = samples[i * channels + l_idx];
                let mut r_acc = 0.0f32;
                for c in r_idx..channels {
                    r_acc += samples[i * channels + c];
                }
                stereo.push([l, r_acc / (channels - r_idx) as f32]);
                if stereo.len() >= cap_samples {
                    return Ok((DecodedPcm::Stereo(stereo), rate));
                }
            }
        }
    }
    if stereo_mode {
        Ok((DecodedPcm::Stereo(stereo), rate))
    } else {
        Ok((DecodedPcm::Mono(mono), rate))
    }
}

/// Linear-interpolation resampler — deterministic, cheap, and accurate
/// enough for game sounds (vanilla resamples with a similar-quality path).
pub fn resample_linear(pcm: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if pcm.is_empty() || src_rate == 0 || src_rate == dst_rate {
        return pcm.to_vec();
    }
    let ratio = dst_rate as f64 / src_rate as f64;
    let out_len = ((pcm.len() as f64) * ratio).ceil() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let i0 = src_pos.floor() as usize;
        let i1 = (i0 + 1).min(pcm.len() - 1);
        let frac = (src_pos - i0 as f64) as f32;
        let a = pcm[i0.min(pcm.len() - 1)];
        let b = pcm[i1];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Deterministic placeholder voice for events with no resolvable/decodable
/// asset: a short decaying sine whose fundamental is hashed from the event
/// name and shifted per category, so distinct events are audibly distinct
/// but always reproduce bit-for-bit. ~0.9 s at `sample_rate`.
pub fn placeholder_pcm(name: &str, category: i32, sample_rate: u32) -> Vec<f32> {
    let h = sound_name_hash(name);
    let base_hz = 180.0 + ((h >> 8) % 480) as f32;
    let freq = base_hz * (1.0 + 0.07 * (category.rem_euclid(10) as f32));
    let seconds = 0.9;
    let n = (seconds * sample_rate as f32) as usize;
    let mut out = Vec::with_capacity(n);
    let w = 2.0 * std::f32::consts::PI * freq / sample_rate as f32;
    let w2 = w * 2.02; // near-octave second partial keeps it from sounding pure-sine dead
    for i in 0..n {
        let t = i as f32;
        let env = (-3.5 * t / n as f32).exp();
        let s = (0.55 * (w * t).sin() + 0.18 * (w2 * t).sin()) * env;
        // 8 ms fade-in avoids a start click.
        let fade = (t / (0.008 * sample_rate as f32)).min(1.0);
        out.push(s * 0.5 * fade);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_assets(root: &Path) -> PathBuf {
        let objects = root.join("objects/ab");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::create_dir_all(root.join("indexes")).unwrap();
        let ogg_hash = "ab".repeat(20); // 40 hex chars
        let sounds_json_hash = "cd".repeat(20);
        std::fs::create_dir_all(root.join("objects/cd")).unwrap();
        std::fs::write(objects.join(&ogg_hash), b"OggS fake").unwrap();
        std::fs::write(
            root.join("objects/cd").join(&sounds_json_hash),
            r#"{"entity.player.levelup":{"subtitle":"subtitles.levelup","sounds":[{"name":"event/levelup","volume":0.8,"pitch":1.2,"weight":3},{"name":"event/levelup2","weight":1}]},"redirect.test":{"sounds":[{"name":"entity.player.levelup","type":"event"}]}}"#,
        )
        .unwrap();
        let index = format!(
            r#"{{"objects":{{"minecraft/sounds.json":{{"hash":"{sounds_json_hash}","size":10}},"minecraft/sounds/event/levelup.ogg":{{"hash":"{ogg_hash}","size":9}},"minecraft/sounds/event/levelup2.ogg":{{"hash":"{ogg_hash}","size":9}}}}}}"#
        );
        let idx = root.join("indexes/test.json");
        std::fs::write(&idx, index).unwrap();
        root.to_path_buf()
    }

    #[test]
    fn resolver_maps_event_to_object_and_seed_picks_variant() {
        let dir = std::env::temp_dir().join(format!("mc-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_assets(&dir);
        let mut lib = SoundLibrary::load(&dir, "test").unwrap();
        assert_eq!(lib.event_count(), 2);

        // seed in the first 3 weight slots picks variant 0 (vol 0.8, pitch 1.2).
        let r0 = lib.resolve("minecraft:entity.player.levelup", 1).unwrap();
        assert!(r0.path.ends_with("ab".repeat(20)));
        assert!((r0.volume - 0.8).abs() < 1e-6 && (r0.pitch - 1.2).abs() < 1e-6);
        // seed 3 lands in variant 1's weight slot (defaults vol/pitch 1.0).
        let r1 = lib.resolve("entity.player.levelup", 3).unwrap();
        assert!((r1.volume - 1.0).abs() < 1e-6 && (r1.pitch - 1.0).abs() < 1e-6);
        // type:event redirect follows through.
        assert!(lib.resolve("redirect.test", 0).is_some());
        assert!(lib.resolve("no.such.event", 0).is_none());

        // Decode failure on the fake OGG is an error, not a panic.
        assert!(lib.decode(&r0.path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostile_index_entries_are_refused() {
        let dir = std::env::temp_dir().join(format!("mc-assets-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("indexes")).unwrap();
        std::fs::write(
            dir.join("indexes/bad.json"),
            r#"{"objects":{"x/y.ogg":{"hash":"../etc/passwd0000000000000000000000000000"},"z":{"hash":7}}}"#,
        )
        .unwrap();
        let lib = SoundLibrary::load(&dir, "bad").unwrap();
        assert!(lib.objects.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholder_is_deterministic_and_bounded() {
        let a = placeholder_pcm("entity.player.levelup", 4, MIX_SAMPLE_RATE);
        let b = placeholder_pcm("entity.player.levelup", 4, MIX_SAMPLE_RATE);
        assert_eq!(a, b);
        assert_ne!(a, placeholder_pcm("block.stone.break", 4, MIX_SAMPLE_RATE));
        assert!(a.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        assert!(a.iter().any(|s| s.abs() > 0.01));
    }

    #[test]
    fn resample_stays_finite() {
        let src: Vec<f32> = (0..4410).map(|i| (i as f32 * 0.1).sin()).collect();
        let up = resample_linear(&src, 44_100, MIX_SAMPLE_RATE);
        assert!((up.len() as f64 - src.len() as f64 * 48000.0 / 44100.0).abs() <= 1.0);
        assert!(up.iter().all(|s| s.is_finite()));
        assert_eq!(
            resample_linear(&[], 44_100, MIX_SAMPLE_RATE),
            Vec::<f32>::new()
        );
        assert_eq!(resample_linear(&src, MIX_SAMPLE_RATE, MIX_SAMPLE_RATE), src);
    }

    #[test]
    fn registry_table_covers_what_the_server_emits() {
        // Every name the server's `sound_id()` table can emit resolves back.
        for (id, name) in [
            (1115, "entity.player.levelup"),
            (8, "ambient.cave"),
            (629, "block.grass.break"),
            (84, "entity.arrow.hit_player"),
        ] {
            assert_eq!(sound_name_for_id(id), Some(name));
        }
        assert!(sound_name_for_id(999_999).is_none());
        let _ = std::io::stdout().flush();
    }
}
