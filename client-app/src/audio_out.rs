// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Audio output stage: [`AudioSink`] backends (PipeWire/ALSA subprocess pipe,
//! null, WAV file), the bounded PCM queue that feeds a stalled sink without
//! unbounded growth, and [`AudioRuntime`] — the live pipeline that turns
//! decoded `sound_*` packets into spatialized, limited stereo PCM.
//!
//! The runtime lives inside [`crate::protocol::ProtocolClient`]: it is fed
//! by the play-packet decode arms and pumped from the same call sites the
//! session loop already drives, so it needs no separate audio thread for
//! mixing. Wall-clock reads exist only in [`AudioRuntime::pump`], which
//! paces how many samples are *due*; the mixer itself
//! ([`crate::session::SoundMixer`]) is deterministic for fixed inputs.

use std::collections::VecDeque;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::protocol::{WireSoundEvent, WireStopSound};
use crate::session::{AudioStats, OpenAirWorld, PlaySoundEvent, SoundMixer};
use crate::sound_assets::{placeholder_pcm, sound_name_for_id, SoundLibrary, MIX_SAMPLE_RATE};
use crate::spatial_audio::{ListenerPose, SpatialAudioPreset};

/// PCM format every sink consumes: signed 16-bit stereo interleaved at
/// [`MIX_SAMPLE_RATE`].
pub const SINK_BYTES_PER_SEC: usize = MIX_SAMPLE_RATE as usize * 2 * 2;
/// Queue ceiling fed to a subprocess sink: two seconds of PCM. When the
/// child stalls longer than this, the oldest queued chunks are dropped so
/// latency and memory stay bounded.
const SINK_QUEUE_BYTES: usize = SINK_BYTES_PER_SEC * 2;
/// How long `finish` lets the writer drain before the child is killed.
const SINK_DRAIN_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Max backlog `pump` renders in one call — a quarter second. Longer stalls
/// are skipped, not caught up, so the mix stays near real time.
const MAX_PUMP_FRAMES: usize = MIX_SAMPLE_RATE as usize / 4;
/// Pending wire events retained for non-audio consumers.
const MAX_PENDING_WIRE_EVENTS: usize = 256;

// ---------------------------------------------------------------------------
// Sink trait + backends
// ---------------------------------------------------------------------------

/// Where the mixed stereo PCM goes.
pub trait AudioSink: Send {
    /// Push one block of interleaved s16 stereo samples.
    fn push(&mut self, pcm: &[i16]) -> Result<(), String>;
    /// Short backend label for diagnostics (`"pipewire"`, `"wav"`, ...).
    fn label(&self) -> &'static str;
    /// Finalize output (patch WAV headers, close the child stdin and reap).
    /// Idempotent; also invoked from `Drop`.
    fn finish(&mut self) {}
}

/// Selected backend from `--audio-sink`/`--audio-wav`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSinkKind {
    /// `pw-cat` if present, else `aplay`, else `null` (with a warning).
    Auto,
    /// Force `pw-cat`; falls back to `null` with a warning when it cannot
    /// start or dies.
    PipeWire,
    /// Force `aplay`; same null fallback.
    Alsa,
    /// Discard samples (still counts them) — the fail-closed default.
    Null,
    /// Stream a 48 kHz s16le stereo WAV to the given path (`--audio-wav`).
    Wav(PathBuf),
    /// No runtime at all — audio disabled.
    Off,
}

/// Sink that drops everything but keeps a sample counter for diagnostics.
#[derive(Default)]
pub struct NullSink {
    /// Total interleaved samples pushed.
    pub samples_written: u64,
}

impl AudioSink for NullSink {
    fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
        self.samples_written += pcm.len() as u64;
        Ok(())
    }
    fn label(&self) -> &'static str {
        "null"
    }
}

/// Streaming 48 kHz s16le stereo RIFF WAV. The header is written up front
/// with zero sizes and patched on [`AudioSink::finish`]/`Drop`.
pub struct WavSink {
    path: PathBuf,
    file: BufWriter<std::fs::File>,
    data_bytes: u64,
    finished: bool,
}

impl WavSink {
    fn create(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
        }
        let file = std::fs::File::create(path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let mut sink = Self {
            path: path.to_path_buf(),
            file: BufWriter::new(file),
            data_bytes: 0,
            finished: false,
        };
        sink.write_header()?;
        Ok(sink)
    }

    fn write_header(&mut self) -> Result<(), String> {
        let h = &mut [0u8; 44];
        h[0..4].copy_from_slice(b"RIFF");
        // h[4..8] total size — patched on finish.
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
        h[22..24].copy_from_slice(&2u16.to_le_bytes()); // stereo
        h[24..28].copy_from_slice(&MIX_SAMPLE_RATE.to_le_bytes());
        h[28..32].copy_from_slice(&(MIX_SAMPLE_RATE * 4).to_le_bytes());
        h[32..34].copy_from_slice(&4u16.to_le_bytes()); // block align
        h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits
        h[36..40].copy_from_slice(b"data");
        // h[40..44] data size — patched on finish.
        self.file
            .write_all(h)
            .map_err(|e| format!("{}: header write failed: {e}", self.path.display()))
    }
}

impl AudioSink for WavSink {
    fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
        for s in pcm {
            self.file
                .write_all(&s.to_le_bytes())
                .map_err(|e| format!("{}: write failed: {e}", self.path.display()))?;
        }
        self.data_bytes += (pcm.len() * 2) as u64;
        Ok(())
    }

    fn label(&self) -> &'static str {
        "wav"
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let _ = self.file.flush();
        let f = self.file.get_mut();
        let data = (self.data_bytes.min(u64::from(u32::MAX))) as u32;
        let total = 36u64
            .saturating_add(self.data_bytes)
            .min(u64::from(u32::MAX)) as u32;
        let _ = f.seek(SeekFrom::Start(4));
        let _ = f.write_all(&total.to_le_bytes());
        let _ = f.seek(SeekFrom::Start(40));
        let _ = f.write_all(&data.to_le_bytes());
        let _ = f.flush();
    }
}

impl Drop for WavSink {
    fn drop(&mut self) {
        self.finish();
    }
}

// ---------------------------------------------------------------------------
// Subprocess sink (pw-cat / aplay) + bounded queue
// ---------------------------------------------------------------------------

/// Bounded byte queue between the mixer thread and the sink writer thread.
/// Full ⇒ oldest chunks are dropped (real-time beats backpressure);
/// `closed` ⇒ the writer exits once the queue empties.
struct PcmQueue {
    inner: Mutex<PcmQueueInner>,
    not_empty: Condvar,
}

struct PcmQueueInner {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    cap_bytes: usize,
    closed: bool,
    dropped_chunks: u64,
}

impl PcmQueue {
    fn new(cap_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(PcmQueueInner {
                chunks: VecDeque::new(),
                bytes: 0,
                cap_bytes,
                closed: false,
                dropped_chunks: 0,
            }),
            not_empty: Condvar::new(),
        }
    }

    /// Enqueue `chunk`, dropping the oldest queued chunks first when over
    /// capacity. Chunks larger than the whole queue are dropped outright.
    fn push(&self, chunk: Vec<u8>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if chunk.len() > g.cap_bytes {
            g.dropped_chunks += 1;
            return;
        }
        while g.bytes + chunk.len() > g.cap_bytes {
            let Some(old) = g.chunks.pop_front() else {
                break;
            };
            g.bytes = g.bytes.saturating_sub(old.len());
            g.dropped_chunks += 1;
        }
        g.bytes += chunk.len();
        g.chunks.push_back(chunk);
        drop(g);
        self.not_empty.notify_one();
    }

    /// Oldest chunk, or `None` once closed and drained. Waits up to `wait`
    /// for new data so the writer can notice shutdown promptly.
    fn pop(&self, wait: Duration) -> Option<Vec<u8>> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + wait;
        loop {
            if let Some(chunk) = g.chunks.pop_front() {
                g.bytes = g.bytes.saturating_sub(chunk.len());
                return Some(chunk);
            }
            if g.closed {
                return None;
            }
            let rem = deadline.checked_duration_since(Instant::now())?;
            let (guard, _timeout) = self
                .not_empty
                .wait_timeout(g, rem)
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
        }
    }

    fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.closed = true;
        drop(g);
        self.not_empty.notify_all();
    }

    fn dropped_chunks(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped_chunks
    }
}

/// `pw-cat`/`aplay` subprocess sink. Mixed PCM is queued on a bounded buffer
/// and drained into the child's stdin by a writer thread; a dead or stalled
/// child fails the sink closed (the runtime swaps in [`NullSink`]).
pub struct SubprocessSink {
    prog: &'static str,
    label: &'static str,
    child: Option<Child>,
    queue: Arc<PcmQueue>,
    writer_done: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    finished: bool,
}

impl SubprocessSink {
    fn spawn(prog: &'static str, args: &[&str], label: &'static str) -> Result<Self, String> {
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start {prog}: {e}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("cannot open stdin for {prog}"))?;
        let queue = Arc::new(PcmQueue::new(SINK_QUEUE_BYTES));
        let writer_done = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let writer = {
            let queue = queue.clone();
            let writer_done = writer_done.clone();
            let dead = dead.clone();
            std::thread::Builder::new()
                .name("mc-spine-audio-sink".to_string())
                .spawn(move || {
                    let _mark_done = writer_done;
                    while let Some(chunk) = queue.pop(Duration::from_millis(100)) {
                        if stdin.write_all(&chunk).is_err() {
                            dead.store(true, Ordering::Relaxed);
                            _mark_done.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                    // `drop(stdin)` closes the pipe so the child sees EOF.
                    _mark_done.store(true, Ordering::Relaxed);
                })
                .map_err(|e| format!("cannot spawn audio writer thread: {e}"))?
        };
        Ok(Self {
            prog,
            label,
            child: Some(child),
            queue,
            writer_done,
            dead,
            writer: Some(writer),
            finished: false,
        })
    }

    /// Queued chunks the writer had to drop because the child fell behind.
    pub fn dropped_chunks(&self) -> u64 {
        self.queue.dropped_chunks()
    }
}

impl AudioSink for SubprocessSink {
    fn push(&mut self, pcm: &[i16]) -> Result<(), String> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(format!("{} playback process died", self.prog));
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.dead.store(true, Ordering::Relaxed);
                    return Err(format!("{} exited while streaming", self.prog));
                }
                Ok(None) => {}
                Err(e) => {
                    self.dead.store(true, Ordering::Relaxed);
                    return Err(format!("cannot poll {}: {e}", self.prog));
                }
            }
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.queue.push(bytes);
        Ok(())
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Close the queue: the writer drains what remains then closes stdin.
        self.queue.close();
        let deadline = Instant::now() + SINK_DRAIN_TIMEOUT;
        while !self.writer_done.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(mut child) = self.child.take() {
            if !self.writer_done.load(Ordering::Relaxed) {
                // Writer is stuck in a blocking write on a stalled child —
                // kill the child, which errors the writer out promptly.
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Drop for SubprocessSink {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Open a sink for `kind`, returning the sink plus an optional warning.
/// `Auto` tries `pw-cat` then `aplay` before falling back to `null`.
pub fn open_sink(kind: &AudioSinkKind) -> (Box<dyn AudioSink>, Option<String>) {
    const PW_CAT_ARGS: [&str; 8] = [
        "--playback",
        "--raw",
        "--rate",
        "48000",
        "--channels",
        "2",
        "--format",
        "s16",
    ];
    const APLAY_ARGS: [&str; 10] = [
        "-q", "-f", "S16_LE", "-r", "48000", "-c", "2", "-t", "raw", "-",
    ];
    const PW_TARGET: [&str; 1] = ["-"];
    match kind {
        AudioSinkKind::Off | AudioSinkKind::Null => (Box::<NullSink>::default(), None),
        AudioSinkKind::Wav(path) => match WavSink::create(path) {
            Ok(sink) => (Box::new(sink), None),
            Err(e) => (
                Box::<NullSink>::default(),
                Some(format!("{e}; falling back to null sink")),
            ),
        },
        AudioSinkKind::PipeWire | AudioSinkKind::Alsa | AudioSinkKind::Auto => {
            let try_one = |prog: &'static str,
                           args: &'static [&'static str],
                           label: &'static str|
             -> Option<Box<dyn AudioSink>> {
                let mut argv: Vec<&str> = args.to_vec();
                if prog == "pw-cat" {
                    argv.extend_from_slice(&PW_TARGET);
                }
                SubprocessSink::spawn(prog, &argv, label)
                    .ok()
                    .map(|s| Box::new(s) as Box<dyn AudioSink>)
            };
            match kind {
                AudioSinkKind::Auto => {
                    if let Some(s) = try_one("pw-cat", &PW_CAT_ARGS, "pipewire") {
                        return (s, None);
                    }
                    if let Some(s) = try_one("aplay", &APLAY_ARGS, "alsa") {
                        return (s, None);
                    }
                    (
                        Box::<NullSink>::default(),
                        Some("no pw-cat or aplay on PATH; audio sink is null".to_string()),
                    )
                }
                AudioSinkKind::PipeWire => match try_one("pw-cat", &PW_CAT_ARGS, "pipewire") {
                    Some(s) => (s, None),
                    None => (
                        Box::<NullSink>::default(),
                        Some("pw-cat could not start; audio sink is null".to_string()),
                    ),
                },
                AudioSinkKind::Alsa => match try_one("aplay", &APLAY_ARGS, "alsa") {
                    Some(s) => (s, None),
                    None => (
                        Box::<NullSink>::default(),
                        Some("aplay could not start; audio sink is null".to_string()),
                    ),
                },
                _ => unreachable!(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session-level audio configuration (installed once by main)
// ---------------------------------------------------------------------------

/// Everything the live audio pipeline needs, resolved once from CLI flags +
/// client settings.
#[derive(Clone, Debug)]
pub struct AudioSessionConfig {
    /// Selected output backend.
    pub sink: AudioSinkKind,
    /// `--assetsDir` for the vanilla object store (`None` → placeholder
    /// tones only).
    pub assets_dir: Option<PathBuf>,
    /// `--assetIndex` id (`None` → placeholder tones only).
    pub asset_index: Option<String>,
    /// `qol.max_volume_cap` — the bus limiter ceiling.
    pub max_volume_cap: f32,
    /// `qol.max_concurrent_same_sound` — admission coalescing budget.
    pub max_concurrent_same_sound: u8,
    /// Optional sidecar file receiving one subtitle cue per line
    /// (`<time_s>\t<dir>\t<loudness>\t<distance>\t<name>`). Defaults to
    /// `<wav>.subtitles.txt` when the sink is a WAV file.
    pub subtitle_path: Option<PathBuf>,
    /// `true` (default) to also echo subtitle cues to stderr — the headless
    /// client's event stream.
    pub subtitles_to_stderr: bool,
}

static SESSION_AUDIO_CONFIG: Mutex<Option<AudioSessionConfig>> = Mutex::new(None);

/// Install the session audio configuration (called once by `main` before a
/// session runs). [`crate::protocol::ProtocolClient`] picks it up when a
/// connection is established.
pub fn install_session_audio_config(cfg: AudioSessionConfig) {
    *SESSION_AUDIO_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(cfg);
}

/// Remove the installed configuration (tests; a fresh install always
/// overwrites, so this is only needed to prove the no-audio path).
pub fn clear_session_audio_config() {
    *SESSION_AUDIO_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Snapshot of the installed configuration.
pub fn session_audio_config() -> Option<AudioSessionConfig> {
    SESSION_AUDIO_CONFIG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

// ---------------------------------------------------------------------------
// AudioRuntime — the live pipeline inside ProtocolClient
// ---------------------------------------------------------------------------

/// The live audio pipeline: wire events in, spatialized/limited PCM out.
/// Created once per connection when `AudioSinkKind::Off` is not selected.
pub struct AudioRuntime {
    /// The deterministic mixer (event queue → voices → limited stereo).
    pub mixer: SoundMixer,
    sink: Box<dyn AudioSink>,
    library: Option<SoundLibrary>,
    library_tried: bool,
    assets_dir: Option<PathBuf>,
    asset_index: Option<String>,
    subtitle_file: Option<BufWriter<std::fs::File>>,
    subtitles_to_stderr: bool,
    listener_yaw: f32,
    listener_pitch: f32,
    last_pump: Instant,
    carry_frames: f32,
    scratch_f32: Vec<f32>,
    scratch_i16: Vec<i16>,
    warned_null: bool,
    finished: bool,
}

impl AudioRuntime {
    /// Build the runtime: open the sink, construct the mixer (open-air
    /// acoustic world — the wire layer cannot see the client's section
    /// cache, so occlusion/reverb trace open sky until a world view is
    /// attached via [`SoundMixer::set_world`]), and defer the sound library
    /// load until the first event (asset indexes can be large).
    pub fn new(cfg: &AudioSessionConfig) -> Self {
        let (sink, warning) = open_sink(&cfg.sink);
        if let Some(w) = warning {
            eprintln!("mc-spine-client audio: {w}");
        }
        let label = sink.label();
        let mut mixer = SoundMixer::new(MIX_SAMPLE_RATE, SpatialAudioPreset::Realistic);
        mixer.set_world(Box::new(OpenAirWorld));
        mixer.limiter.set_max_volume_cap(cfg.max_volume_cap);
        mixer
            .limiter
            .set_max_concurrent_same_sound(cfg.max_concurrent_same_sound);

        let subtitle_path = match &cfg.subtitle_path {
            Some(p) => Some(p.clone()),
            None => match &cfg.sink {
                AudioSinkKind::Wav(p) => {
                    let mut name = p.as_os_str().to_os_string();
                    name.push(".subtitles.txt");
                    Some(PathBuf::from(name))
                }
                _ => None,
            },
        };
        let subtitle_file = subtitle_path.and_then(|p| match std::fs::File::create(&p) {
            Ok(f) => Some(BufWriter::new(f)),
            Err(e) => {
                eprintln!("mc-spine-client audio: cannot write {}: {e}", p.display());
                None
            }
        });
        if cfg.sink != AudioSinkKind::Off {
            eprintln!("mc-spine-client audio: sink={label} (48 kHz s16 stereo)");
        }
        Self {
            mixer,
            sink,
            library: None,
            library_tried: false,
            assets_dir: cfg.assets_dir.clone(),
            asset_index: cfg.asset_index.clone(),
            subtitle_file,
            subtitles_to_stderr: cfg.subtitles_to_stderr,
            listener_yaw: 0.0,
            listener_pitch: 0.0,
            last_pump: Instant::now(),
            carry_frames: 0.0,
            scratch_f32: Vec::new(),
            scratch_i16: Vec::new(),
            warned_null: false,
            finished: false,
        }
    }

    /// `true` when a runtime should exist for this config.
    pub fn enabled(cfg: &AudioSessionConfig) -> bool {
        cfg.sink != AudioSinkKind::Off
    }

    /// Lazily load the sound library on first use so a session that never
    /// hears a sound never pays the index parse.
    fn library(&mut self) -> Option<&mut SoundLibrary> {
        if self.library.is_none() && !self.library_tried {
            self.library_tried = true;
            if let (Some(dir), Some(index)) = (&self.assets_dir, &self.asset_index) {
                match SoundLibrary::load(dir, index) {
                    Ok(lib) => {
                        eprintln!(
                            "mc-spine-client audio: {} sound events from {}",
                            lib.event_count(),
                            dir.display()
                        );
                        self.library = Some(lib);
                    }
                    Err(e) => eprintln!(
                        "mc-spine-client audio: sound library unavailable ({e}); \
                         placeholder tones will play"
                    ),
                }
            }
        }
        self.library.as_mut()
    }

    /// Ingest one decoded `sound_effect`/`sound_entity` packet.
    pub fn ingest(&mut self, ev: &WireSoundEvent) {
        let (name, label) = match (&ev.name, ev.registry_id) {
            (Some(n), _) => (n.clone(), n.clone()),
            (None, Some(id)) => match sound_name_for_id(id) {
                Some(n) => (n.to_string(), n.to_string()),
                None => {
                    self.mixer.stats.unknown_sound_id += 1;
                    (format!("sound.{id}"), format!("sound.{id}"))
                }
            },
            (None, None) => return,
        };

        // Asset resolution: real OGG when the index + sounds.json map this
        // event, else a deterministic placeholder tone counted as such.
        let mut pcm = None;
        let mut is_placeholder = false;
        let mut variant_volume = 1.0f32;
        let mut variant_pitch = 1.0f32;
        if let Some(lib) = self.library() {
            match lib.resolve(&name, ev.seed) {
                Some(resolved) => {
                    variant_volume = resolved.volume;
                    variant_pitch = resolved.pitch;
                    match lib.decode(&resolved.path) {
                        Ok(p) => pcm = Some(p),
                        Err(e) => {
                            self.mixer.stats.decode_errors += 1;
                            if self.mixer.stats.decode_errors <= 3 {
                                eprintln!("mc-spine-client audio: {e}");
                            }
                        }
                    }
                }
                None => self.mixer.stats.missing_event_def += 1,
            }
        } else {
            self.mixer.stats.missing_assets += 1;
        }
        let pcm = match pcm {
            Some(p) => p,
            None => {
                is_placeholder = true;
                self.mixer.stats.placeholder_voices += 1;
                Arc::new(placeholder_pcm(
                    &name,
                    ev.category,
                    self.mixer.sample_rate(),
                ))
            }
        };
        if ev.entity_id.is_some() {
            self.mixer.stats.entity_pos_unresolved += 1;
        }
        self.mixer.submit(PlaySoundEvent {
            label,
            pcm,
            is_placeholder,
            category: ev.category,
            position: ev.position,
            entity_id: ev.entity_id,
            volume: (ev.volume * variant_volume).clamp(0.0, 1.0),
            pitch: (ev.pitch * variant_pitch).clamp(0.5, 2.0),
            seed: ev.seed,
        });
    }

    /// Ingest one decoded `stop_sound` packet.
    pub fn ingest_stop(&mut self, s: &WireStopSound) {
        self.mixer.submit_stop(s.category, s.name.as_deref());
    }

    /// Listener pose update (feet position + optional look angles — `None`
    /// keeps the previously seen angle, since `move_player_pos` carries
    /// none). Called from the `send_player_position*` /
    /// `send_accept_teleportation` paths the session runner drives.
    pub fn update_listener(&mut self, pos: [f64; 3], yaw: Option<f32>, pitch: Option<f32>) {
        if let Some(y) = yaw {
            self.listener_yaw = y;
        }
        if let Some(p) = pitch {
            self.listener_pitch = p;
        }
        self.mixer.set_listener(ListenerPose {
            position: [
                pos[0] as f32,
                (pos[1] + 1.62) as f32, // feet -> eye height
                pos[2] as f32,
            ],
            yaw_deg: self.listener_yaw,
            pitch_deg: self.listener_pitch,
            velocity: [0.0; 3],
            submerged: false,
        });
    }

    /// Wall-clock pacing: render however many frames elapsed since the last
    /// call (bounded at [`MAX_PUMP_FRAMES`]) and push them to the sink.
    /// Only `pump` reads the clock; [`Self::pump_frames`] is the
    /// deterministic entry point tests use.
    pub fn pump(&mut self) {
        let due = self.last_pump.elapsed().as_secs_f64() * f64::from(MIX_SAMPLE_RATE)
            + f64::from(self.carry_frames);
        let frames = (due.floor() as usize).min(MAX_PUMP_FRAMES);
        self.carry_frames = (due - frames as f64) as f32;
        self.last_pump = Instant::now();
        self.pump_frames(frames);
    }

    /// Render exactly `frames` stereo frames and push them to the sink.
    pub fn pump_frames(&mut self, frames: usize) {
        if frames == 0 {
            self.drain_subtitles();
            return;
        }
        self.scratch_f32.clear();
        self.scratch_f32.resize(frames * 2, 0.0);
        self.mixer.render(&mut self.scratch_f32);
        self.scratch_i16.clear();
        self.scratch_i16.reserve(frames * 2);
        for &s in &self.scratch_f32 {
            self.scratch_i16
                .push((s.clamp(-1.0, 1.0) * 32_767.0).round() as i16);
        }
        if self.sink.push(&self.scratch_i16).is_err() {
            if !self.warned_null {
                self.warned_null = true;
                eprintln!(
                    "mc-spine-client audio: {} sink failed; continuing on null",
                    self.sink.label()
                );
            }
            self.sink.finish();
            self.sink = Box::<NullSink>::default();
            let _ = self.sink.push(&self.scratch_i16);
        }
        self.drain_subtitles();
    }

    /// Emit pending subtitle cues to stderr and the sidecar file.
    fn drain_subtitles(&mut self) {
        while let Some(sub) = self.mixer.drain_subtitle() {
            let dir = match sub.cue.direction {
                crate::spatial_audio::SubtitleDirection::Left => "<",
                crate::spatial_audio::SubtitleDirection::Center => "=",
                crate::spatial_audio::SubtitleDirection::Right => ">",
                crate::spatial_audio::SubtitleDirection::Behind => "v",
            };
            let line = format!(
                "{:.3}\t{}\t{:.2}\t{:.1}\t{}",
                sub.at_sample as f64 / f64::from(MIX_SAMPLE_RATE),
                dir,
                sub.cue.loudness,
                sub.cue.distance_m,
                sub.name
            );
            if self.subtitles_to_stderr {
                eprintln!(
                    "mc-spine-client subtitle: {dir} {} ({:.1}m)",
                    sub.name, sub.cue.distance_m
                );
            }
            if let Some(f) = &mut self.subtitle_file {
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }
        if let Some(f) = &mut self.subtitle_file {
            let _ = f.flush();
        }
    }

    /// The active sink backend label.
    pub fn sink_label(&self) -> &'static str {
        self.sink.label()
    }

    /// Mixer counters (events seen, voices, limiter verdicts, placeholders).
    pub fn stats(&self) -> &AudioStats {
        &self.mixer.stats
    }

    /// End-of-session summary line for the headless event stream.
    pub fn report_line(&self) -> String {
        let s = self.stats();
        format!(
            "sink={} events={} voices={} stolen={} suppressed={} placeholder={} \
             unknown_id={} missing_event={} decode_err={} subtitles={}",
            self.sink.label(),
            s.events_submitted,
            s.voices_started,
            s.voices_stolen,
            s.events_suppressed,
            s.placeholder_voices,
            s.unknown_sound_id,
            s.missing_event_def,
            s.decode_errors,
            s.subtitle_cues,
        )
    }

    /// Flush the sink and print the report line. Called on drop too.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.sink.finish();
        eprintln!("mc-spine-client audio done: {}", self.report_line());
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Bound for the protocol client's retained wire-event queue.
pub fn pending_wire_event_bound() -> usize {
    MAX_PENDING_WIRE_EVENTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_sink_writes_patchable_header_and_samples() {
        let path = std::env::temp_dir().join(format!("mc-wav-{}.wav", std::process::id()));
        {
            let mut sink = WavSink::create(&path).unwrap();
            sink.push(&[0, 1000, -1000, 32767, -32768, 0]).unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 12);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            MIX_SAMPLE_RATE
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pcm_queue_drops_oldest_when_full_and_stops_after_close() {
        let q = PcmQueue::new(8);
        q.push(vec![1, 2, 3, 4]);
        q.push(vec![5, 6, 7, 8]);
        // Over capacity: oldest chunk dropped, newer stays queued.
        q.push(vec![9, 9]);
        assert!(q.dropped_chunks() >= 1);
        assert_eq!(q.pop(Duration::from_millis(0)), Some(vec![5, 6, 7, 8]));
        assert_eq!(q.pop(Duration::from_millis(0)), Some(vec![9, 9]));
        // Oversized chunks are rejected wholesale.
        q.push(vec![0; 9]);
        let g = q.inner.lock().unwrap_or_else(|e| e.into_inner());
        assert!(g.chunks.is_empty());
        drop(g);
        q.close();
        assert_eq!(q.pop(Duration::from_millis(5)), None);
    }

    #[test]
    fn open_sink_off_and_null_are_silent_success() {
        let (sink, warn) = open_sink(&AudioSinkKind::Off);
        assert!(warn.is_none());
        assert_eq!(sink.label(), "null");
        let (sink, warn) = open_sink(&AudioSinkKind::Null);
        assert!(warn.is_none());
        assert_eq!(sink.label(), "null");
    }
}
