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
}

/// A `sounds.json` event definition.
#[derive(Clone, Debug, Default)]
pub struct SoundEventDef {
    /// Variants in declaration order.
    pub variants: Vec<SoundVariant>,
    /// `subtitle` translation key, when declared (e.g.
    /// `subtitles.entity.player.levelup`).
    pub subtitle: Option<String>,
}

/// A resolved variant ready to decode.
#[derive(Clone, Debug)]
pub struct ResolvedSound {
    /// Object-store path of the `.ogg`.
    pub path: PathBuf,
    /// Combined volume multiplier (variant-level).
    pub volume: f32,
    /// Combined pitch multiplier (variant-level).
    pub pitch: f32,
}

/// The parsed asset index + `sounds.json` for one `--assetsDir`/
/// `--assetIndex` pair, with a decoded-PCM cache so replayed sounds decode
/// once.
pub struct SoundLibrary {
    objects: HashMap<String, PathBuf>,
    events: HashMap<String, SoundEventDef>,
    decode_cache: HashMap<PathBuf, Arc<Vec<f32>>>,
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
            decode_cache: HashMap::new(),
        })
    }

    /// Number of event definitions loaded from `sounds.json`.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Resolve `event_name` (`"entity.player.levelup"` or namespaced) to an
    /// on-disk OGG plus its variant multipliers. `seed` (the packet's sound
    /// seed) picks the variant by cumulative weight, following vanilla's
    /// seeded-variant rule. `type: "event"` indirections resolve through
    /// their target, bounded at 4 hops.
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
            let path = self.objects.get(&chosen.object_key)?.clone();
            return Some(ResolvedSound {
                path,
                volume: chosen.volume,
                pitch: chosen.pitch,
            });
        }
        None
    }

    /// Decode an object-store OGG to 48 kHz mono `f32`, memoized by path.
    pub fn decode(&mut self, path: &Path) -> Result<Arc<Vec<f32>>, String> {
        if let Some(hit) = self.decode_cache.get(path) {
            return Ok(hit.clone());
        }
        let (pcm, rate) = decode_ogg_mono(path, MAX_OGG_BYTES, MAX_DECODED_SAMPLES)?;
        let pcm = if rate == MIX_SAMPLE_RATE {
            pcm
        } else {
            resample_linear(&pcm, rate, MIX_SAMPLE_RATE)
        };
        let shared: Arc<Vec<f32>> = Arc::new(pcm);
        self.decode_cache.insert(path.to_path_buf(), shared.clone());
        Ok(shared)
    }
}

/// One parsed `sounds.json` event: `{sounds: [...], subtitle?, replace?}`.
fn parse_sound_event_def(value: &JsonValue) -> SoundEventDef {
    let mut def = SoundEventDef::default();
    if let Some(subtitle) = value.get("subtitle").and_then(JsonValue::as_str) {
        def.subtitle = Some(subtitle.to_string());
    }
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

/// Decode an OGG Vorbis file to mono `f32` at its native rate.
/// `cap_bytes`/`cap_samples` bound hostile inputs. All channels are
/// downmixed equally; symphonia already sanitizes non-finite output, and
/// the mixer bus limiter clamps whatever survives.
pub fn decode_ogg_mono(
    path: &Path,
    cap_bytes: u64,
    cap_samples: usize,
) -> Result<(Vec<f32>, u32), String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSource, MediaSourceStream};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    // Read/Seek facade that never yields more than `cap` absolute bytes,
    // even across seeks (the OGG demuxer seeks to the tail for duration).
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
    let src = Capped {
        inner: file,
        pos: 0,
        cap: len,
    };
    let mss = MediaSourceStream::new(Box::new(src), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("ogg");
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("{}: probe failed: {e}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| format!("{}: no audio track", path.display()))?;
    let rate = track.codec_params.sample_rate.unwrap_or(MIX_SAMPLE_RATE);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("{}: decoder init failed: {e}", path.display()))?;
    let track_id = track.id;
    let mut out: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(_)) => break,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(format!("{}: demux failed: {e}", path.display())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => return Err(format!("{}: decode failed: {e}", path.display())),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let frames = decoded.frames();
        let mut buf = SampleBuffer::<f32>::new(frames as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();
        let inv = 1.0 / channels as f32;
        for i in 0..frames.min(samples.len() / channels) {
            let mut acc = 0.0f32;
            for c in 0..channels {
                acc += samples[i * channels + c];
            }
            out.push(acc * inv);
            if out.len() >= cap_samples {
                return Ok((out, rate));
            }
        }
    }
    Ok((out, rate))
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
